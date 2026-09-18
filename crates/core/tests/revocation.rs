//! 权限撤回的回归测试（§12.1、§12.3）。
//!
//! §12.1 对 A1 那一档的放行要求是"范围限定授权，**撤回立即生效**"。这句话有**两半**，
//! 而它们是同一个问题的两面：
//!
//! * **将来**——撤回之后，这个能力名下不再产生新的观测，也不再签发任何许可。
//! * **过去**——撤回之前读到的内容**还算不算数**。
//!
//! 只做前一半的话，一份通过已撤回授权读到的内容会继续被检索、被引用、被写进模型上下文；
//! 撤回就成了一句只对将来有效的空话。而只做后一半更糟：那意味着撤回了却还能继续读。

use soca_contracts::{
    ActionLevel, Candidate, CapabilityPolicyRef, DataClass, EvidenceRef, ExplorationQuota,
    GoalBudget, ModelBackend, ModelBudget, ModelVersion, PermissionScope, SubjectId, UserChannel,
    VerificationKind, WallClock,
};
use soca_core::{ActionBroker, AdvanceStep, RoundOutcome, SimulatedOs, Subject};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";
const CAP_A: &str = "cap:read-selected-folder";
const CAP_B: &str = "cap:approval-window";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn cap(name: &str) -> CapabilityPolicyRef {
    CapabilityPolicyRef::new(name).expect("固定能力策略")
}

fn owner() -> SubjectId {
    SubjectId::new("user:local").expect("固定主体")
}

fn scope(name: &str) -> PermissionScope {
    PermissionScope {
        capability_policy_ref: cap(name),
        max_action_level: ActionLevel::A1,
    }
}

fn budget() -> GoalBudget {
    GoalBudget::new(16, 32, 8192, 3_600_000).expect("合法额度")
}

