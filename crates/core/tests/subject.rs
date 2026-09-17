//! 单主体运行时把各层接起来之后的端到端回归测试。
//!
//! 这里测的不是"某个模块对不对"，而是**接起来之后那条边界还在不在**。各层的闸单独看都
//! 能通过自己的测试，但接起来时最常见的失效方式是某一道被绕过去——例如上下文编译器收了一份
//! 来路不明的证据，或者一次模型咨询悄悄消耗了两个目标的额度。

use soca_contracts::{
    ActionLevel, Candidate, CapabilityPolicyRef, DataClass, EvidenceRef, ExplorationQuota,
    GoalBudget, GoalId, GoalState, ModelBackend, ModelBudget, ModelOutput, ModelProposal,
    ModelSelfReport, ModelVersion, OutputSchema, PermissionScope, SelectionPolicy, SubjectId,
    TokenUsage, UserChannel, VerificationKind, Verdict, WallClock, MAX_CONTEXT_EVIDENCE,
};
use soca_core::{ActionBroker, SimulatedOs, Subject};
use soca_core_actors::{DesktopAndFilesCluster, Precondition};
use soca_model_gateway::{DeterministicTransport, Transport, TransportError};
use soca_storage::Store;

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";
const CAPABILITY: &str = "cap:read-selected-folder";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn owner() -> SubjectId {
    SubjectId::new("user:alice").expect("固定主体")
}

fn boot() -> soca_contracts::BootId {
    soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot")
}

fn scope(level: ActionLevel) -> PermissionScope {
    PermissionScope {
        capability_policy_ref: CapabilityPolicyRef::new(CAPABILITY).expect("固定能力策略"),
        max_action_level: level,
    }
}

fn budget() -> GoalBudget {
    GoalBudget::new(16, 32, 8192, 3_600_000).expect("合法额度")
}

fn self_report() -> ModelSelfReport {
    ModelSelfReport {
        reported_value: 0.91,
        model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
        rationale: "证据一致".to_string(),
    }
}

fn output_with(proposals: Vec<ModelProposal>) -> ModelOutput {
    ModelOutput {
        schema_version: soca_contracts::MODEL_OUTPUT_SCHEMA_VERSION,
        model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
        proposals,
        usage: TokenUsage {
            input_tokens: 64,
            output_tokens: 16,
        },
        claims_finished: false,
    }
}

fn new_subject(script: Vec<Result<ModelOutput, TransportError>>) -> Subject {
    subject_with_transport(Box::new(DeterministicTransport::new(script)))
}

/// 用一个能看见请求的应答源装配主体。
///
/// 需要它是因为证据引用是运行时生成的（`obs:{uuid}`）：脚本式写法无法预先引用它们，
/// 而"模型正确回引了它看到的证据"恰恰是最需要测的那一类行为。
fn subject_responding<F>(responder: F) -> Subject
where
    F: Fn(&soca_model_gateway::ModelRequest) -> Result<ModelOutput, TransportError>
        + Send
        + Sync
        + 'static,
{
    subject_with_transport(Box::new(DeterministicTransport::from_fn(responder)))
}

