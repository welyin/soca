//! §15.1 授权资料整理：整条端到端跑一遍。
//!
//! 规格把这一步写成它的验收场景，并且特意加了一句：
//!
//! > 这个任务同时验证最小单元、组合、LLM在环、OS权限、读后验证、**冷热驻留与恢复**；
//! > **不是"一个大prompt调用多个工具"就算完成**。
//!
//! 前半句是这份测试的提纲，后半句是它存在的理由。所以这里**逐步断言**，不写成
//! "跑一遍看最后对不对"：把七步压成一句话，恰好就变成了那句话说的东西。
//!
//! 三块还没有的部分，明确写在这里而不是让它们藏在"通过"后面：
//!
//! * **冷热驻留与恢复**（第 7 步的 checkpoint 与休眠）。单元生命周期有类型、有状态机，
//!   但主体还没有把它们接起来；这一份测试里也没有那一段。
//! * **预览**。§15.1 第 5 步要求"Broker显示预览，等待用户批准"——这里用
//!   "批准绑定到这份具体参数"替代了界面上的预览，而绑定是预览的**用法**，不是它的替身：
//!   真正的预览还要有人把内容展示出来。
//! * **遗漏检查**（第 3 步的"和遗漏"）。它需要一个"应当覆盖什么"的期望，而那是任务合同
//!   的事，本版还没有。
//!
//! 第 5 步那句"**文件已变化则失效草稿并重新核验**"已经有落点了，见
//! [`a_draft_is_invalidated_when_the_source_changes_under_it`]：它靠的是 `EvidenceFreshness`
//! 那条判定——"这条结论的依据里有没有最新的那条观测"。

use soca_contracts::{
    ActionLevel, Approval, ApprovalId, Candidate, CapabilityPolicyRef, DataClass, ExplorationQuota,
    GoalBudget, GoalId, ModelBackend, ModelBudget, ModelOutput, ModelProposal, ModelSelfReport,
    ModelVersion, OutputSchema, PermissionScope, SelectionPolicy, Sha256Hex,
    SubjectId, UserChannel, VerificationKind, Verdict, WallClock,
};
use soca_core::{
    ActionBroker, AdvanceStep, RetentionPolicy, RoundOutcome, SimulatedOs, Subject,
};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::{DeterministicTransport, Transport, TransportError};
use soca_storage::Store;

const CAP: &str = "cap:read-selected-folder";
/// §15.1 第 1 步："UI选择目录并生成范围授权。"
const SELECTED_DIR: &str = "file:D:\\资料\\摘要";
const SOURCE: &str = "file:D:\\资料\\摘要\\summary.md";
/// §15.1 第 4 步："预测单元记录计划写入的**文件名**、内容摘要和版本前提。"
const DRAFT_TARGET: &str = "file:D:\\资料\\摘要\\out.md";

/// 被整理的那份原始资料。
const SOURCE_BODY: &str = "资料摘要\n\n- 项目代号：晨星\n- 负责团队：系统组\n- 下一次评审：2026-10-15\n";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn owner() -> SubjectId {
    SubjectId::new("user:local").expect("固定主体")
}

fn cap(name: &str) -> CapabilityPolicyRef {
    CapabilityPolicyRef::new(name).expect("固定能力策略")
}

fn self_report() -> ModelSelfReport {
    ModelSelfReport {
        reported_value: 0.9,
        model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
        rationale: "引用都在正文里".to_string(),
    }
}

/// 装配一个能看见原始资料、并会照着它起草的主体。
///
/// 应答源读的是**它实际收到的上下文**，因此引用的证据是运行时生成的那一条——这正是
/// "模型只能回引它看到的东西"能被测到的原因（脚本式写法预先写不出 `obs:{uuid}`）。
fn subject() -> Subject {
    let mut os = SimulatedOs::new();
    os.seed(SOURCE, SOURCE_BODY);

    let transport: Box<dyn Transport> = Box::new(DeterministicTransport::from_fn(
        |request| -> Result<ModelOutput, TransportError> {
            let evidence = request
                .context
                .evidence
                .first()
                .expect("先观测过，上下文里必然有证据")
                .clone();
            Ok(ModelOutput {
                schema_version: soca_contracts::MODEL_OUTPUT_SCHEMA_VERSION,
                model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
                usage: soca_contracts::TokenUsage {
                    input_tokens: 120,
                    output_tokens: 40,
                },
                proposals: vec![ModelProposal {
                    candidate: Candidate::Claim {
                        statement: "资料摘要里说下一次评审是 2026-10-15".to_string(),
                        evidence_refs: vec![evidence.evidence_ref],
                    },
                    self_report: self_report(),
                    rationale: "按正文里的日期".to_string(),
                }],
                claims_finished: false,
            })
        },
    ));

    let cluster = DesktopAndFilesCluster::new(SOURCE, Vec::new()).expect("装配能力簇");
    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(os),
        cluster,
        owner(),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
        transport,
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 2048,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    )
    .expect("装配主体")
}

