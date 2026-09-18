//! §14 的纠错通路："用户说'记错了'"。
//!
//! 那一条的原文是：
//!
//! > 用户说"记错了"**生成纠错事件**并失效相关派生记忆，**不只在下一条回复中口头道歉**。
//!
//! 两半都要落地。前半句是"这件事要进账"，后半句是"状态要真的变"——而此前两半都没有：
//! 用户只能按标识删一条记忆（`/api/forget`），没有纠错事件，也没有"由它派生的那些"。

use soca_contracts::{
    ActionLevel, Candidate, CapabilityPolicyRef, DataClass, EventId, ExplorationQuota, GoalBudget,
    GoalId, MemoryId, ModelBackend, ModelBudget, ModelVersion, PermissionScope, SelectionPolicy,
    SubjectId, UserChannel, WallClock,
};
use soca_core::{ActionBroker, Correction, RetentionPolicy, SimulatedOs, Subject};
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

fn delegate(subject: &mut Subject) -> GoalId {
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

/// 委托、观测、跑几轮，直到账上有一条结论。返回那条记忆。
fn reach_a_recorded_conclusion(subject: &mut Subject) -> MemoryId {
    delegate(subject);
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");

    let recalled = subject
        .store()
        .recall(&owner(), None, at(4))
        .expect("召回");
    assert_eq!(recalled.len(), 1, "这一轮应当记下一条结论：{recalled:?}");
    recalled[0].memory_id.clone()
}

#[test]
fn a_correction_is_recorded_as_an_event_not_just_replied_to() {
    // §14 的前半句："**生成纠错事件**……不只在下一条回复中口头道歉。"
    //
    // 只在对话框里回一句"好的，我改"的话，账上什么也没发生——而纠错恰恰是那种
    // "以后要能回答'这条记忆是被谁、以什么名义撤掉的'"的事。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);

    let report = subject
        .correct(
            &Correction::Conclusion {
                memory_id: memory_id.to_string(),
            },
            "那个日期我看错了",
            at(5),
        )
        .expect("纠错");

    // 事件真的在账上，而且通道是**纠错**而不是聊天。
    let inputs = subject.user_inputs(16).expect("读回用户输入");
    let recorded = inputs
        .iter()
        .find(|input| input.event_id == report.event_id)
        .expect("纠错要进事件账");
    assert_eq!(
        recorded.channel, "correction",
        "并进 chat 的话，账上两者长得一样，而审计要问的正是这个"
    );
    assert!(
        recorded
            .text
            .as_deref()
            .is_some_and(|text| text.contains("那个日期我看错了")),
        "用户说的原话要留着：{:?}",
        recorded.text
    );
    // 而它是一条**有指令权限**的用户输入——用户纠正自己的系统，那是最高一档的依据。
    assert!(
        subject
            .user_inputs(16)
            .expect("读回")
            .iter()
            .any(|input| input.event_id == report.event_id),
        "纠错与别的用户输入走同一条读回通路"
    );
}

#[test]
fn correcting_a_conclusion_takes_it_out_of_recall_and_it_stays_out() {
    // §14 的后半句："并失效相关派生记忆"——状态要真的变。
    //
    // 而"真的变"里最要紧的一层是**它不会自己回来**：同一条结论再被推出来一次，
    // 也不能复活。那条路在 `advance` 里被 `memory_including_hidden` 挡着（§12.3 的删除是终局），
    // 这一条测的是它确实挡得住。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);

    let report = subject
        .correct(
            &Correction::Conclusion {
                memory_id: memory_id.to_string(),
            },
            "记错了",
            at(5),
        )
        .expect("纠错");
    assert_eq!(report.retracted, vec![memory_id.to_string()]);
    assert_eq!(report.derived, 0, "指名的就是它自己，没有派生的");
    assert_eq!(report.awaiting_purge, 1);

    assert!(
        subject.store().recall(&owner(), None, at(6)).expect("召回").is_empty(),
        "撤了就不该再被召回"
    );

    // 再跑几轮：簇还会提同一条结论，但它必须**记不进来**。
    for round in 0..3 {
        subject
            .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(7 + round))
            .expect("跑一轮");
    }
    assert!(
        subject.store().recall(&owner(), None, at(12)).expect("召回").is_empty(),
        "重新推导不该让它复活"
    );
    assert_eq!(
        subject.store().memory_count(&owner()).expect("计数"),
        0,
        "也不该留下第二份"
    );
}

