//! §8 上下文编译与模型网关的回归测试。
//!
//! 两组测试分别针对两条最容易在实现里走形的边界：
//!
//! * **私人内容不得出站**（编译期拒绝，而不是发送前过滤）；
//! * **模型不能引用它没看到的证据，也不能自己签发预测引用**。

use serde_json::json;
use soca_contracts::{
    ActionId, ActionIntent, ActionLevel, BeliefSummary, Candidate, CandidateKind, CapabilitySlice,
    ContextBundle, ContractError, DataClass, EvidenceRef, EvidenceSlice, ModelBackend, ModelBudget,
    ModelOutput, ModelProposal, ModelSelfReport, ModelVersion, OutputSchema, PredictionRef,
    ResourceCost, ResourceScope, TokenUsage, ToolId, UnitId, WallClock,
};
use soca_model_gateway::{
    ContextCompiler, ContextInput, DeterministicTransport, GatewayError, ModelGateway,
    TransportError, ValidatedOutput,
};

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn evidence_ref(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn slice(name: &str, subject: &str, value: &str, class: DataClass) -> EvidenceSlice {
    EvidenceSlice {
        evidence_ref: evidence_ref(name),
        subject_ref: subject.to_string(),
        observed_value: value.to_string(),
        data_class: class,
    }
}

fn belief(statement: &str, names: &[&str]) -> BeliefSummary {
    BeliefSummary {
        statement: statement.to_string(),
        evidence_refs: names.iter().map(|name| evidence_ref(name)).collect(),
    }
}

fn capabilities() -> CapabilitySlice {
    CapabilitySlice {
        tool_ids: vec![ToolId::new("fs.read").expect("固定工具")],
        max_action_level: ActionLevel::A1,
    }
}

fn input(evidence: Vec<EvidenceSlice>, beliefs: Vec<BeliefSummary>) -> ContextInput {
    ContextInput {
        goal: "为已授权目录生成摘要".to_string(),
        evidence,
        belief: beliefs,
        past_outcomes: Vec::new(),
        capabilities: capabilities(),
        deadline: at(600),
        output_schema: OutputSchema::full(),
        recorded_predictions: vec![PredictionRef::new("prediction:1").expect("固定预测")],
    }
}

fn self_report() -> ModelSelfReport {
    ModelSelfReport {
        // §3.2：这不是概率真值。下面一条测试会盯着它不被当成概率使用。
        reported_value: 0.97,
        model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
        rationale: "证据看起来一致".to_string(),
    }
}

fn proposal(candidate: Candidate, rationale: &str) -> ModelProposal {
    ModelProposal {
        candidate,
        self_report: self_report(),
        rationale: rationale.to_string(),
    }
}

fn output(proposals: Vec<ModelProposal>) -> ModelOutput {
    ModelOutput {
        schema_version: soca_contracts::MODEL_OUTPUT_SCHEMA_VERSION,
        model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
        proposals,
        usage: TokenUsage {
            input_tokens: 128,
            output_tokens: 32,
        },
        claims_finished: false,
    }
}

fn budget(max_attempts: u8) -> ModelBudget {
    ModelBudget {
        max_output_tokens: 1024,
        max_wall_millis: 30_000,
        max_attempts,
    }
}

fn gateway(
    transport: DeterministicTransport,
    backend: ModelBackend,
    remote_authorized: bool,
    budget: ModelBudget,
) -> ModelGateway<DeterministicTransport> {
    ModelGateway::new(
        transport,
        backend,
        remote_authorized,
        budget,
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    )
    .expect("预算合法")
}

fn invoke(
    gateway: &mut ModelGateway<DeterministicTransport>,
    context: &ContextBundle,
) -> Result<ValidatedOutput, GatewayError> {
    gateway.invoke(
        context,
        soca_contracts::ModelProfileRef::new("profile:reasoning").expect("固定画像"),
    )
}

fn write_intent(prediction: &str) -> Candidate {
    Candidate::RequestAction {
        intent: Box::new(
            ActionIntent::new(
                ActionId::new("action:1").expect("固定动作"),
                ToolId::new("fs.write").expect("固定工具"),
                ResourceScope::new("D:\\资料\\摘要").expect("固定范围"),
                json!({"path": "D:\\资料\\摘要\\summary.md", "content": "新的摘要内容"}),
                vec!["目标目录已授权".to_string()],
                PredictionRef::new(prediction).expect("固定预测引用"),
                ActionLevel::A1,
                ResourceCost {
                    est_ram_bytes: 0,
                    est_tokens: 0,
                    est_millis: 10,
                },
                UnitId::new("unit:file-summary:07").expect("固定单元"),
            )
            .expect("合法动作意图"),
        ),
    }
}

// ---------------------------------------------------------------------------
// 上下文编译
// ---------------------------------------------------------------------------

#[test]
fn a_compiled_context_carries_the_seven_things_section_8_lists() {
    // §8 逐项点名：当前目标、可用证据引用、局部 belief 摘要、过去动作结果、能力范围、
    // 截止时间与输出 Schema。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(
            vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
            vec![belief("摘要文件已经更新", &["1"])],
        ))
        .expect("编译");

    assert_eq!(context.goal, "为已授权目录生成摘要");
    assert_eq!(context.evidence.len(), 1);
    assert_eq!(context.belief.len(), 1);
    assert!(context.past_outcomes.is_empty());
    assert_eq!(context.capabilities.tool_ids.len(), 1);
    assert_eq!(context.deadline, at(600));
    assert_eq!(context.output_schema.version, 1);
    assert!(context.validate().is_ok());
}

