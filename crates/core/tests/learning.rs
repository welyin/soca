//! 策略改进的准入，端到端（§13.2 的第二种学习）。
//!
//! §13.2 那两句分开看：
//!
//! > 记忆/策略改进：摘要、规则和技能作为候选，经**回放与保留任务集检验**后**版本化启用**。
//! > ……**系统可建议**新的单元或拓扑，但**没有自行安装可执行代码、修改签名策略或提高权限
//! > 的能力**。
//!
//! 本文件测的是它们的合流：**提议方与闸都做完了自己那一半，策略才真的换掉**，
//! 而且换掉之后**下一轮的判定真的跟着变了**——不是只多了一个版本号。
//!
//! 这一条链的上游是 §14 的"记错了"：那是这套系统里唯一一个明确的"我们错了"信号，
//! 而它现在有消费者了。

use soca_contracts::{
    ActionLevel, Candidate, CapabilityPolicyRef, CognitiveUnit, DataClass, ExplorationQuota,
    GoalBudget, GoalId, MemoryId, ModelBackend, ModelBudget, ModelVersion, PermissionScope,
    RetryWhen, SelectionPolicy, SubjectId, UserChannel, WallClock,
};
use soca_core::{ActionBroker, Correction, SimulatedOs, Subject};
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
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000b2").expect("固定 boot"),
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

/// 观测一次、跑一轮，直到记下一条结论。返回那条记忆的标识。
fn record_one(subject: &mut Subject, offset: i64) -> MemoryId {
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
    let recalled = subject
        .store()
        .recall(&owner(), None, at(offset + 2))
        .expect("召回");
    recalled
        .last()
        .map(|entry| entry.memory_id.clone())
        .expect("这一轮应当记下一条结论")
}

fn correct(subject: &mut Subject, memory_id: &MemoryId, offset: i64) {
    subject
        .correct(
            &Correction::Conclusion {
                memory_id: memory_id.to_string(),
            },
            "这条错了",
            at(offset),
        )
        .expect("纠错");
}

#[test]
fn the_holdout_reads_mistakes_from_the_correction_path_and_nothing_else() {
    // §13.2 的"保留任务集"从哪来。这里钉的是**它不只收错案，还只收那一种错案**。
    //
    // "被删掉"与"被证明是错的"是两件事，而两者都让结论不可见。把三者（用户删除、保留期
    // 到期、证据被撤回）当成同一个信号，系统学到的教训就会是"有人删过东西"，
    // 然后据此把门槛提上去——而撤回一次权限是用户的常规操作，不是一次纠错。
    let mut subject = subject();
    delegate(&mut subject);
    let first = record_one(&mut subject, 2);
    let second = record_one(&mut subject, 5);
    assert_ne!(first, second, "两次观测产生的是两条记忆");

    // 一条纠错，一条用户主动删除。
    correct(&mut subject, &first, 8);
    subject.forget(&second, at(9)).expect("用户主动删掉");

    let holdout = subject.holdout(at(10)).expect("保留集");
    assert_eq!(
        holdout.known_wrong.len(),
        1,
        "只有被纠错的那条算错案：{:?}",
        holdout.known_wrong
    );
    assert_eq!(holdout.known_wrong[0].memory_id, first.to_string());
    assert!(
        holdout.known_right.is_empty(),
        "两条都不可见了，没有对的样本：{:?}",
        holdout.known_right
    );
}