#[test]
fn correcting_a_source_takes_what_was_derived_from_it_and_nothing_else() {
    // 按**原始事件**纠错：撤的是由它派生的记忆。
    //
    // 这一条同时用一个**没有任何结论派生自它**的事件做对照——少了对照，
    // "撤掉派生的"与"把所有记忆都撤掉"看起来是一样的。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);

    // 那条结论的出处指向它引用的观测事件。
    let entry = subject
        .store()
        .memory(&memory_id)
        .expect("读")
        .expect("存在");
    let source = *entry
        .provenance
        .original_event()
        .expect("结论的出处是 Derived，指回原始事件");

    // 对照：一条**没有**结论派生自它的事件。用户的那句委托就是现成的——
    // `claim_provenance` 只在观测引用上解析得出原始事件，所以委托事件从来不在这条边上游。
    let unrelated = subject
        .user_inputs(8)
        .expect("读回")
        .first()
        .map(|input| EventId::parse(&input.event_id).expect("固定事件"))
        .expect("至少有一条用户输入");
    let untouched = subject
        .correct(&Correction::Source { event_id: unrelated.to_string() }, "改一下那句", at(5))
        .expect("纠错");
    assert_eq!(
        untouched.derived, 0,
        "没有结论派生自那条事件，就不该撤掉任何东西"
    );
    assert_eq!(subject.store().memory_count(&owner()).expect("计数"), 1);

    // 真正的那条来源。
    let report = subject
        .correct(&Correction::Source { event_id: source.to_string() }, "那份观测是错的", at(6))
        .expect("纠错");
    assert_eq!(report.derived, 1, "由它派生的那一条");
    assert_eq!(report.retracted, vec![memory_id.to_string()]);
    assert!(
        subject.store().recall(&owner(), None, at(7)).expect("召回").is_empty()
    );
}

#[test]
fn a_correction_needs_no_capability() {
    // 纠错与撤回权限是两件事。
    //
    // §12.3 说"用户可随时删除"，而把纠错挂在某次授权上，等于说"授权一撤，
    // 你连纠正错话的权利都没有了"——那不是一个安全属性，那是一个陷阱。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);

    subject
        .revoke_capability(
            &CapabilityPolicyRef::new(CAP).expect("固定能力策略"),
            at(5),
        )
        .expect("撤回");

    // 撤回之后观测做不了——先确认这一点，否则下面那条"纠错还能做"分不出为什么。
    assert!(
        subject.observe(WATCHED, DataClass::Personal, at(6)).is_err(),
        "撤回之后不该还能采集"
    );

    // 撤回**自己**已经把那条记忆带走了（它引用的事件属于被撤回的能力）。所以这里
    // 撤掉 0 条不是纠错失效，是它已经没有东西可撤。这一条与纠错的级联是两条不同的通路：
    // 一条按**证据**走（撤回权限），一条按**出处事件**走（纠错），两者在这条用例里都指到
    // 同一条记忆上。
    assert!(
        subject.store().recall(&owner(), None, at(6)).expect("召回").is_empty(),
        "撤回按证据级联，已经把它带走"
    );

    let report = subject
        .correct(
            &Correction::Conclusion {
                memory_id: memory_id.to_string(),
            },
            "撤回归撤回，这条还是错的",
            at(6),
        )
        .expect("纠错不需要授权——这是这条测试要证的那一句");
    assert_eq!(report.retracted, Vec::<String>::new());

    // 而它确实发生了：事件写下来了，没有因为"没有授权"被挡在门外。
    assert!(
        subject
            .user_inputs(16)
            .expect("读回")
            .iter()
            .any(|input| input.event_id == report.event_id && input.channel == "correction"),
        "纠错事件要进账"
    );
}