#[test]
fn evidence_cited_by_belief_is_never_dropped_by_the_quota() {
    // 配额只用来裁剪**没被引用**的部分。丢掉被引用的那一条，整份包就不成立。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false).with_max_evidence(1);
    let context = compiler
        .compile(input(
            vec![
                slice("cited", "file:summary.md", "sha256:aaa", DataClass::Personal),
                slice("extra-1", "file:a.md", "sha256:b", DataClass::Personal),
                slice("extra-2", "file:b.md", "sha256:c", DataClass::Personal),
            ],
            vec![belief("摘要文件已经更新", &["cited"])],
        ))
        .expect("编译");

    assert_eq!(context.evidence.len(), 1);
    assert!(context.knows(&evidence_ref("cited")));
    assert!(!context.knows(&evidence_ref("extra-1")));
}

#[test]
fn compilation_fails_rather_than_dropping_cited_evidence() {
    // 被引用的证据挤不进配额时，编译**失败**，而不是悄悄丢掉它们。丢掉引用等于让模型
    // 看到一个缺少依据的结论——错误的形式变了，错误的量没变。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false).with_max_evidence(1);
    let result = compiler.compile(input(
        vec![
            slice("a", "file:summary.md", "sha256:aaa", DataClass::Personal),
            slice("b", "file:a.md", "sha256:bbb", DataClass::Personal),
        ],
        vec![
            belief("第一条", &["a"]),
            belief("第二条", &["b"]),
        ],
    ));

    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::ContextLimitExceeded {
            field: "context.evidence(被引用的部分)",
            ..
        }))
    ));
}

#[test]
fn a_belief_citing_evidence_that_was_never_supplied_is_refused() {
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let result = compiler.compile(input(
        vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
        vec![belief("依据是一条我没给它的观测", &["ghost"])],
    ));

    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::EvidenceNotInContext { .. }))
    ));
}

#[test]
fn the_evidence_quota_cannot_be_raised_above_the_contract_limit() {
    // 契约层的硬上限是上下文有界性的最后一道。绕过它就等于让"有界"变成一个
    // 由调用方自己填的数字。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false).with_max_evidence(usize::MAX);
    let many: Vec<EvidenceSlice> = (0..soca_contracts::MAX_CONTEXT_EVIDENCE + 8)
        .map(|index| {
            slice(
                &format!("e{index}"),
                "file:x.md",
                "sha256:z",
                DataClass::Public,
            )
        })
        .collect();

    let context = compiler
        .compile(input(many, Vec::new()))
        .expect("编译");
    assert_eq!(
        context.evidence.len(),
        soca_contracts::MAX_CONTEXT_EVIDENCE,
        "只能收紧到契约上限，不能突破它"
    );
}

// ---------------------------------------------------------------------------
// §8 的出站边界
// ---------------------------------------------------------------------------

#[test]
fn personal_evidence_cannot_be_compiled_for_a_remote_backend() {
    // §8：「私人数据类别不得出站到云端」。这条判断放在**编译出口**，不是发送前：
    // 放晚了就有人能先构造好请求再"顺便"检查。
    let compiler = ContextCompiler::new(ModelBackend::Remote, true);
    let result = compiler.compile(input(
        vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
        Vec::new(),
    ));

    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::EgressDenied {
            class: "personal"
        }))
    ));
}