/// 一次写入动作的参数摘要，与 `Subject::request_write` 用的是同一条算式。
fn write_digest(subject_ref: &str, content: &str) -> Sha256Hex {
    let path = subject_ref.strip_prefix("file:").unwrap_or(subject_ref);
    let parameters = serde_json::json!({"path": path, "content": content});
    Sha256Hex::of_bytes(&serde_json::to_vec(&parameters).expect("可序列化"))
}

#[test]
fn the_authorized_summary_task_runs_end_to_end() {
    let mut subject = subject();

    // ---------------------------------------------------------------------
    // 第 1 步：UI选择目录并生成范围授权。
    // ---------------------------------------------------------------------
    let goal_id: GoalId = subject
        .delegate(
            "把我选的资料目录整理成一份摘要，保存前让我看",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: cap(CAP),
                max_action_level: ActionLevel::A2,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");

    // 授权是**范围限定**的（§12.1），而范围由守望对象所在的那一层派生。
    let granted = subject.granted_capabilities();
    assert_eq!(
        subject
            .policy()
            .grant_scope(granted[0])
            .expect("默认能力带范围")
            .prefixes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![SELECTED_DIR.to_string()]
    );

    // ---------------------------------------------------------------------
    // 第 2 步：文件单元读取授权文件的版本；事件账记录来源。
    // ---------------------------------------------------------------------
    let observed = subject
        .observe(SOURCE, DataClass::Personal, at(2))
        .expect("观测");
    let source_evidence = observed.observation.evidence_ref.clone();
    assert!(
        source_evidence.as_str().starts_with("obs:"),
        "证据引用指向那一条**事件**（§7.1）"
    );
    assert!(
        observed.observation.body_ref.is_some(),
        "正文也要进来——第 3 步的引用核对靠的是它"
    );

    // ---------------------------------------------------------------------
    // 第 3 步：LLM依据这些内容生成带引用草稿；核验单元检查引用存在与数字来源。
    // ---------------------------------------------------------------------
    let consultation = subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(3))
        .expect("咨询");
    assert_eq!(
        consultation.queued_candidates, 1,
        "草稿要成为候选，而不是只躺在返回值里（§6 第 3 步）"
    );
    assert_eq!(
        consultation.context.evidence[0].body.as_deref(),
        Some(SOURCE_BODY),
        "模型看到的必须是正文，不是版本摘要"
    );

    let (candidates, selection) = subject
        .select(&SelectionPolicy::default(), ActionLevel::A1, at(4))
        .expect("选择");
    let drafted = candidates
        .candidates
        .iter()
        .position(|candidate| {
            matches!(candidate, Candidate::Claim { statement, .. } if statement.contains("2026-10-15"))
        })
        .expect("草稿在候选里");
    let review = selection
        .reviews
        .iter()
        .find(|review| review.candidate_index == drafted)
        .expect("每条候选都要有档案");
    assert!(
        review.outcomes.iter().any(|outcome| {
            outcome.kind == VerificationKind::Tool && outcome.verdict == Verdict::Supported
        }),
        "草稿引用的就是正文里那个日期，依据核对应当支持：{review:?}"
    );

    // ---------------------------------------------------------------------
    // 第 4–5 步：记录预测与版本前提；提交 ActionIntent；等用户批准。
    // ---------------------------------------------------------------------
    let draft = "摘要\n\n- 项目代号：晨星\n- 下次评审：2026-10-15\n";
    subject
        .request_write(DRAFT_TARGET, draft, at(5))
        .expect("提交动作");

    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(6))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step: AdvanceStep::NeedsApproval { level, .. },
        } => assert_eq!(level, "A2", "§12.1：A2 的放行要求里含明确批准"),
        other => panic!("A2 写入应当停下来等批准，实际：{other:?}"),
    }
    assert_eq!(
        subject.store().action_count().expect("动作账"),
        0,
        "等待批准期间不该有任何动作记录"
    );

    // 批准绑定到**这一次具体动作**（§12.2 的参数摘要绑定）。这是预览的用法：
    // 用户看到的是具体的一次写入，批准的也应当是具体的那一次。
    let approval = Approval::new(
        ApprovalId::new("approval:write-draft").expect("固定审批"),
        owner(),
        ActionLevel::A2,
        UserChannel::ApprovalUi,
        at(6),
        None,
        1,
    )
    .expect("合法批准")
    .for_parameters(write_digest(DRAFT_TARGET, draft));
    subject.grant_approval(&approval, at(6)).expect("记下批准");

    // §12.1：审批期间目标停在 `WaitingApproval`。要接着做，必须**显式**把它放行。
    //
    // "显式"正是这一步的意义——`resume_after_approval` 只在目标确实处于等待审批时才成功，
    // 所以这次调用**本身**就是"刚才确实停了"的断言。把它藏进通用的状态迁移里，就等于给了
    // 调用方一条静悄悄绕过审批的路。
    subject
        .resume_after_approval(&goal_id, at(6))
        .expect("收到批准，恢复推进");

    // ---------------------------------------------------------------------
    // 第 6 步：批准后在授权目录创建文件，记录回执，再重新读取核对。
    // ---------------------------------------------------------------------
    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(7))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step:
                AdvanceStep::Action {
                    receipt, verdict, ..
                },
        } => {
            assert_eq!(receipt, "Completed", "回执不等于后置条件已验证（§7.2）");
            assert_eq!(
                verdict.as_deref(),
                Some("Supported"),
                "写完再读一遍，目标对象确实是预期的样子——这就是第 6 步的读后核验"
            );
        }
        other => panic!("批准之后应当执行，实际：{other:?}"),
    }
    assert_eq!(subject.store().action_count().expect("动作账"), 1);

    // ---------------------------------------------------------------------
    // 第 7 步：只保存授权的任务摘要和必要审计。
    // ---------------------------------------------------------------------
    //
    // 先让第 3 步那份草稿走完它自己的路。**它前面还有一条**：簇自己提的那条
    // "……的版本是……"也是候选，两条各有一条证据，并列时按出现顺序取——簇的子单元排在
    // 前面，所以第一轮记的是它。第二条要等到下一轮（推过的结论进了 `handled_claims`，
    // 不会再被提出来）。
    //
    // 这不是巧合，是 §6 第 2 步那个"路由器"在起作用：没有它，第二轮还会选同一条。
    for round in 0..3 {
        subject
            .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(8 + round))
            .expect("跑一轮");
    }
    let recalled = subject.store().recall(&owner(), None, at(12)).expect("召回");
    assert!(
        recalled
            .iter()
            .any(|entry| entry.claim.contains("2026-10-15")),
        "第 3 步那份带引用的草稿要落进 L5——它就是这次任务产出的结论：{recalled:?}"
    );
    let before_retention = recalled.len();

    // §12.3 的原话是"最小审计账"。所以这里断言的是**哪些东西会随保留期走**，
    // 而不是"跑完之后什么都没了"：对话原文到期退休，而结论与审计留下。
    let report = subject
        .enforce_retention(&RetentionPolicy::default(), at(400 * 86_400))
        .expect("执行保留期");
    assert!(
        !report.retired_content.is_empty(),
        "对话原文到期退休：{report:?}"
    );
    assert_eq!(
        subject.store().memory_count(&owner()).expect("记忆数"),
        before_retention,
        "而授权的任务摘要留着"
    );
}

