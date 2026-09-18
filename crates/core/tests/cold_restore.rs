//! §17 的"冷恢复"那一行，以及它顺带接上的 §9.2。
//!
//! §17 那一行是：
//!
//! > 目标 NVMe 机器上 **64 KiB 轻量单元状态**的 p95 恢复 ≤ 200 ms；**模型冷加载单独计时且
//! > 可取消，不混入此指标**。
//!
//! 而那一节的开头还有一句，读这一行时同样要紧：
//!
//! > 以下数字是**首轮建议门槛，不是已经达到的成绩**。
//!
//! 所以本文件测的是**可测量**，不是"让它达标"：恢复耗时被单独计量出来、和状态字节数并排
//! 报出，而**被拒的唤醒不计时**（它不是"恢复花了很久"，是"根本没有恢复"——把它算进去会让
//! p95 被一堆瞬间失败拉低）。
//!
//! ## 为什么 §9.2 要一起接
//!
//! `UnitRegistry` 有 `wake`／`checkpoint`／未决动作的闸门，有一整套测试，而**主体从来没碰过
//! 它**。这一行要的"恢复一份 64 KiB 的单元状态"正是它做的事——所以把这一行做出来，
//! §9.2 那条线也就接通了。

use soca_contracts::{
    ActionLevel, CapabilityPolicyRef, CognitiveUnit, DataClass, ExplorationQuota, GoalBudget,
    ModelBackend, ModelBudget, ModelVersion, PermissionScope, SelectionPolicy, SubjectId,
    UserChannel, UnitState, WallClock,
};
use soca_core::{ActionBroker, SimulatedOs, Subject, WakeOutcome};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;
use tempfile::TempDir;

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
    subject_on(Store::open_in_memory(at(0)).expect("内存存储"))
}