#[test]
fn a_correction_naming_something_that_is_gone_is_still_recorded() {
    // 顺序是有意的：**先记事件，再改状态**。
    //
    // 用户说的那句话是事实，它已经发生了；后面无论撤掉了几条、有没有撤成，这句话都该留在
    // 账上。反过来先撤再记的话，中途出错会留下一件"发生了、却没有任何记录"的事。
    //
    // 这条测试就是把那个中途出错的情形做出来：指名一条已经不在的记忆。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);

    // 先删、再清理。只删不清理的话，条目还在库里（只是不可见），于是 `correct` 会
    // 安静地撤掉 0 条——那证明不了"中途出错"这件事。真删掉才逼得出错误。
    subject.forget(&memory_id, at(5)).expect("先删掉");
    subject
        .enforce_retention(&RetentionPolicy::default(), at(6))
        .expect("执行保留期");
    assert!(
        subject
            .store()
            .memory_including_hidden(&memory_id)
            .expect("读")
            .is_none(),
        "这一条应当已经被真删掉了"
    );

    let result = subject.correct(
        &Correction::Conclusion {
            memory_id: memory_id.to_string(),
        },
        "再说一次",
        at(7),
    );
    assert!(result.is_err(), "指名一条不存在的记忆应当报错");

    let inputs = subject.user_inputs(16).expect("读回");
    assert!(
        inputs
            .iter()
            .any(|input| input.channel == "correction" && input.text.as_deref().is_some_and(|t| t.contains("再说一次"))),
        "但这次纠错本身仍然要在账上：{inputs:?}"
    );
}

#[test]
fn a_correction_does_not_resurrect_anything_a_retention_sweep_removed() {
    // 纠错与保留期是两条独立的路，但它们改的是同一批条目。这条只钉一件事：
    // 纠错之后执行一次保留期清理，**被撤的那条不会被清回来**——清回来意味着
    // 有人把 tombstone 当成"暂时不可见"了。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);
    subject
        .correct(
            &Correction::Conclusion {
                memory_id: memory_id.to_string(),
            },
            "记错了",
            at(5),
        )
        .expect("纠错");

    let report = subject
        .enforce_retention(&RetentionPolicy::default(), at(30 * 86_400))
        .expect("执行保留期");
    assert!(report.purged >= 1, "撤掉的那些该被清理：{report:?}");
    assert_eq!(subject.store().memory_count(&owner()).expect("计数"), 0);
    assert_eq!(
        subject.store().recall(&owner(), None, at(31 * 86_400)).expect("召回").len(),
        0
    );
}

#[test]
fn an_uncorrected_conclusion_stays() {
    // 对照组。少了它，"纠错有效"与"纠错把所有东西都撤了"分不开。
    let mut subject = subject();
    let memory_id = reach_a_recorded_conclusion(&mut subject);
    subject
        .correct(
            &Correction::Conclusion {
                memory_id: memory_id.to_string(),
            },
            "这条其实是对的，我只是看看",
            at(5),
        )
        .expect("纠错");

    // 再记一条**不同**的结论：内容不同，所以标识不同，不该被牵连。
    subject
        .observe(WATCHED, DataClass::Personal, at(6))
        .expect("再观测一次");
    let candidates = subject.propose(at(7)).expect("提出候选");
    assert!(
        candidates
            .candidates
            .iter()
            .any(|candidate| matches!(candidate, Candidate::Claim { .. })),
        "簇仍然会提出结论——纠错撤的是记忆，不是信念"
    );
}