#[test]
fn a_draft_is_invalidated_when_the_source_changes_under_it() {
    // §15.1 第 5 步的后半句："在此期间，相关单元可温存；**文件已变化则失效草稿并重新核验**。"
    //
    // 场景是：起草之后、保存之前，原文被改了。此时那份草稿依据的是一个已经不存在的版本，
    // 而它**看起来完全正常**——引用合法、依据核对通过（那个日期确实在它引用的那份旧正文里）。
    // 只有把"这份材料是不是最新的"单独核一遍才发现得了。
    //
    // 让世界改变的办法是**写一次原文**：那是一次真实的副作用，走的也是读取—许可—执行—回执
    // 那条路。而写进去的内容**保留同一个日期**，这样依据核对仍然通过——于是这条测试验的
    // 确实是新鲜度，而不是顺带被"数字来源"那条挡下了。
    let mut subject = subject();
    let goal_id = subject
        .delegate(
            "整理摘要",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: cap(CAP),
                max_action_level: ActionLevel::A2,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");

    subject
        .observe(SOURCE, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(3))
        .expect("起草");

    // 原文被改：内容里仍有那个日期，但版本变了。
    let revised = format!("{SOURCE_BODY}- 补充：会后追加一行\n");
    subject
        .request_write(SOURCE, &revised, at(4))
        .expect("递交写入");
    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(5))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step: AdvanceStep::NeedsApproval { .. },
        } => {}
        other => panic!("A2 写原文应当先等批准，实际：{other:?}"),
    }
    let approval = Approval::new(
        ApprovalId::new("approval:revise").expect("固定审批"),
        owner(),
        ActionLevel::A2,
        UserChannel::ApprovalUi,
        at(5),
        None,
        1,
    )
    .expect("合法批准")
    .for_parameters(write_digest(SOURCE, &revised));
    subject.grant_approval(&approval, at(5)).expect("记下批准");
    subject
        .resume_after_approval(&goal_id, at(5))
        .expect("恢复");
    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(6))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step: AdvanceStep::Action { verdict, .. },
        } => assert_eq!(verdict.as_deref(), Some("Supported"), "原文确实被改了"),
        other => panic!("实际：{other:?}"),
    }

    // 再观测一次：同一个对象上出现了一条**更晚**的观测。
    subject
        .observe(SOURCE, DataClass::Personal, at(7))
        .expect("重新观测");

    let (candidates, selection) = subject
        .select(&SelectionPolicy::default(), ActionLevel::A1, at(8))
        .expect("选择");
    let drafted = candidates
        .candidates
        .iter()
        .position(|candidate| {
            matches!(candidate, Candidate::Claim { statement, .. } if statement.contains("2026-10-15"))
        })
        .expect("草稿还在候选里");
    let review = selection
        .reviews
        .iter()
        .find(|review| review.candidate_index == drafted)
        .expect("有档案");

    assert!(
        review.outcomes.iter().any(|outcome| {
            outcome.kind == VerificationKind::EvidenceFreshness
                && outcome.verdict == Verdict::Refuted
        }),
        "原文已经变了，草稿要失效——而且报的是**过期**而不是别的毛病：{review:?}"
    );
    assert!(review.is_refuted());
}