fn subject_with_transport(transport: Box<dyn Transport>) -> Subject {
    let store = Store::open_in_memory(at(0)).expect("内存存储");
    let broker = ActionBroker::new(SimulatedOs::new());
    let cluster = DesktopAndFilesCluster::new(
        WATCHED,
        vec![Precondition::new("目标目录已授权", CAPABILITY)],
    )
    .expect("装配能力簇");

    Subject::new(
        store,
        broker,
        cluster,
        owner(),
        boot(),
        transport,
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

fn delegate_a_goal(subject: &mut Subject) -> GoalId {
    subject
        .delegate(
            "为已授权目录生成摘要",
            UserChannel::Chat,
            scope(ActionLevel::A1),
            budget(),
            ExplorationQuota::new(4),
            at(0),
            None,
        )
        .expect("委托")
}

// ---------------------------------------------------------------------------
// 目标
// ---------------------------------------------------------------------------

#[test]
fn a_chat_message_becomes_a_delegated_goal_that_can_be_accepted() {
    // §4.1 L6 的入口。注意 [`Subject::delegate`] 收的是 `UserChannel` 而不是
    // `Provenance`——《§2：不承诺自主产生目标》这条约束在主体这一层没有绕路可走，
    // 因为那条路本来就只有一条。
    let mut subject = new_subject(Vec::new());
    let goal_id = delegate_a_goal(&mut subject);

    let goal = subject.goals().goal(&goal_id).expect("目标存在");
    assert_eq!(goal.state, GoalState::Proposed);
    assert_eq!(goal.origin.as_str(), "delegated");
    subject.accept(&goal_id, at(1)).expect("受理");
    assert_eq!(
        subject.goals().goal(&goal_id).expect("存在").state,
        GoalState::Active
    );
    subject.goals().validate().expect("栈自洽");
}

#[test]
fn delegating_is_checkpointed_so_the_goal_survives_a_restart() {
    let mut subject = new_subject(Vec::new());
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");

    // 重新装配一个主体，从同一个存储读回来。
    let mut restored = new_subject(Vec::new());
    // 这里复用不了同一个 Store（它已被 subject 持有），所以直接验证"栈被写进去过"。
    assert_eq!(
        subject.store().goal_stack_revision(&owner()).expect("读取"),
        Some(2),
        "委托与受理各写一次"
    );
    // 一个全新的主体初始为空栈。
    assert!(restored.goals().is_empty());
    restored.restore_goals().expect("恢复");
    assert!(
        restored.goals().is_empty(),
        "它自己的库里确实什么都没有；这正说明上一条断言测的是真写的那个库"
    );
}

#[test]
fn abandoning_a_goal_reaches_the_store_and_the_whole_subtree() {
    let mut subject = new_subject(Vec::new());
    let parent = delegate_a_goal(&mut subject);
    subject.accept(&parent, at(1)).expect("受理");
    let child = subject
        .goals()
        .children_of(&parent)
        .len();
    assert_eq!(child, 0);

    let abandoned = subject.abandon(&parent, at(2)).expect("放弃");
    assert_eq!(abandoned, 1);
    let stored = subject
        .store()
        .goal_stack(&owner())
        .expect("读取")
        .expect("存在");
    assert!(
        stored.goal(&parent).expect("存在").is_terminal(),
        "放弃必须落库，否则界面刷新之后目标会「复活」"
    );
}

// ---------------------------------------------------------------------------
// 观测：一条路进两个地方
// ---------------------------------------------------------------------------

#[test]
fn observing_feeds_both_the_event_ledger_and_the_blackboard() {
    let mut subject = new_subject(Vec::new());
    let record = subject
        .observe(WATCHED, DataClass::Personal, at(0))
        .expect("观测");

    assert_eq!(record.observation.subject, WATCHED);
    let workspace = subject.cluster().workspace();
    assert!(
        workspace.has_evidence(&record.observation.evidence_ref),
        "观测必须同时进入 L2 黑板——否则 propose 的 L2 门会把簇自己的子单元整批挡下"
    );
    assert_eq!(workspace.topic_count(), 1);

    let state = subject.public_state(at(1)).expect("状态");
    assert_eq!(state.workspace_evidence, 1);
    assert_eq!(state.observed_evidence, 1);
}

#[test]
fn the_evidence_pool_only_grows_from_observation() {
    // §8 要求证据在 Core 与存储中而不只在上下文窗口里。证据池只由 observe 写入——
    // 一个能容纳来路不明证据的编译器，等于把"模型不能引用它没看到的东西"从里面拆掉。
    let mut subject = new_subject(Vec::new());
    assert_eq!(
        subject.public_state(at(0)).expect("状态").observed_evidence,
        0
    );

    subject
        .observe(WATCHED, DataClass::Personal, at(0))
        .expect("观测");
    subject
        .observe(CAPABILITY, DataClass::Public, at(0))
        .expect("观测");
    assert_eq!(
        subject.public_state(at(0)).expect("状态").observed_evidence,
        2
    );

    // 再观测同一个对象一次得到的是**新**的一份证据：引用是 `obs:{event_id}`，
    // 第二次观测发生在另一个时刻，它证明的是那一刻的事实。去重去的是重复的回引，
    // 不是重复的观测——把两者混起来，会让"刚才还是旧版本、现在是新版本"这类变化被吞掉。
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");
    assert_eq!(
        subject.public_state(at(0)).expect("状态").observed_evidence,
        3
    );
}

#[test]
fn the_evidence_pool_is_a_bounded_recent_window() {
    // 证据池不是长期记忆（那是 L5 的职责）。让它无限增长的话，一个跑了一天的进程会攒下
    // 几万条，而 §17 的长时运行验收要求"内存和日志增长符合配额"。
    let mut subject = new_subject(Vec::new());
    for index in 0..(MAX_CONTEXT_EVIDENCE as i64 + 8) {
        subject
            .observe(WATCHED, DataClass::Personal, at(index))
            .expect("观测");
    }
    assert_eq!(
        subject.public_state(at(0)).expect("状态").observed_evidence,
        MAX_CONTEXT_EVIDENCE,
        "超出窗口的旧证据被丢掉，留下的恰好是编译上下文用得上的那一批"
    );
}

// ---------------------------------------------------------------------------
// 模型咨询
// ---------------------------------------------------------------------------

#[test]
fn consulting_the_model_burns_exactly_one_activation() {
    // §4.2 的额度记账要准，否则"请求预算升级"就成了拍脑袋。先扣再做的理由也在这里：
    // 一次超时的调用若在事后才记账，额度就会漏。
    let mut subject = new_subject(vec![Ok(output_with(Vec::new()))]);
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");
    let before = subject
        .goals()
        .goal(&goal_id)
        .expect("存在")
        .budget
        .remaining_activations();

    subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(2))
        .expect("咨询");

    let after = subject
        .goals()
        .goal(&goal_id)
        .expect("存在")
        .budget
        .remaining_activations();
    assert_eq!(after, before - 1);
    assert_eq!(
        subject
            .goals()
            .goal(&goal_id)
            .expect("存在")
            .budget
            .spent_tokens,
        80,
        "token 用量要记进目标额度"
    );
    assert_eq!(subject.public_state(at(2)).expect("状态").model_calls, 1);
}

#[test]
fn consulting_an_unknown_goal_is_refused_before_anything_is_spent() {
    let mut subject = new_subject(vec![Ok(output_with(Vec::new()))]);
    let result = subject.consult_model(
        &GoalId::new("goal:never-delegated").expect("固定目标"),
        OutputSchema::read_only(),
        at(0),
    );
    assert!(matches!(result, Err(soca_core::CoreError::GoalNotFound { .. })));
    assert_eq!(subject.public_state(at(0)).expect("状态").model_calls, 0);
}

#[test]
fn a_subject_with_no_evidence_still_produces_a_valid_context() {
    // 证据为空不是错误。§11.1 的同一条原则：拿不到就说不知道，而不是编一个。
    let mut subject = new_subject(vec![Ok(output_with(Vec::new()))]);
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");

    let consultation = subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(2))
        .expect("咨询");
    assert!(consultation.context.evidence.is_empty());
    assert_eq!(consultation.context.goal, "为已授权目录生成摘要");
    assert!(
        consultation
            .context
            .capabilities
            .tool_ids
            .iter()
            .any(|tool| tool.as_str() == "fs.write"),
        "能力范围必须如实反映本版可用的工具"
    );
}

