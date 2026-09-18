//! §17 的"弹性"那一行：资源压力下的行为。
//!
//! 那一行的原文是：
//!
//! > 人为降低可用内存、GPU OOM、磁盘忙时，**停止后台扩容**并保持**取消/审批可响应**；
//! > 记录峰值私有提交及工作集，**不只看平均值**。
//!
//! 前半句有落点了：规划器（§10.1、`core-topology`）判可行性，调度器照它的结论停。
//! 而在这一项之前，那台规划器**没有任何调用方**——36 处引用全在它自己和自己的测试里。
//!
//! 后半句（峰值）还没有。写在这里，免得读的人以为整行都做完了。

use soca_contracts::{
    ActionLevel, Approval, ApprovalId, CapabilityPolicyRef, ExplorationQuota, GoalBudget,
    ModelBackend, ModelBudget, ModelReservation, ModelVersion, PermissionScope, PlanState,
    PlannerPolicy, ResourceEnvelope, SelectionPolicy, Sha256Hex, SubjectId, UserChannel, WallClock,
    LEAF_PROFILES,
};
use soca_core::{ActionBroker, Resources, ScheduleOutcome, Scheduler, SimulatedOs, Subject};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const CAP: &str = "cap:read-selected-folder";
const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn owner() -> SubjectId {
    SubjectId::new("user:local").expect("固定主体")
}

fn subject() -> Subject {
    let mut os = SimulatedOs::new();
    os.seed(WATCHED, "资料摘要\n- 下一次评审：2026-10-15\n");
    let cluster = DesktopAndFilesCluster::new(WATCHED, Vec::new()).expect("装配能力簇");
    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(os),
        cluster,
        owner(),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000c3").expect("固定 boot"),
        Box::new(DeterministicTransport::new(Vec::new())),
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    )
    .expect("装配主体")
}