#[test]
fn a_draft_whose_date_is_not_in_the_source_is_never_written() {
    // 上一条的正反面配对。少了它，"整条链跑通了"与"整条链什么都没做"分不开——
    // 而一条只会走完整流程、不会否定的通路，看起来和一条正确的通路一模一样。
    //
    // 这里模型写错了日期：正文说 2026-10-15，草稿说 2026-12-01。
    let mut os = SimulatedOs::new();
    os.seed(SOURCE, SOURCE_BODY);
    let transport: Box<dyn Transport> = Box::new(DeterministicTransport::from_fn(|request| {
        let evidence = request.context.evidence.first().expect("有证据").clone();
        Ok(ModelOutput {
            schema_version: soca_contracts::MODEL_OUTPUT_SCHEMA_VERSION,
            model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
            usage: soca_contracts::TokenUsage {
                input_tokens: 120,
                output_tokens: 40,
            },
            proposals: vec![ModelProposal {
                candidate: Candidate::Claim {
                    statement: "资料摘要里说下一次评审是 2026-12-01".to_string(),
                    evidence_refs: vec![evidence.evidence_ref],
                },
                self_report: self_report(),
                rationale: "按正文".to_string(),
            }],
            claims_finished: false,
        })
    }));

    let cluster = DesktopAndFilesCluster::new(SOURCE, Vec::new()).expect("装配能力簇");
    let mut subject = Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(os),
        cluster,
        owner(),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
        transport,
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 2048,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    )
    .expect("装配主体");

    let goal_id = subject
        .delegate(
            "整理摘要",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: cap(CAP),
                max_action_level: ActionLevel::A2,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");
    subject
        .observe(SOURCE, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(3))
        .expect("咨询");

    let (candidates, selection) = subject
        .select(&SelectionPolicy::default(), ActionLevel::A1, at(4))
        .expect("选择");
    let drafted = candidates
        .candidates
        .iter()
        .position(|candidate| {
            matches!(candidate, Candidate::Claim { statement, .. } if statement.contains("2026-12-01"))
        })
        .expect("草稿在候选里");
    assert!(
        selection
            .reviews
            .iter()
            .find(|review| review.candidate_index == drafted)
            .expect("有档案")
            .is_refuted(),
        "日期在正文里根本没有，这条草稿不该算数"
    );

    // 而它也不会变成记忆：跑多少轮都推不动它。
    for round in 0..3 {
        subject
            .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(5 + round))
            .expect("跑一轮");
    }
    let recalled = subject
        .store()
        .recall(&owner(), None, at(9))
        .expect("召回");
    assert!(
        recalled
            .iter()
            .all(|entry| !entry.claim.contains("2026-12-01")),
        "被否定的草稿不该进 L5：{recalled:?}"
    );
}