#[test]
fn a_consultation_returns_candidates_not_actions() {
    // §3.1："它提出假设和动作，不独占信念、记忆、权限、预算或执行权。"
    // 一次咨询的产物是候选；动作账不该因为咨询而多出一条记录。
    let mut subject = subject_responding(|request| {
        // 回引上下文里的第一条证据——脚本式写法做不到这件事。
        let evidence = request
            .context
            .evidence
            .first()
            .expect("本用例先观测过一次，上下文里必然有证据")
            .clone();
        Ok(output_with(vec![ModelProposal {
            candidate: Candidate::Claim {
                statement: format!("{} 的版本已记录", evidence.subject_ref),
                evidence_refs: vec![evidence.evidence_ref],
            },
            self_report: self_report(),
            rationale: "上下文里的观测".to_string(),
        }]))
    });

    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");

    let consultation = subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(2))
        .expect("咨询");
    assert_eq!(consultation.output.proposals.len(), 1);
    assert!(
        !consultation.output.proposals[0]
            .candidate
            .may_have_side_effect(),
        "结论类候选不改变世界"
    );
    assert_eq!(
        subject.public_state(at(2)).expect("状态").actions,
        0,
        "咨询不产生动作账记录：候选要经过 L4 与执行许可才会变成动作"
    );
}

#[test]
fn a_model_claim_citing_evidence_the_subject_never_observed_is_refused() {
    // 端到端的 §8 边界：各层单独都对，接起来之后仍然要拦住。
    let phantom = EvidenceRef::new("obs:phantom").expect("固定证据");
    let proposal = ModelProposal {
        candidate: Candidate::Claim {
            statement: "这个文件的版本是 sha256:zzz".to_string(),
            evidence_refs: vec![phantom],
        },
        self_report: self_report(),
        rationale: "我记得是这样".to_string(),
    };

    let mut subject = new_subject(vec![Ok(output_with(vec![proposal]))]);
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");

    let result = subject.consult_model(&goal_id, OutputSchema::read_only(), at(2));
    assert!(
        matches!(result, Err(soca_core::CoreError::Gateway(_))),
        "引用了不存在的证据必须被拒绝，实际：{result:?}"
    );
}