fn delegate(subject: &mut Subject) -> soca_contracts::GoalId {
    let goal_id = subject
        .delegate(
            "整理摘要",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: CapabilityPolicyRef::new(CAP).expect("固定能力策略"),
                max_action_level: ActionLevel::A1,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");
    goal_id
}

/// 按给定的包络算一份拓扑计划，再读成资源状况。
///
/// 走的是**产品那条路**：包络 → `plan_topology` → `Resources`。直接手搓一个
/// `Resources::Paused` 也能过测试，但那证明不了规划器被接上了——而"规划器没人调"
/// 正是这一项要修的那件事。
fn resources_for(ram_limit_mib: u64, gpu_mib: u64, model: &ModelReservation) -> Resources {
    let envelope = ResourceEnvelope {
        ram_limit_mib,
        cpu_slots: 4,
        gpu_allocatable_mib: gpu_mib,
        telemetry_age_seconds: 0.0,
    };
    let plan = soca_topology_plan(&envelope, model);
    assert_eq!(
        plan.state,
        PlanState::Paused,
        "这份包络本该承载不了最小档位：{plan:?}"
    );
    Resources::from_plan(&plan)
}

fn soca_topology_plan(
    envelope: &ResourceEnvelope,
    model: &ModelReservation,
) -> soca_contracts::TopologyPlan {
    soca_core_topology::plan_topology(
        envelope,
        model,
        u64::from(LEAF_PROFILES[0]),
        &PlannerPolicy::default(),
    )
    .expect("规划")
}

// ---------------------------------------------------------------------------
// 停了
// ---------------------------------------------------------------------------

#[test]
fn a_lowered_memory_ceiling_stops_the_background_loop_before_any_round_runs() {
    // §17 的原话是"**人为**降低可用内存……时，停止后台扩容"。停的位置很要紧：
    // **一轮都没跑**，而不是跑一轮再发现。跑完一轮再检查，那一轮要的资源已经要过了。
    let mut subject = subject();
    delegate(&mut subject);

    // 先确认这条包络确实跑得起来——不然下面的"停了"分不出是压力还是别的原因。
    let healthy = Scheduler::default()
        .run(
            &mut subject,
            &Resources::Running,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(2),
        )
        .expect("跑一段");
    assert!(
        !matches!(healthy.outcome, ScheduleOutcome::ResourcePressure { .. }),
        "健康包络下不该报资源压力：{:?}",
        healthy.outcome
    );

    // 现在把上限降到连控制预算都不够。
    let starved = resources_for(64, 0, &ModelReservation::default());
    let report = Scheduler::default()
        .run(
            &mut subject,
            &starved,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(10),
        )
        .expect("跑一段");

    match &report.outcome {
        ScheduleOutcome::ResourcePressure {
            reason,
            emergency,
            rounds,
        } => {
            assert_eq!(*rounds, 0, "一轮都不该跑");
            assert!(!emergency, "这是计划上的暂停，不是一次压力事件");
            assert!(
                reason.contains("CONTROL_RESERVE") || reason.contains("MINIMUM_TOPOLOGY"),
                "理由要来自规划器：{reason}"
            );
        }
        other => panic!("该停下：{other:?}"),
    }
    assert!(report.rounds.is_empty(), "逐轮记录也该是空的");
}

#[test]
fn a_gpu_that_cannot_hold_the_model_stops_the_loop() {
    // "GPU OOM"是 §17 点名的第二种压力。它与"内存不够"走的是规划器里**不同**的一条分支，
    // 所以理由不一样——而理由不一样意味着操作员要做的事不一样。
    let mut subject = subject();
    delegate(&mut subject);

    // `vram_mib` 只有 GPU 后端可以非零（契约层会拒掉别的组合），所以这一条得先把后端
    // 也说对——不然它测的会是"输入不合法"，而不是"显存不够"。
    let model = ModelReservation {
        backend: ModelBackend::Gpu,
        vram_mib: 8192,
        ..ModelReservation::default()
    };
    // 包络给得出控制预算与最小档位，但 VRAM 给不出模型要的。
    let starved = resources_for(4096, 1024, &model);

    match starved {
        Resources::Paused { reason } => assert!(
            reason.contains("MODEL_VRAM"),
            "该说清是显存不够：{reason}"
        ),
        other => panic!("该报成计划暂停：{other:?}"),
    }
}

#[test]
fn an_emergency_is_reported_as_an_event_not_as_a_plan_pause() {
    // §5.3："不能等 10 秒伸缩滞后才处理实际 OOM。"所以这一档与"计划暂停"都是停，
    // 但**报出来必须不一样**：前者是"这台机器现在承载不了这个档位"，后者是"刚刚出事了"。
    // 合成一件事，操作员会去等一个永远不会自己好的东西。
    let mut subject = subject();
    delegate(&mut subject);

    let emergency = Resources::Emergency {
        reason: "磁盘忙".to_string(),
    };
    let report = Scheduler::default()
        .run(
            &mut subject,
            &emergency,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(2),
        )
        .expect("跑一段");

    match &report.outcome {
        ScheduleOutcome::ResourcePressure {
            reason, emergency, ..
        } => {
            assert!(*emergency, "要能分出这是一次事件");
            assert_eq!(reason, "磁盘忙");
        }
        other => panic!("该停下：{other:?}"),
    }
}

#[test]
fn an_unknown_envelope_runs_and_that_default_is_deliberate() {
    // 调用方没给包络时按"可以跑"处理，方向与 §12.2 的失败关闭相反，所以钉一条测试。
    //
    // 权限的默认必须朝拒绝，因为放行一次不该放行的动作不可逆；而调度器的默认朝放行，
    // 因为"没给包络"是**配置缺失**，不是一种压力。让缺配置表现为"永远不跑"的话，
    // 一个忘了接线的新调用点第一次运行时会看起来像挂了——那种故障最难归因。
    let mut subject = subject();
    delegate(&mut subject);

    let report = Scheduler::default()
        .run(
            &mut subject,
            &Resources::Unknown,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(2),
        )
        .expect("跑一段");
    assert!(
        !matches!(report.outcome, ScheduleOutcome::ResourcePressure { .. }),
        "{:?}",
        report.outcome
    );
    assert!(Resources::Unknown.is_runable());
}

// ---------------------------------------------------------------------------
// "保持取消/审批可响应"
// ---------------------------------------------------------------------------

#[test]
fn pause_approval_and_cancel_still_work_while_the_machine_is_out_of_resources() {
    // §17 那半句："停止后台扩容并**保持取消/审批可响应**"。
    //
    // 这一条测的不是"它们没被挡住"——它们本来就不走调度器。它测的是**那条界线**：
    // 资源压力停的是**后台的认知工作**，不是用户对这个系统的控制权。
    // §14 说"LLM 不可用时仍能关闭设备、撤销授权、查看动作账和退出程序"，
    // 那是同一个原则：**控制通道不排在资源队列后面。**
    let mut subject = subject();
    let goal_id = delegate(&mut subject);

    let emergency = Resources::Emergency {
        reason: "内存耗尽".to_string(),
    };
    Scheduler::default()
        .run(
            &mut subject,
            &emergency,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(2),
        )
        .expect("跑一段");

    // 一、暂停仍然能按。
    subject.policy_mut().pause("压力太大，先停");
    assert!(
        subject.policy().pause_reason().is_some(),
        "资源压力不该挡住用户按暂停"
    );

    // 二、批准仍然能记。
    let approval = Approval::new(
        ApprovalId::new("approval:pressure").expect("固定审批"),
        owner(),
        ActionLevel::A1,
        UserChannel::ApprovalUi,
        at(3),
        None,
        1,
    )
    .expect("合法批准")
    .for_parameters(Sha256Hex::of_bytes("内容".as_bytes()));
    subject.grant_approval(&approval, at(3)).expect("记下批准");

    // 三、取消（放弃目标）仍然能做。
    let abandoned = subject.abandon(&goal_id, at(4)).expect("放弃");
    assert!(abandoned >= 1, "该把那个目标放掉");

    // 四、看状态仍然能看——§17 说"记录峰值私有提交及工作集"，那要先看得见。
    let state = subject.public_state(at(5)).expect("状态");
    assert!(state.paused.is_some(), "暂停的理由要能看到");
}