/// 在给定的存储上装一个主体。
///
/// 跨重启那一条测试要的是**同一份库、新的进程**——所以存储必须能从外面给进来。
/// 内存库做不到这件事：它的生命周期就是那个连接的。
fn subject_on(store: Store) -> Subject {
    let mut os = SimulatedOs::new();
    os.seed(WATCHED, "资料摘要\n- 下一次评审：2026-10-15\n");
    let cluster = DesktopAndFilesCluster::new(WATCHED, Vec::new()).expect("装配能力簇");
    Subject::new(
        store,
        ActionBroker::new(os),
        cluster,
        owner(),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000d4").expect("固定 boot"),
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

fn delegate(subject: &mut Subject) {
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
}

/// 一份已经登记过、已经醒着、并且干过活的单元。
///
/// 顺序不能省：**登记是冷的，要先唤醒才能干活，也才能降温**。`sleep` 对一份没在热表里的
/// 单元报 `UnitNotHot`——那一档的存在理由是"降温的对象必须是正在跑的东西"，
/// 而不是"随便什么都能标成冷的"。
fn registered_subject() -> Subject {
    let mut subject = subject();
    subject.register_unit(at(1)).expect("登记单元");
    subject.wake(at(1)).expect("唤醒");
    delegate(&mut subject);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");
    subject
}

#[test]
fn a_unit_sleeps_and_wakes_and_the_restore_is_measured() {
    // 这是那一行的主路径：降温 → 唤醒 → **恢复耗时被单独记下来**。
    let mut subject = registered_subject();
    assert!(subject.is_awake(), "醒着才干得了活");

    let slept = subject.sleep(at(5)).expect("降温");
    assert_eq!(
        subject.store().unit(subject.cluster().unit_id()).expect("读").map(|s| s.state),
        Some(UnitState::Cold),
        "降温之后库里要是冷的"
    );
    assert!(
        slept.handed_over.is_empty(),
        "这一轮没有投递过动作，所以没有被移交的：{:?}",
        slept.handed_over
    );
    assert!(!subject.is_awake());

    let woken = subject.wake(at(6)).expect("唤醒");
    assert!(matches!(woken, WakeOutcome::Ready { .. }), "{woken:?}");
    assert!(subject.is_awake());

    let ledger = subject.resource_ledger();
    let restore = ledger
        .metric("unit_restore_ms")
        .expect("恢复耗时要被单独记一笔");
    assert!(restore.peak >= restore.current, "峰值不该低于当前值");
    // §17 的门槛是 200 ms，而那是"建议门槛，不是已经达到的成绩"——所以这里不断言它的值。
    // 断言的是**它存在、而且是单独的一项**：混进别的计时，这个数就答非所问。
    assert!(restore.peak < 60_000, "一次内存库的恢复不该要一分钟");
}

#[test]
fn the_size_of_the_state_being_restored_is_reported_too() {
    // §17 的门槛是"**64 KiB** 轻量单元状态"——所以"那份状态有多大"必须是个能读出来的数。
    // 少了它，"≤ 200 ms 恢复"没有分母。
    let mut subject = registered_subject();
    subject.sleep(at(5)).expect("降温");
    subject.wake(at(6)).expect("唤醒");

    let bytes = subject
        .resource_ledger()
        .metric("unit_state_bytes")
        .expect("状态字节数要被记下来");
    assert!(bytes.peak > 0, "一份单元快照不可能是 0 字节");
    assert!(
        bytes.peak < 64 * 1024,
        "§17 的门槛是 64 KiB，而这一份是 {} 字节——超了就该有人来看",
        bytes.peak
    );
}

#[test]
fn a_refused_wake_is_not_measured_as_a_slow_restore() {
    // 被拒的唤醒**不计时**。
    //
    // 一次被拒的唤醒不是"恢复花了很久"，它是"**根本没有恢复**"。把它算进恢复耗时，
    // 会让 p95 被一堆瞬间失败拉低——而那正是这个指标最不该出现的样子：
    // 它看起来变快了，实际是有东西根本没跑起来。
    //
    // §9.2 的"权限重验"就是这条拒绝的来源："完成版本迁移与**权限重验**后进入 READY"。
    let mut subject = registered_subject();
    subject.sleep(at(5)).expect("降温");

    let before = subject
        .resource_ledger()
        .metric("unit_restore_ms")
        .map_or(0, |metric| metric.peak);
    assert_eq!(before, 0, "还没成功唤醒过");

    // 撤回能力：那个单元不该再被唤醒。
    subject
        .revoke_capability(
            &CapabilityPolicyRef::new(CAP).expect("固定能力策略"),
            at(6),
        )
        .expect("撤回");

    let refused = subject.wake(at(7)).expect("唤醒调用本身不报错");
    match &refused {
        WakeOutcome::Refused { reason } => {
            assert!(reason.contains("已失效"), "理由要指向权限：{reason}");
        }
        other => panic!("撤回之后不该唤醒成功：{other:?}"),
    }
    assert!(
        subject
            .resource_ledger()
            .metric("unit_restore_ms")
            .is_none()
            || subject
                .resource_ledger()
                .metric("unit_restore_ms")
                .is_some_and(|metric| metric.peak == 0),
        "被拒的唤醒不该进恢复耗时的账"
    );
    assert!(!subject.is_awake());
}

#[test]
fn sleeping_hands_pending_work_over_instead_of_erasing_it() {
    // §9.2："**存在不明副作用时由持久在线动作账继续核对，不以卸载单元'解决'它。**"
    //
    // 这一条测的是那句话的后半句：降温**移交**未决动作并如实报出来，而不是把它们丢掉。
    // "卸载"看起来像把事情解决了——机器轻了、账上也没有未决状态了——而实际上那件事
    // 到底做没做，没有人知道。
    let mut subject = registered_subject();

    // 投递一次写入，但不推进它：于是它躺在待推进队列里。
    subject
        .request_write(WATCHED, "内容", at(5))
        .expect("投递");
    let pending_before = subject.pending_actions();
    assert!(pending_before > 0, "投递之后该有未决动作");

    let slept = subject.sleep(at(6)).expect("降温");

    // 降温**没有**把它抹掉：待推进队列里还在，等着下一次有人核对。
    assert_eq!(
        subject.pending_actions(),
        pending_before,
        "降温不是放弃——未决动作要留着，由在线动作账继续核对"
    );
    // 而快照里不许再留着它（§9.2 的"冷态只在注册表和持久邮箱中存在"）。
    let snapshot = subject
        .store()
        .unit(subject.cluster().unit_id())
        .expect("读")
        .expect("存在");
    assert!(snapshot.pending_action_ids.is_empty());
    assert_eq!(snapshot.state, UnitState::Cold);
    let _ = slept;
}

#[test]
fn a_cold_unit_restores_its_semantic_state_across_a_restart() {
    // 这是"冷恢复"那一行的实质，也是 §9.2 那句"**跨重启恢复的是语义状态，不是 OS 线程栈**"。
    //
    // 上面几条测的是"同一进程里睡下去再醒来"。这一条测的是**同一份库、新的进程**：
    // 那份状态得从盘上被读回来、过权限重验、追平游标，然后接着用。
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    let (evidence_before, cursor_before, version_before) = {
        let mut subject = subject_on(Store::open(path.clone(), at(0)).expect("打开存储"));
        subject.register_unit(at(1)).expect("登记");
        subject.wake(at(1)).expect("唤醒");
        delegate(&mut subject);
        subject
            .observe(WATCHED, DataClass::Personal, at(2))
            .expect("观测");
        subject
            .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(3))
            .expect("跑一轮");

        let live = subject.cluster().snapshot();
        subject.sleep(at(4)).expect("降温");
        (
            live.evidence_refs.len(),
            live.last_applied_sequence,
            live.strategy_version.to_string(),
        )
    };
    assert!(evidence_before > 0, "干过一轮活就该有证据");

    // 新进程、同一份库。
    let mut restarted = subject_on(Store::open(path, at(5)).expect("重新打开"));
    let woken = restarted.wake(at(5)).expect("唤醒");
    let WakeOutcome::Ready { snapshot, .. } = woken else {
        panic!("重启之后应当能恢复：{woken:?}");
    };
    assert_eq!(
        snapshot.evidence_refs.len(),
        evidence_before,
        "恢复出来的状态要带着那一轮拿到的证据"
    );
    assert_eq!(
        snapshot.last_applied_sequence, cursor_before,
        "游标是语义状态的一部分：它决定这个单元重启之后从哪儿接着读"
    );
    assert_eq!(
        snapshot.strategy_version.to_string(),
        version_before,
        "当时用的是哪一版判定标准，也是要恢复的东西（§13.2 的\"版本化启用\"）"
    );

    // 而它真的能接着干活。
    restarted
        .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(6))
        .expect("恢复之后接着跑");
    assert!(
        restarted
            .resource_ledger()
            .metric("unit_restore_ms")
            .is_some(),
        "重启之后的这次唤醒也要被计时"
    );
}

#[test]
fn registering_twice_does_not_erase_the_state_that_was_already_there() {
    // 登记是幂等的，而且**不覆盖**已有快照：把它盖掉等于抹掉上一段运行的状态，
    // 而那正是重启之后要恢复的东西。
    //
    // 快照是**降温时**落库的，所以先跑一轮再降温，才有一份"干过活"的状态可谈。
    let mut subject = registered_subject();
    subject.sleep(at(5)).expect("降温");

    let stored = subject
        .store()
        .unit(subject.cluster().unit_id())
        .expect("读")
        .expect("存在");
    assert!(
        !stored.evidence_refs.is_empty(),
        "降温时落库的快照里该带着这一轮拿到的证据"
    );

    assert!(!subject.register_unit(at(7)).expect("再登记一次"));
    let again = subject
        .store()
        .unit(subject.cluster().unit_id())
        .expect("读")
        .expect("存在");
    assert_eq!(
        again.evidence_refs, stored.evidence_refs,
        "再登记一次不该把已有的状态换掉"
    );
}
