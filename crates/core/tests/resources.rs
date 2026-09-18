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
    ActionLevel, Approval, ApprovalId, CapabilityPolicyRef, DataClass, ExplorationQuota, GoalBudget,
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
// 峰值（§17 的后半句）
// ---------------------------------------------------------------------------

/// 观测一次、跑一轮，直到记下一条结论。返回那条记忆的标识。
fn record_one(subject: &mut Subject, offset: i64) -> soca_contracts::MemoryId {
    subject
        .observe(WATCHED, DataClass::Personal, at(offset))
        .expect("观测");
    subject
        .run_round(
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(offset + 1),
        )
        .expect("跑一轮");
    subject
        .store()
        .recall(&owner(), None, at(offset + 2))
        .expect("召回")
        .last()
        .map(|entry| entry.memory_id.clone())
        .expect("这一轮应当记下一条结论")
}

#[test]
fn a_peak_survives_the_value_falling_back() {
    // 这一条就是 §17 那半句的落点。
    //
    // 峰值**不能**在事后从采样里算——那要求把所有采样都留着，而"留所有采样"正是长跑里
    // 最先撑不住的东西。所以它必须在**值变化的那一刻**记下来。而"值掉回去了、峰值还在"
    // 正是"事后算"做不到的那件事：事后只看得到现在是多少。
    let mut subject = subject();
    delegate(&mut subject);
    let first = record_one(&mut subject, 2);
    let second = record_one(&mut subject, 5);

    let peak = subject
        .resource_ledger()
        .metric("memories")
        .expect("跑过轮就该有计量")
        .peak;
    assert!(peak >= 2, "两次结论该把峰值推到 2：{peak}");

    // 把两条都删掉、再清理掉。当前值会掉回去。
    subject.forget(&first, at(8)).expect("删第一条");
    subject.forget(&second, at(9)).expect("删第二条");
    subject
        .enforce_retention(&soca_core::RetentionPolicy::default(), at(10))
        .expect("清理");

    let metric = subject
        .resource_ledger()
        .metric("memories")
        .expect("有计量");
    assert_eq!(metric.current, 0, "当前值该掉到 0");
    assert_eq!(
        metric.peak, peak,
        "而峰值要留在原处——它记的是'被撑到过哪里'，不是'现在是多少'"
    );
    assert!(
        metric.peak_at.is_some(),
        "峰值出现的时刻要记着：'第 3 分钟还是第 11 小时'决定了它是启动抖动还是泄漏"
    );
    assert!(
        subject.resource_ledger().headroom("memories") >= 2,
        "两者之差就是'退回去多少'"
    );
}

#[test]
fn a_new_window_starts_from_the_current_value_not_from_zero() {
    // §17 要"先 8 小时后 24 小时试运行"——两段。第一段的峰值不该污染第二段。
    //
    // 而把峰值压到 **0** 会让新窗口的峰值比实际低，因为"当时已经是 2 了"这件事被抹掉了。
    // 一个偏低的峰值比没有峰值更糟：它看起来是个答案。
    let mut subject = subject();
    delegate(&mut subject);
    let kept = record_one(&mut subject, 2);

    subject.reset_resource_peaks(at(6)).expect("开新窗口");
    let metric = subject
        .resource_ledger()
        .metric("memories")
        .expect("有计量");
    assert_eq!(metric.current, 1, "那条结论还在");
    assert_eq!(metric.peak, 1, "新窗口的峰值起点是当前值，不是 0");

    // 而新窗口确实重新开始积累。
    let _other = record_one(&mut subject, 7);
    assert!(
        subject
            .resource_ledger()
            .metric("memories")
            .expect("有计量")
            .peak
            >= 2,
        "新窗口照样往上记"
    );
    let _ = kept;
}

#[test]
fn every_round_samples_so_the_peak_does_not_depend_on_who_is_looking() {
    // 采样发生在**动作之后**，而不是"读状态的时候现算"。
    //
    // 差别是实的：现算的话，记下的峰值只反映"有人来看过的那几个时刻"，
    // 而把一台机器压垮的那一下通常发生在没人看的时候。这条测试**一次也不读**
    // `public_state`，然后断言峰值已经涨上去了。
    let mut subject = subject();
    delegate(&mut subject);
    assert!(
        subject.resource_ledger().metric("events").is_none(),
        "还没干过活，账上应当是空的"
    );

    let _ = record_one(&mut subject, 2);

    let events = subject
        .resource_ledger()
        .metric("events")
        .expect("跑过轮就该采过样");
    assert!(events.peak >= 1, "观测写了一条事件：{events:?}");
    assert!(
        subject.resource_ledger().total_peak() > 0,
        "总峰值要能报出来"
    );
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