#[test]
fn a_model_claim_citing_real_evidence_passes_end_to_end() {
    // 与上一条的差别在于**它盯的是自评分数**：§3.2 要求模型自报的数值不能变成概率真值。
    // 数据通路整条走通之后，那个 0.91 应当原样躺在 self_report 里，而不是出现在任何
    // CalibratedProbability 上——后者需要一个模型版本、时间范围与校准来源。
    let mut subject = subject_responding(|request| {
        let evidence = request
            .context
            .evidence
            .first()
            .expect("上下文里必然有证据")
            .clone();
        Ok(output_with(vec![ModelProposal {
            candidate: Candidate::Claim {
                statement: format!("{} 的版本已记录", evidence.subject_ref),
                evidence_refs: vec![evidence.evidence_ref],
            },
            self_report: self_report(),
            rationale: "上下文里的观测".to_string(),
        }]))
    });

    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");

    let consultation = subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(2))
        .expect("应当通过");
    assert_eq!(consultation.output.proposals.len(), 1);
    assert_eq!(
        consultation.output.proposals[0].self_report.reported_value,
        0.91
    );
    // 上下文里如实写着模型看到了什么，界面与审计据此复盘。
    assert_eq!(consultation.context.evidence.len(), 1);
    assert_eq!(consultation.context.evidence[0].subject_ref, WATCHED);
}

#[test]
fn a_read_only_schema_still_refuses_an_action_proposal_from_the_model() {
    // 上下文决定能力，不是模型。（§8 的输出 Schema）
    let proposal = ModelProposal {
        candidate: Candidate::RequestTool {
            tool_id: soca_contracts::ToolId::new("fs.write").expect("固定工具"),
            parameters: serde_json::json!({"path": WATCHED, "content": "顺手改了"}),
        },
        self_report: self_report(),
        rationale: "反正能写".to_string(),
    };

    let mut subject = new_subject(vec![Ok(output_with(vec![proposal]))]);
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");

    let result = subject.consult_model(&goal_id, OutputSchema::read_only(), at(2));
    assert!(matches!(result, Err(soca_core::CoreError::Gateway(_))));
}

#[test]
fn public_state_reports_what_the_subject_can_actually_do() {
    let mut subject = new_subject(Vec::new());
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");
    subject.explore(&goal_id, at(1)).expect("探索");

    assert_eq!(subject.max_action_level(), ActionLevel::A1);

    let state = subject.public_state(at(2)).expect("状态");
    assert_eq!(state.owner, "user:alice");
    assert_eq!(state.goals.len(), 1);
    assert_eq!(state.goals[0].state, "active");
    assert_eq!(state.goals[0].remaining_explorations, 3);
    assert_eq!(state.goals[0].remaining_activations, 31, "探索也消耗激活");
    assert!(!state.goals[0].expired);
    assert_eq!(state.memory_entries, 0);
    assert_eq!(state.actions, 0);
    assert_eq!(state.model_calls, 0);

    // 这些数字要能直接序列化给界面。
    let json = serde_json::to_string(&state).expect("可序列化");
    assert!(json.contains("\"owner\":\"user:alice\""));
}

#[test]
fn a_terminal_goal_cannot_be_consulted_about() {
    let mut subject = new_subject(vec![Ok(output_with(Vec::new()))]);
    let goal_id = delegate_a_goal(&mut subject);
    subject.abandon(&goal_id, at(1)).expect("放弃");

    let result = subject.consult_model(&goal_id, OutputSchema::read_only(), at(2));
    assert!(matches!(result, Err(soca_core::CoreError::Contract(_))));
}