#[test]
fn a_suggestion_that_the_holdout_supports_is_admitted_and_then_really_bites() {
    // 这一条是整项的落点：**提议 → 闸 → 启用 → 下一轮的判定真的跟着变了**。
    let mut subject = subject();
    delegate(&mut subject);

    // 两条结论：第一条 1 条证据，第二条 2 条（多观测了一次）。
    let thin = record_one(&mut subject, 2);
    let thick = record_one(&mut subject, 5);
    let thin_count = subject
        .store()
        .memory_including_hidden(&thin)
        .expect("读")
        .expect("存在")
        .evidence_refs
        .len();
    let thick_count = subject
        .store()
        .memory_including_hidden(&thick)
        .expect("读")
        .expect("存在")
        .evidence_refs
        .len();
    assert!(thin_count < thick_count, "两次观测应当让第二条证据更多");

    // 把**证据少的那条**纠正掉：它当时刚好够过 A1 的门槛（1 条）。
    correct(&mut subject, &thin, 8);

    let version_before = subject.strategy_version().to_string();
    let base_before = subject.strategy().base_evidence;

    // 系统提一个。
    let suggestion = subject
        .propose_strategy(ActionLevel::A1, at(9))
        .expect("提议")
        .expect("有一条错案够格，应当提得出来");
    assert_eq!(
        suggestion.policy.base_evidence,
        base_before + 1,
        "刚好挡住那一条：从 {base_before} 提到 {}",
        base_before + 1
    );
    assert!(
        suggestion.rationale.contains(&thin.to_string()),
        "理由要指出依据：{}",
        suggestion.rationale
    );

    // 过闸。
    let admission = subject
        .admit_strategy(&suggestion, ActionLevel::A1, at(10))
        .expect("准入");
    assert!(admission.is_admitted(), "这条提议该过闸：{admission:?}");
    assert_eq!(subject.strategy().base_evidence, base_before + 1);
    assert_ne!(
        subject.strategy_version().as_str(),
        version_before,
        "启用了新策略就该换版本号"
    );

    // 而它真的生效了：**调用方递一个更松的策略进来也没用**。
    //
    // 这是 §13.2 那句"没有自行……提高权限的能力"的另一面——反过来说，任何一条路径也不该能
    // 悄悄降低它，而策略是每一轮都要从外面传进来的。少了 `effective_policy` 那一行，
    // 这道闸就只管住了它自己那一个入口。
    let effective = subject.effective_policy(&SelectionPolicy::default());
    assert_eq!(effective.evidence_bar(ActionLevel::A1), base_before + 1);

    // 学习要挡住的那个形状——**1 条证据的结论**——现在过不了 A1 的门槛。
    let thin_only = soca_contracts::select(
        &one_evidence_claim(),
        Vec::new(),
        &effective,
        ActionLevel::A1,
    )
    .expect("选择");
    assert_eq!(thin_only.selected_index(), None, "这个形状该被挡住了");
    assert_eq!(
        thin_only
            .rejection_for(0)
            .expect("有拒绝记录")
            .retry_when,
        RetryWhen::MoreEvidence { short_by: 1 },
        "而它说得出还差几条"
    );

    // 但**不是所有结论**都被挡住了。这一条是对照组：簇手里那条结论有 2 条证据，
    // 学到的门槛没有动它，它照常被选中。
    //
    // 少了这一条，"学习生效了"与"学习把系统卡死了"分不开——而后者在审计上看起来一模一样：
    // 都只是版本号变了。
    let (candidates, selection) = subject
        .select(&SelectionPolicy::default(), ActionLevel::A1, at(11))
        .expect("选择");
    let claim = candidates
        .candidates
        .iter()
        .position(|candidate| matches!(candidate, Candidate::Claim { .. }))
        .expect("簇仍然会提出结论——学的是判定标准，不是信念");
    assert_eq!(
        selection.selected_index(),
        Some(claim),
        "证据够的那条仍然能推进：{:?}",
        selection.rationale
    );
}

/// 一条只有 1 条证据的结论候选。
///
/// 手搓而不是从簇里取，是因为**要挡住的那个形状在真实数据里已经消失了**：簇手里那条结论
/// 在两次观测之后有 2 条证据。这一份代表的是"下一轮里会出现的那种结论"——
/// 而 `selection` 的判定只看得见证据条数，看不见它从哪来。
fn one_evidence_claim() -> soca_contracts::CandidateSet {
    soca_contracts::CandidateSet {
        candidates: vec![Candidate::Claim {
            statement: "文件是 sha256:aaa".to_string(),
            evidence_refs: vec![
                soca_contracts::EvidenceRef::new("obs:a".to_string()).expect("固定证据"),
            ],
        }],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    }
}