#[test]
fn a_single_private_slice_blocks_the_whole_remote_context() {
    // 一份上下文里混了多个类别时，判断看**最敏感的那一条**，而不是第一条或平均。
    // 而且不能"把那条滤掉再发"：过滤后模型看到的结论会缺少依据。
    let compiler = ContextCompiler::new(ModelBackend::Remote, true);
    let result = compiler.compile(input(
        vec![
            slice("1", "doc:public", "open", DataClass::Public),
            slice("2", "file:summary.md", "sha256:aaa", DataClass::Personal),
        ],
        Vec::new(),
    ));
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::EgressDenied { .. }))
    ));
}

#[test]
fn remote_without_authorization_is_refused_even_for_public_evidence() {
    // §4.3：内存不足、本地模型不可用都不是把私人上下文发到云端的理由；
    // 而"已授权"也不是默认状态。
    let compiler = ContextCompiler::new(ModelBackend::Remote, false);
    let result = compiler.compile(input(
        vec![slice("1", "doc:public", "open", DataClass::Public)],
        Vec::new(),
    ));
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::RemoteNotAuthorized))
    ));
}

#[test]
fn the_same_evidence_compiles_fine_for_a_local_backend() {
    let compiler = ContextCompiler::new(ModelBackend::Gpu, false);
    assert!(
        compiler
            .compile(input(
                vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
                Vec::new(),
            ))
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// 返回物校验：模型不能凭空造依据
// ---------------------------------------------------------------------------

#[test]
fn a_model_proposal_citing_unseen_evidence_is_refused() {
    // 这是 §8 那条边界唯一可被机械检查的形式。没有它，模型可以编一个 obs:xxx 作为结论的
    // 依据，而所有结构校验都会通过——结构校验看的是"前缀对不对"，不是"这条证据存不存在"。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(
            vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
            Vec::new(),
        ))
        .expect("编译");

    let transport = DeterministicTransport::new(vec![Ok(output(vec![proposal(
        Candidate::Claim {
            statement: "摘要文件的版本是 sha256:zzz".to_string(),
            evidence_refs: vec![evidence_ref("fabricated")],
        },
        "我记得是这个版本",
    )]))]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(1));

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::EvidenceNotInContext { .. }))
    ));
}

#[test]
fn a_honest_proposal_that_cites_real_evidence_passes() {
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(
            vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
            Vec::new(),
        ))
        .expect("编译");

    let transport = DeterministicTransport::new(vec![Ok(output(vec![proposal(
        Candidate::Claim {
            statement: "摘要文件的版本是 sha256:aaa".to_string(),
            evidence_refs: vec![evidence_ref("1")],
        },
        "上下文里的观测就是这么写的",
    )]))]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(1));

    let validated = invoke(&mut gateway, &context).expect("应当通过");
    assert_eq!(validated.attempts, 1);
    assert_eq!(validated.output.proposals.len(), 1);
    // §3.2：模型自评的 0.97 原样留在 self_report 里，没有变成任何概率字段。
    assert_eq!(validated.output.proposals[0].self_report.reported_value, 0.97);
}

#[test]
fn a_model_proposal_inventing_a_prediction_reference_is_refused() {
    // §6.3 的预测由单元在动作前写下。模型要提动作，只能引用上下文里已有的那一条。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(
            vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
            Vec::new(),
        ))
        .expect("编译");

    let transport = DeterministicTransport::new(vec![Ok(output(vec![proposal(
        write_intent("prediction:invented"),
        "我预测这次写入会成功",
    )]))]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(1));

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::PredictionNotRecorded { .. }))
    ));
}

#[test]
fn a_model_proposal_may_only_reference_a_prediction_the_engine_already_recorded() {
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    // 输入里带着一条已记录的 prediction:1。
    let context = compiler
        .compile(input(
            vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
            Vec::new(),
        ))
        .expect("编译");

    let transport = DeterministicTransport::new(vec![Ok(output(vec![proposal(
        write_intent("prediction:1"),
        "以已记录的预测为依据",
    )]))]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(1));
    assert!(invoke(&mut gateway, &context).is_ok());
}

#[test]
fn a_read_only_context_refuses_an_action_proposal() {
    // 只读任务的输出 Schema 里没有 Action，因此模型即使想提也过不去。
    // 这是"上下文决定能力"而不是"模型决定能力"。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let mut read_only_input = input(
        vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
        Vec::new(),
    );
    read_only_input.output_schema = OutputSchema::read_only();
    let context = compiler.compile(read_only_input).expect("编译");

    assert!(!context.output_schema.allows(CandidateKind::Action));
    assert!(context.output_schema.allows(CandidateKind::Claim));

    let transport = DeterministicTransport::new(vec![Ok(output(vec![proposal(
        write_intent("prediction:1"),
        "顺手把文件改了",
    )]))]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(1));

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::CandidateKindNotAllowed {
            kind: "action"
        }))
    ));
}