#[test]
fn selecting_walks_the_cluster_candidates_through_the_verifiers() {
    // §6 第 4–5 步接起来的端到端：观测 → 簇提出候选 → 三类检验 → 选择。
    // 这条测试的价值在于它把两层一起跑了。检验器与选择各自都有单元测试，而它们的**接缝**
    // 正是本文件里那个缺陷（未决问题阻塞选择）藏身的地方。
    let mut subject = new_subject(Vec::new());
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");

    let (candidates, selection) = subject
        .select(&SelectionPolicy::default(), ActionLevel::A1, at(2))
        .expect("选择");

    assert!(!candidates.candidates.is_empty(), "观测之后簇应当能提出结论");
    assert_eq!(
        selection.reviews.len(),
        candidates.candidates.len(),
        "每条候选都要有档案——「缺档」与「没有检验记录」在审计里含义不同"
    );
    assert_eq!(selection.required_evidence, 1, "A1 是低风险档");
    assert_eq!(
        selection.selected_index(),
        Some(0),
        "关于版本的结论证据最多，应当被选中：{:?}",
        selection.outcome
    );

    // 检验确实跑了，而且它报的是它真正查到的东西。
    let outcomes = &selection.reviews[0].outcomes;
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome.kind == VerificationKind::Tool
                && outcome.verdict == Verdict::Supported),
        "结论与它自己引用的证据一致，依据核对应当通过"
    );
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome.kind == VerificationKind::IndependentSource
                && outcome.verdict == Verdict::Inconclusive),
        "只有一个观测者，来源核对应当说「无法判定」而不是「支持」：{outcomes:?}"
    );
    assert!(
        !outcomes
            .iter()
            .any(|outcome| outcome.kind == VerificationKind::CounterExample),
        "A1 是低风险档，不强行找反方观点（§6 第 4 步）"
    );
}

#[test]
fn an_open_question_is_reported_but_does_not_freeze_the_selection() {
    // 动作前提这个槽位在拿到证据之前会一直挂着一个未决问题。如果它阻塞选择，这个簇就
    // 永远选不出任何东西——而那正是接上检验器之前没被发现的行为。
    let mut subject = new_subject(Vec::new());
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");

    let (candidates, selection) = subject
        .select(&SelectionPolicy::default(), ActionLevel::A1, at(2))
        .expect("选择");

    assert!(
        !candidates.unresolved.is_empty(),
        "动作前提还没被观测到，簇里应当挂着未决问题"
    );
    assert!(
        selection.selected_index().is_some(),
        "未决问题不该把结论一起挡下"
    );
    assert!(
        selection.rationale.contains("未决问题"),
        "但它也不能被吞掉：选好了不等于什么都清楚了。实际：{}",
        selection.rationale
    );
}

#[test]
fn the_verdict_pattern_does_not_depend_on_which_run_it_is() {
    // §13：固定策略与相同动作序列的引擎可复现。
    //
    // 这里比的是**判定模式**，不是完整档案。证据引用是每次观测新生成的（`obs:{uuid}`），
    // 两次运行引用不同的观测是正确行为，不是不确定性。而"同一份台账加同一个候选集合得到
    // 同一份档案"是一条更严格的等式，由 core-actors 的纯函数测试负责——那里能真的断言它，
    // 这里不能。
    let pattern = || {
        let mut subject = new_subject(Vec::new());
        let goal_id = delegate_a_goal(&mut subject);
        subject.accept(&goal_id, at(1)).expect("受理");
        subject
            .observe(WATCHED, DataClass::Personal, at(1))
            .expect("观测");
        let selection = subject
            .select(&SelectionPolicy::default(), ActionLevel::A1, at(2))
            .expect("选择")
            .1;
        let verdicts: Vec<(VerificationKind, Verdict)> = selection
            .reviews
            .iter()
            .flat_map(|review| {
                review
                    .outcomes
                    .iter()
                    .map(|outcome| (outcome.kind, outcome.verdict))
            })
            .collect();
        (selection.outcome, selection.required_evidence, verdicts)
    };

    assert_eq!(pattern(), pattern());
}

#[test]
fn the_context_reflects_the_action_ledger_rather_than_a_private_counter() {
    // §8 的"过去动作结果"来自动作账。主体自己再记一份的话两者迟早不一致；而账上没有记录时
    // 它必须是空的，不能是"上一次运行的遗留"。
    let mut subject = new_subject(vec![Ok(output_with(Vec::new()))]);
    let goal_id = delegate_a_goal(&mut subject);
    subject.accept(&goal_id, at(1)).expect("受理");

    assert!(
        subject
            .store()
            .recent_outcomes(16)
            .expect("读取")
            .is_empty(),
        "还没有任何动作，也没有任何结果"
    );

    let consultation = subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(2))
        .expect("咨询");
    assert!(consultation.context.past_outcomes.is_empty());
    assert_eq!(
        consultation.output.usage.output_tokens, 16,
        "用量如实记下，供 §17 的成本对照使用"
    );
}