#[test]
fn a_suggestion_the_holdout_cannot_support_is_refused_and_the_strategy_stays() {
    // 另一半：**系统提了一个它做不到的改动**，闸挡住了它，而策略一点没变。
    //
    // 场景：证据多的那条（2 条）被证明是错的。要挡住它，门槛得提到 3——而 3 会把那条
    // 1 条证据的、事后确认是对的结论一起挡掉。
    // 也就是说：**这个错不能用提高门槛来纠正**，得换个办法。
    //
    // 这正是"保留任务集"存在的全部理由。没有它，系统会兴高采烈地把门槛提到 3，
    // 然后在下一轮什么都推不动——而账上看起来它"学到东西了"。
    let mut subject = subject();
    delegate(&mut subject);
    let thin = record_one(&mut subject, 2);
    let thick = record_one(&mut subject, 5);
    correct(&mut subject, &thick, 8);

    let version_before = subject.strategy_version().to_string();
    let base_before = subject.strategy().base_evidence;

    let suggestion = subject
        .propose_strategy(ActionLevel::A1, at(9))
        .expect("提议")
        .expect("有一条错案够格");
    assert_eq!(suggestion.policy.base_evidence, base_before + 2);

    let admission = subject
        .admit_strategy(&suggestion, ActionLevel::A1, at(10))
        .expect("准入");
    assert!(!admission.is_admitted(), "会误伤的提议不该过闸");
    assert!(
        admission.report().would_block_right.contains(&thin.to_string()),
        "报告要指出被误伤的是哪一条：{:?}",
        admission.report()
    );

    // 而策略**一点没变**：拒绝不是"以后再试"，它连版本号都不该动。
    assert_eq!(subject.strategy().base_evidence, base_before);
    assert_eq!(subject.strategy_version().as_str(), version_before);
}

#[test]
fn a_paused_or_revoked_subject_learns_nothing_it_should_not() {
    // 保留期清理会把已经删除的记忆行物理删掉（§12.3 的第二步），于是错案只在"删除之后、
    // 清理之前"这段窗口里读得到。那是 §12.3 的取舍，不是这里的疏忽——但**清理之后系统就该
    // 没有什么可学的**，而不是拿一批空数据去提一个空改动。
    let mut subject = subject();
    delegate(&mut subject);
    let thin = record_one(&mut subject, 2);
    correct(&mut subject, &thin, 5);

    assert!(
        subject
            .propose_strategy(ActionLevel::A1, at(6))
            .expect("提议")
            .is_some(),
        "清理之前还读得到那条错案"
    );

    subject
        .enforce_retention(&soca_core::RetentionPolicy::default(), at(7))
        .expect("执行保留期");

    let holdout = subject.holdout(at(8)).expect("保留集");
    assert!(
        holdout.known_wrong.is_empty(),
        "清理之后错案已经不在记录里：{:?}",
        holdout.known_wrong
    );
    assert!(
        subject
            .propose_strategy(ActionLevel::A1, at(8))
            .expect("提议")
            .is_none(),
        "没有教材就不该提"
    );
}

#[test]
fn an_admitted_strategy_survives_a_checkpoint_into_the_snapshot() {
    // §13.2 的"版本化**启用**"要能跨重启。§9.2 说"跨重启恢复的是语义状态"，
    // 而"当时用的是哪一版判定标准"正是语义状态的一部分——所以它得落进单元快照，
    // 而不是只活在进程内存里。
    let mut subject = subject();
    delegate(&mut subject);
    let thin = record_one(&mut subject, 2);
    // 第二条只是为了让第一条成为"证据更少的那一条"。
    let _thick = record_one(&mut subject, 5);
    correct(&mut subject, &thin, 8);

    let suggestion = subject
        .propose_strategy(ActionLevel::A1, at(9))
        .expect("提议")
        .expect("应当提得出来");
    assert!(
        subject
            .admit_strategy(&suggestion, ActionLevel::A1, at(10))
            .expect("准入")
            .is_admitted()
    );

    // 取一次快照：它就是要落进 `units` 表的那一份。
    let snapshot = subject.cluster().snapshot();
    assert_eq!(
        snapshot.strategy_version.as_str(),
        subject.strategy_version().as_str(),
        "快照里要写着这一版——重启之后才能知道当时用的是哪一版"
    );
}
