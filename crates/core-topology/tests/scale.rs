//! 拓扑规划与伸缩滞后回归测试（工单 ENG-03）。
//!
//! 前 13 项是 `design/soca_reference/test_topology.py` 的等价移植，函数名刻意保持一致，
//! 便于逐条对照。期望值全部写死，不用本实现自己算出来再对照自己。
//!
//! 第 14 项直接核对实施规格 §5.2 的五档计划表；第 15 项核对计划 JSON 的字段集合与
//! 参考规划器 CLI 输出一致。

use soca_contracts::{
    ModelBackend, ModelReservation, PlanReason, PlanState, PlannerPolicy, ResourceEnvelope,
    ScaleAction, ScaleReason, TopologyError, CLUSTERS_PER_COORDINATOR, LEAVES_PER_CLUSTER,
    LEAF_PROFILES, MAX_CLUSTERS_BEFORE_COORDINATOR, ROOT_MAX_CHILDREN,
};
use soca_core_topology::{candidate_plan, plan_json, plan_topology, ScaleGate};

fn envelope(ram_limit_mib: u64, cpu_slots: u32) -> ResourceEnvelope {
    ResourceEnvelope::new(ram_limit_mib, cpu_slots)
}

fn gpu_envelope(ram_limit_mib: u64, cpu_slots: u32, gpu_allocatable_mib: u64) -> ResourceEnvelope {
    ResourceEnvelope {
        gpu_allocatable_mib,
        ..ResourceEnvelope::new(ram_limit_mib, cpu_slots)
    }
}

fn stale_envelope(ram_limit_mib: u64, cpu_slots: u32, age: f64) -> ResourceEnvelope {
    ResourceEnvelope {
        telemetry_age_seconds: age,
        ..ResourceEnvelope::new(ram_limit_mib, cpu_slots)
    }
}

// ---------------------------------------------------------------------------
// 参考实现等价移植
// ---------------------------------------------------------------------------

#[test]
fn same_demand_scales_all_topology_levels_with_hardware() {
    let model = ModelReservation::default();
    let settings = [
        (2048_u64, 2_u32, 16_u32),
        (4096, 4, 64),
        (8192, 8, 128),
        (16384, 16, 256),
        (32768, 32, 512),
    ];

    for (memory, cpu, expected) in settings {
        let plan = plan_topology(&envelope(memory, cpu), &model, 512, &PlannerPolicy::default())
            .expect("合法输入");
        assert_eq!(plan.leaves, expected, "{memory} MiB / {cpu} 槽");
        assert_eq!(plan.clusters, expected / 8);
        assert_eq!(
            plan.coordinators,
            if expected > 128 { expected / 64 } else { 0 }
        );
        assert!(plan.ram_required_mib <= memory);
        assert!(plan.workers <= cpu);
        assert_eq!(plan.subjects, 1, "主体身份不随硬件伸缩");
    }
}

#[test]
fn exact_fit_is_admitted_but_one_less_mib_is_not() {
    let model = ModelReservation::default();
    let policy = PlannerPolicy::default();
    let required = candidate_plan(8, &envelope(65536, 32), &model, &policy)
        .expect("合法档位")
        .ram_required_mib;
    assert_eq!(required, 1829, "参考成本模型的可复算结果");

    let admitted = plan_topology(&envelope(required, 1), &model, 8, &policy).expect("合法输入");
    assert_eq!(admitted.state, PlanState::Running);

    let refused =
        plan_topology(&envelope(required - 1, 1), &model, 8, &policy).expect("合法输入");
    assert_eq!(refused.state, PlanState::Paused);
}