// ---------------------------------------------------------------------------
// 预算、超时与重试
// ---------------------------------------------------------------------------

#[test]
fn a_non_retryable_failure_is_not_retried() {
    // 鉴权失败或返回物解析不了——再试多少次都是同一个结果。把它重试会让一个配置错误
    // 变成一串请求，而每一次都要等满超时。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(Vec::new(), Vec::new()))
        .expect("编译");

    let transport = DeterministicTransport::new(vec![
        Err(TransportError::Rejected {
            reason: "鉴权失败".to_string(),
        }),
        Ok(output(Vec::new())),
    ]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(3));

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::Transport(TransportError::Rejected { .. }))
    ));
    assert_eq!(gateway.calls(), 1, "只试了一次");
    assert_eq!(gateway.transport().invoked(), 1);
}

#[test]
fn a_retryable_failure_is_retried_within_the_budget() {
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(Vec::new(), Vec::new()))
        .expect("编译");

    let transport = DeterministicTransport::new(vec![
        Err(TransportError::Unavailable {
            reason: "排队中".to_string(),
        }),
        Ok(output(Vec::new())),
    ]);
    let mut gateway = gateway(transport, ModelBackend::Cpu, false, budget(2));

    let validated = invoke(&mut gateway, &context).expect("第二次应当成功");
    assert_eq!(validated.attempts, 2);
    assert_eq!(gateway.transport().invoked(), 2);
}

#[test]
fn retries_stop_at_the_budget_bound() {
    // §8："超时无无限重试"。有界重试是允许的，无限不是。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(Vec::new(), Vec::new()))
        .expect("编译");

    let script: Vec<Result<ModelOutput, TransportError>> = (0..5)
        .map(|_| {
            Err(TransportError::Timeout {
                limit_millis: 30_000,
            })
        })
        .collect();
    let mut gateway = gateway(
        DeterministicTransport::new(script),
        ModelBackend::Cpu,
        false,
        budget(2),
    );

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::AttemptsExhausted { attempts: 2, .. })
    ));
    assert_eq!(gateway.calls(), 2, "不会偷偷多试");
    assert_eq!(gateway.transport().invoked(), 2);
}

#[test]
fn an_attempt_budget_of_zero_is_refused_at_construction() {
    let result = ModelGateway::new(
        DeterministicTransport::always(output(Vec::new())),
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 0,
        },
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    );
    assert!(matches!(result, Err(GatewayError::Contract(_))));
}

#[test]
fn the_total_wall_clock_is_enforced_by_the_gateway_not_left_to_the_transport() {
    // 传输层"答应遵守自己的超时"是一句约定。网关另有总墙钟兜底，因为一次不守约的调用
    // 就能占满整个任务额度。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(Vec::new(), Vec::new()))
        .expect("编译");

    let mut gateway = gateway(
        DeterministicTransport::always(output(Vec::new())),
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 0,
            max_attempts: 3,
        },
    );

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::WallClockExceeded { .. })
    ));
    assert_eq!(gateway.calls(), 0, "额度为零时不该发出任何请求");
}

#[test]
fn an_output_exceeding_the_token_budget_is_refused() {
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(Vec::new(), Vec::new()))
        .expect("编译");

    let mut greedy = output(Vec::new());
    greedy.usage = TokenUsage {
        input_tokens: 100,
        output_tokens: 4096,
    };
    let mut gateway = gateway(
        DeterministicTransport::always(greedy),
        ModelBackend::Cpu,
        false,
        budget(1),
    );

    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::ContextLimitExceeded {
            field: "model_output.usage.output_tokens",
            ..
        }))
    ));
}

#[test]
fn the_gateway_rechecks_egress_before_every_call() {
    // 编译期已经查过一次，网关再查一次。两次检查互不依赖：一份上下文可能在编译时是本地
    // 后端，而调用时换了后端。少查一次就等于给了一条绕过路径。
    let compiler = ContextCompiler::new(ModelBackend::Cpu, false);
    let context = compiler
        .compile(input(
            vec![slice("1", "file:summary.md", "sha256:aaa", DataClass::Personal)],
            Vec::new(),
        ))
        .expect("本地后端可以编译私人证据");

    let mut gateway = gateway(
        DeterministicTransport::always(output(Vec::new())),
        ModelBackend::Remote,
        true,
        budget(1),
    );
    let result = invoke(&mut gateway, &context);
    assert!(matches!(
        result,
        Err(GatewayError::Contract(ContractError::EgressDenied { .. }))
    ));
    assert_eq!(gateway.calls(), 0, "被拦下的请求不发出去");
}