fn subject() -> Subject {
    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(SimulatedOs::new()),
        DesktopAndFilesCluster::new(WATCHED, Vec::new()).expect("装配能力簇"),
        SubjectId::new("user:local").expect("固定主体"),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
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

fn delegate_a_goal(subject: &mut Subject, capability: &str) -> soca_contracts::GoalId {
    let goal_id = subject
        .delegate(
            "核对摘要文件",
            UserChannel::Chat,
            scope(capability),
            budget(),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");
    goal_id
}

// ---------------------------------------------------------------------------
// 将来
// ---------------------------------------------------------------------------

#[test]
fn revoking_stops_new_observations() {
    let mut subject = subject();
    let goal_id = delegate_a_goal(&mut subject, CAP_A);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("撤回之前能观测");

    assert!(subject.revoke_capability(&cap(CAP_A), at(3)).expect("撤回").was_granted);

    let refused = subject.observe(WATCHED, DataClass::Personal, at(4));
    assert!(
        matches!(refused, Err(soca_core::CoreError::CapabilityRevoked { .. })),
        "撤回之后不该再产生新的观测：{refused:?}"
    );
    // 事件账上只有两条：委托时那条用户输入，和撤回之前那次观测。**没有第三条**——
    // 这个断言比"返回了错误"更硬：撤回不是"记下来但不用"。
    assert_eq!(
        subject.store().read_events_after(0, 64).expect("读事件").len(),
        2,
        "被拒绝的观测不该在账上留下痕迹"
    );
    assert!(subject.goals().goal(&goal_id).is_some());
}

#[test]
fn regranting_lets_observations_resume() {
    // 撤回是**可逆**的，否则用户不敢用它。而"恢复"是一次显式的授予，不是等一会儿就好。
    let mut subject = subject();
    delegate_a_goal(&mut subject, CAP_A);
    subject.revoke_capability(&cap(CAP_A), at(2)).expect("撤回");
    assert!(subject.observe(WATCHED, DataClass::Personal, at(3)).is_err());

    assert!(
        subject.grant_capability(cap(CAP_A), at(3)).expect("重新授予"),
        "这是一次新的授予"
    );
    subject
        .observe(WATCHED, DataClass::Personal, at(4))
        .expect("恢复之后又能观测");
}

#[test]
fn a_revoked_capability_no_longer_issues_permits() {
    // §12.2 的签发判定与 §12.1 的撤回必须连起来。断开的话，撤回只挡住了观测，
    // 而动作照旧——那正是撤回最不该留的那个口子。
    let mut subject = subject();
    let goal_id = delegate_a_goal(&mut subject, CAP_A);
    subject.revoke_capability(&cap(CAP_A), at(2)).expect("撤回");

    subject.request_write(WATCHED, "摘要内容", at(3)).expect("投递");
    let report = subject
        .run_round(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(4))
        .expect("跑一轮");

    match &report.outcome {
        RoundOutcome::Advanced {
            step: AdvanceStep::Refused { reason },
        } => assert!(reason.contains("撤回") || reason.contains("授权"), "实际：{reason}"),
        other => panic!("已撤回的授权不该签发许可，实际：{other:?}"),
    }
    assert_eq!(subject.goals().goal(&goal_id).map(|goal| goal.state), Some(soca_contracts::GoalState::Active));
}

// ---------------------------------------------------------------------------
// 过去
// ---------------------------------------------------------------------------

#[test]
fn revoking_invalidates_memories_derived_from_its_events() {
    let mut subject = subject();
    delegate_a_goal(&mut subject, CAP_A);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .run_round(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");
    assert_eq!(
        subject.store().memory_count(&SubjectId::new("user:local").expect("固定主体")).expect("计数"),
        1,
        "环路应当记下一条结论"
    );

    let report = subject.revoke_capability(&cap(CAP_A), at(4)).expect("撤回");
    assert_eq!(
        report.events_covered, 2,
        "委托时那条用户输入也在这个授权下"
    );
    assert_eq!(report.memories_invalidated, 1, "它引用的证据来自被撤回的授权");
    assert_eq!(report.awaiting_purge, 1, "立即不可见，等着清理");

    assert_eq!(
        subject.store().memory_count(&SubjectId::new("user:local").expect("固定主体")).expect("计数"),
        0,
        "撤回之后它不再被检索到"
    );
    assert_eq!(
        subject.store().recall(&SubjectId::new("user:local").expect("固定主体"), None, at(4)).expect("召回").len(),
        0
    );
}

#[test]
fn revoking_one_capability_leaves_another_capabilitys_memories_alone() {
    // 这条是整份文件里最难做对、也最值得钉住的一条。
    //
    // 顺序是刻意的：**先在 B 下面攒出一条记忆，再在 A 下面产生事件**。反过来的话，
    // 那条结论会把两边的证据一起引用（能力簇的证据是累积的），于是撤回 A 时它本来就该失效——
    // 测试就会变成在测"引用被撤回证据的结论会失效"，而不是"另一个授权的记忆不受影响"。
    let mut subject = subject();
    assert!(
        subject
            .grant_capability(cap(CAP_B), at(0))
            .expect("授予 B")
    );

    let goal_b = delegate_a_goal(&mut subject, CAP_B);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("在 B 下观测");
    subject
        .run_round(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");
    let owner = SubjectId::new("user:local").expect("固定主体");
    assert_eq!(subject.store().memory_count(&owner).expect("计数"), 1);
    subject.abandon(&goal_b, at(4)).expect("结束 B");

    // A 下面产生一个事件，但它没有派生任何记忆。
    let goal_a = delegate_a_goal(&mut subject, CAP_A);
    subject
        .observe(WATCHED, DataClass::Personal, at(5))
        .expect("在 A 下观测");
    // 四条：B 的委托、B 的观测、A 的委托、A 的观测。委托本身也是一条事件——
    // 用户说过的话在事件账上，而它同样属于目标声明的那个授权。
    assert_eq!(subject.store().read_events_after(0, 64).expect("读事件").len(), 4);

    let report = subject.revoke_capability(&cap(CAP_A), at(6)).expect("撤回 A");
    assert_eq!(report.events_covered, 2, "A 覆盖两条事件：委托时的那条与观测那条");
    assert_eq!(report.memories_invalidated, 0, "A 的事件没有派生过记忆");
    assert_eq!(
        subject.store().memory_count(&owner).expect("计数"),
        1,
        "B 名下的记忆不该因为撤回 A 而消失"
    );
    assert!(
        subject.granted_capabilities().contains(&&cap(CAP_B)),
        "B 仍然生效"
    );
    assert!(!subject.granted_capabilities().contains(&&cap(CAP_A)));
    assert!(subject.goals().goal(&goal_a).is_some());
}

#[test]
fn revoking_is_idempotent() {
    let mut subject = subject();
    delegate_a_goal(&mut subject, CAP_A);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .run_round(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");

    let first = subject.revoke_capability(&cap(CAP_A), at(4)).expect("第一次");
    assert!(first.was_granted);
    assert_eq!(first.memories_invalidated, 1);

    let second = subject.revoke_capability(&cap(CAP_A), at(5)).expect("第二次");
    assert!(!second.was_granted, "第二次不该说自己是第一次撤回");
    assert_eq!(
        second.memories_invalidated, 0,
        "那些记忆已经不可见了，再标一次不该有新的效果"
    );
    assert_eq!(second.awaiting_purge, 1, "它们还在等着清理");
}

#[test]
fn after_revoking_the_cluster_material_is_retracted_not_just_the_memory() {
    // 撤回的第二个后果，也是此前漏掉的那一半（§7.2 的"仍可访问"）。
    //
    // 只做记忆那一半的话，撤回之后系统会**继续拿被撤回的证据下结论**——只是那些结论存不进
    // 记忆而已。而"下出了结论但存不进去"比"下不出结论"难发现得多：界面上一切正常，
    // 环路照常一轮一轮地跑，什么错也不报。
    let mut subject = subject();
    delegate_a_goal(&mut subject, CAP_A);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .run_round(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("第一轮");
    assert_eq!(subject.store().memory_count(&owner()).expect("计数"), 1);

    let report = subject.revoke_capability(&cap(CAP_A), at(4)).expect("撤回");
    assert_eq!(report.memories_invalidated, 1);
    assert_eq!(
        report.evidence_retracted, 1,
        "簇手里的那条证据也要失效——它才是「还能拿来下结论的材料」"
    );

    // 簇仍然会提出那条结论（它的信念没变），但它现在必须**出局**。
    let (candidates, selection) = subject
        .select(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(5))
        .expect("选择");
    let claim_index = candidates
        .candidates
        .iter()
        .position(|candidate| matches!(candidate, Candidate::Claim { .. }))
        .expect("结论仍在候选里");
    let review = selection
        .reviews
        .iter()
        .find(|review| review.candidate_index == claim_index)
        .expect("每条候选都要有档案");
    assert!(
        review.is_refuted(),
        "引用了已撤回证据的结论必须出局：{review:?}"
    );
    assert!(
        review
            .outcomes
            .iter()
            .any(|outcome| outcome.kind == VerificationKind::EvidenceAccess),
        "而且要报成「证据不可用」，不是报成一个别的毛病：{review:?}"
    );
    assert_ne!(
        selection.selected_index(),
        Some(claim_index),
        "被否定的候选不该被选中"
    );
}

// ---------------------------------------------------------------------------
// 环路里的表现
// ---------------------------------------------------------------------------

#[test]
fn the_loop_reports_a_revoked_capability_as_a_refusal_not_a_crash() {
    // 撤回是一个需要人处理的拒绝，不是"内部出错了"。抛成普通错误的话，环路会断在一句
    // "错误"上，而真正的原因（授权被收回了）看不出来。
    let mut subject = subject();
    delegate_a_goal(&mut subject, CAP_A);
    subject.revoke_capability(&cap(CAP_A), at(2)).expect("撤回");

    let report = subject
        .run_round(&soca_contracts::SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");

    match &report.outcome {
        RoundOutcome::Advanced {
            step: AdvanceStep::Refused { reason },
        } => assert!(reason.contains("已撤回"), "实际：{reason}"),
        other => panic!("应当报成拒绝而不是崩溃：{other:?}"),
    }

    let entries = subject.store().audit_entries(64).expect("读审计");
    assert!(
        entries.iter().any(|entry| entry.category == "capability_revoked"),
        "撤回要留痕：{entries:?}"
    );
    assert!(
        entries.iter().any(|entry| entry.category == "capability_denied"),
        "被拒绝也留痕"
    );
}

#[test]
fn an_observation_under_a_revoked_capability_is_not_written_at_all() {
    // 撤回的意义在于**它没有发生**，而不是"发生了但不用"。这一条直接对着事件账断言。
    let mut subject = subject();
    delegate_a_goal(&mut subject, CAP_A);
    subject.revoke_capability(&cap(CAP_A), at(2)).expect("撤回");

    let before = subject.store().read_events_after(0, 64).expect("读事件").len();
    let _ = subject.observe(WATCHED, DataClass::Personal, at(3));
    let after = subject.store().read_events_after(0, 64).expect("读事件").len();

    assert_eq!(before, after, "被拒绝的观测不该在账上留下任何痕迹");
}

#[test]
fn evidence_built_from_an_event_id_round_trips() {
    // 撤回依赖 `EvidenceRef::for_observation` 与 `origin_event_id` 互为逆运算。
    // 只改一边的表现是"撤回漏掉了一批记忆"或"来源链断了"——两种都不会报错，所以在这里钉住。
    let event_id = soca_contracts::EventId::parse("11111111-1111-4111-8111-111111111111")
        .expect("固定事件");
    let reference = EvidenceRef::for_observation(&event_id).expect("构造");
    assert_eq!(reference.origin_event_id(), Some(event_id));
}