#[test]
fn no_task_demand_does_not_fill_available_hardware() {
    let plan = plan_topology(
        &envelope(65536, 64),
        &ModelReservation::default(),
        0,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");

    assert_eq!(
        (plan.leaves, plan.clusters, plan.subjects, plan.logical_units()),
        (8, 1, 1, 10),
        "空闲硬件不生成无任务角色"
    );
}

#[test]
fn cpu_bottleneck_limits_topology_even_with_abundant_ram() {
    let plan = plan_topology(
        &envelope(65536, 1),
        &ModelReservation::default(),
        512,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");

    assert_eq!(plan.leaves, 16, "RAM 再大也不能突破 CPU 槽位");
    assert_eq!(plan.workers, 1);
}

#[test]
fn selected_gpu_model_cannot_silently_fall_back_to_cloud() {
    let model = ModelReservation {
        vram_mib: 8192,
        backend: ModelBackend::Gpu,
        ..ModelReservation::default()
    };
    let plan = plan_topology(&gpu_envelope(32768, 16, 4096), &model, 128, &PlannerPolicy::default())
        .expect("合法输入");

    assert_eq!(
        (plan.state, plan.reason),
        (PlanState::Paused, PlanReason::ModelVramUnavailable)
    );
}

#[test]
fn unauthorized_cloud_and_stale_telemetry_pause() {
    let remote = ModelReservation {
        backend: ModelBackend::Remote,
        remote_authorized: false,
        ..ModelReservation::default()
    };
    assert_eq!(
        plan_topology(&envelope(8192, 8), &remote, 64, &PlannerPolicy::default())
            .expect("合法输入")
            .reason,
        PlanReason::RemoteNotAuthorized
    );

    let stale = stale_envelope(8192, 8, 4.0);
    assert_eq!(
        plan_topology(
            &stale,
            &ModelReservation::default(),
            64,
            &PlannerPolicy::default()
        )
        .expect("合法输入")
        .reason,
        PlanReason::StaleTelemetry
    );
}

#[test]
fn no_resources_never_creates_workers_or_discards_identity() {
    let plan = plan_topology(
        &envelope(0, 0),
        &ModelReservation::default(),
        64,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");

    assert_eq!((plan.subjects, plan.workers, plan.leaves), (1, 0, 0));
    assert_eq!(plan.reason, PlanReason::ControlReserveUnavailable);
    assert!(plan.ram_required_mib > plan.ram_limit_mib);
}

#[test]
fn parent_fanout_and_totals_are_consistent() {
    let model = ModelReservation::default();
    let policy = PlannerPolicy::default();

    for leaves in LEAF_PROFILES {
        let plan = candidate_plan(leaves, &envelope(65536, 64), &model, &policy)
            .expect("合法档位");

        let leaves_per_cluster = leaves
            .checked_div(plan.clusters)
            .expect("运行中的计划必须有能力簇");
        assert!(leaves_per_cluster <= LEAVES_PER_CLUSTER, "每簇最多 8 叶");

        if plan.coordinators > 0 {
            let clusters_per_coordinator = plan
                .clusters
                .checked_div(plan.coordinators)
                .expect("有协调层就必定有簇");
            assert!(clusters_per_coordinator <= CLUSTERS_PER_COORDINATOR);
            assert!(plan.coordinators <= ROOT_MAX_CHILDREN);
        } else {
            assert!(plan.clusters <= MAX_CLUSTERS_BEFORE_COORDINATOR);
        }
        assert_eq!(
            plan.logical_units(),
            plan.leaves + plan.clusters + plan.coordinators + 1
        );
    }
}

#[test]
fn model_calls_and_manual_cap_are_independent_from_leaf_count() {
    let model = ModelReservation {
        max_parallel_calls: 1,
        ..ModelReservation::default()
    };
    let policy = PlannerPolicy {
        max_leaves: 64,
        ..PlannerPolicy::default()
    };
    let plan = plan_topology(&envelope(65536, 64), &model, 512, &policy).expect("合法输入");

    assert_eq!((plan.leaves, plan.llm_parallel_calls), (64, 1));
}

#[test]
fn expansion_waits_and_changes_at_most_one_profile() {
    let mut gate = ScaleGate::default();
    let desired = plan_topology(
        &envelope(65536, 64),
        &ModelReservation::default(),
        256,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");

    assert_eq!(
        gate.observe(32, &desired, 0.0, false).expect("合法").action,
        ScaleAction::Keep
    );
    assert_eq!(
        gate.observe(32, &desired, 59.0, false).expect("合法").action,
        ScaleAction::Keep
    );

    let decision = gate.observe(32, &desired, 60.0, false).expect("合法");
    assert_eq!(
        (decision.action, decision.target_leaves),
        (ScaleAction::ProposeTransaction, 64),
        "一次最多上升一档"
    );

    gate.acknowledge(61.0).expect("合法确认");
    gate.observe(64, &desired, 62.0, false).expect("合法");
    assert_eq!(
        gate.observe(64, &desired, 122.0, false).expect("合法").action,
        ScaleAction::Keep,
        "冷却期内不得再次扩容"
    );
    assert_eq!(
        gate.observe(64, &desired, 182.0, false)
            .expect("合法")
            .target_leaves,
        128
    );
}

#[test]
fn sustained_reduction_can_skip_profiles_but_emergency_is_immediate() {
    let mut gate = ScaleGate::default();
    let desired = plan_topology(
        &envelope(2048, 2),
        &ModelReservation::default(),
        512,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");
    assert_eq!(desired.leaves, 16);

    gate.observe(256, &desired, 0.0, false).expect("合法");
    assert_eq!(
        gate.observe(256, &desired, 10.0, false)
            .expect("合法")
            .target_leaves,
        16,
        "缩容可以跨档"
    );

    let emergency = gate.observe(256, &desired, 11.0, true).expect("合法");
    assert_eq!(emergency.action, ScaleAction::FreezeAndReconcile);
    assert_eq!(emergency.reason, ScaleReason::PressureEmergency);
}

#[test]
fn alternating_targets_reset_hold_time() {
    let mut gate = ScaleGate::default();
    let low = plan_topology(
        &envelope(4096, 4),
        &ModelReservation::default(),
        64,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");
    let high = plan_topology(
        &envelope(8192, 8),
        &ModelReservation::default(),
        128,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");

    for (now, desired) in [(0.0, &high), (30.0, &low), (60.0, &high), (90.0, &low)] {
        assert_eq!(
            gate.observe(32, desired, now, false).expect("合法").action,
            ScaleAction::Keep,
            "目标来回波动必须重置等待时间（now={now}）"
        );
    }
}

#[test]
fn invalid_inputs_are_rejected() {
    // 参考实现里"资源量为负"的检查在 Rust 由类型承担：u64/u32 无法表示负数。
    // 仍然需要运行时校验的是遥测年龄、模型保留与策略组合。
    let negative_age = stale_envelope(4096, 4, -1.0);
    assert!(matches!(
        plan_topology(
            &negative_age,
            &ModelReservation::default(),
            64,
            &PlannerPolicy::default()
        ),
        Err(TopologyError::InvalidTelemetryAge { .. })
    ));

    let illegal_vram = ModelReservation {
        backend: ModelBackend::Cpu,
        vram_mib: 1,
        ..ModelReservation::default()
    };
    assert!(matches!(
        plan_topology(&envelope(4096, 4), &illegal_vram, 64, &PlannerPolicy::default()),
        Err(TopologyError::InvalidModelReservation { .. })
    ));

    let no_workers = PlannerPolicy {
        max_workers: 0,
        ..PlannerPolicy::default()
    };
    assert!(matches!(
        plan_topology(
            &envelope(4096, 4),
            &ModelReservation::default(),
            64,
            &no_workers
        ),
        Err(TopologyError::InvalidPolicy { .. })
    ));

    let mut gate = ScaleGate::default();
    let desired = plan_topology(
        &envelope(4096, 4),
        &ModelReservation::default(),
        64,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");
    gate.observe(32, &desired, 2.0, false).expect("合法");
    assert!(matches!(
        gate.observe(32, &desired, 1.0, false),
        Err(TopologyError::NonMonotonicTime { .. })
    ));

    // 非档位的当前规模必须被拒绝，而不是被当成一个不存在的档位继续算。
    assert!(matches!(
        gate.observe(48, &desired, 3.0, false),
        Err(TopologyError::InvalidCurrentTopology { actual: 48 })
    ));
}

// ---------------------------------------------------------------------------
// 规格 §5.2 的五档计划表
// ---------------------------------------------------------------------------

#[test]
fn the_documented_five_tier_table_reproduces() {
    // (RAM MiB, CPU 槽, R0, R1, R1b, R2, Total, Required MiB)
    let table = [
        (2048_u64, 2_u32, 16_u32, 2_u32, 0_u32, 1_u32, 19_u32, 1863_u64),
        (4096, 4, 64, 8, 0, 1, 73, 2838),
        (8192, 8, 128, 16, 0, 1, 145, 4138),
        (16384, 16, 256, 32, 4, 1, 293, 6746),
        (32768, 32, 512, 64, 8, 1, 585, 11954),
    ];

    for (ram, cpu, r0, r1, r1b, r2, total, required) in table {
        let plan = plan_topology(
            &envelope(ram, cpu),
            &ModelReservation::default(),
            512,
            &PlannerPolicy::default(),
        )
        .expect("合法输入");

        assert_eq!(plan.leaves, r0, "{ram} MiB 的 R0");
        assert_eq!(plan.clusters, r1, "{ram} MiB 的 R1");
        assert_eq!(plan.coordinators, r1b, "{ram} MiB 的 R1b");
        assert_eq!(plan.subjects, r2, "{ram} MiB 的 R2");
        assert_eq!(plan.logical_units(), total, "{ram} MiB 的总槽数");
        assert_eq!(plan.ram_required_mib, required, "{ram} MiB 的所需 RAM");
        assert_eq!(
            plan.composition_depth(),
            if r1b > 0 { 4 } else { 3 },
            "{ram} MiB 的组合深度"
        );
        assert!(plan.is_running());
    }
}

#[test]
fn plan_json_matches_the_reference_shape() {
    let plan = plan_topology(
        &envelope(8192, 8),
        &ModelReservation::default(),
        512,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");
    let json = plan_json(&plan).expect("合法计划必须可序列化");
    let object = json.as_object().expect("计划必须是 JSON 对象");

    let mut actual: Vec<&str> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();
    let expected = [
        "clusters",
        "composition_depth",
        "coordinators",
        "hot_leaves",
        "leaves",
        "llm_parallel_calls",
        "logical_units",
        "ram_limit_mib",
        "ram_required_mib",
        "reason",
        "reference_simulation_only",
        "state",
        "subjects",
        "workers",
    ];
    assert_eq!(actual, expected);

    assert_eq!(json["state"], serde_json::json!("RUNNING"));
    assert_eq!(json["reason"], serde_json::json!("ADMITTED"));
    assert_eq!(json["logical_units"], serde_json::json!(145));
    assert_eq!(json["composition_depth"], serde_json::json!(3));
    assert_eq!(
        json["reference_simulation_only"],
        serde_json::json!(true),
        "输出只由输入的模拟包络算出，未读取真实硬件"
    );
}

#[test]
fn a_paused_plan_reports_zero_depth_but_keeps_the_subject() {
    let plan = plan_topology(
        &envelope(0, 0),
        &ModelReservation::default(),
        64,
        &PlannerPolicy::default(),
    )
    .expect("合法输入");

    assert_eq!(plan.composition_depth(), 0, "未运行时没有组合深度");
    assert_eq!(plan.subjects, 1, "资源不足不是删除主体的理由（§3.1 R2）");
    assert_eq!(plan.state, PlanState::Paused);
}

#[test]
fn coordinator_layer_appears_only_above_the_fanout_limit() {
    let model = ModelReservation::default();
    let policy = PlannerPolicy::default();

    for (leaves, expected_coordinators) in
        [(128_u32, 0_u32), (256, 4), (512, 8)]
    {
        let plan = candidate_plan(leaves, &envelope(65536, 64), &model, &policy)
            .expect("合法档位");
        assert_eq!(
            plan.coordinators, expected_coordinators,
            "{leaves} 叶的中间协调层"
        );
    }
    // 每个中间协调器最多 8 簇，这正是它出现的条件（§3.2）。
    assert_eq!(CLUSTERS_PER_COORDINATOR, 8);
    assert_eq!(MAX_CLUSTERS_BEFORE_COORDINATOR, 16);
    assert_eq!(ROOT_MAX_CHILDREN, 16);
}
