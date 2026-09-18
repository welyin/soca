//! 契约层回归测试。
//!
//! 每个测试对应架构文档里一条可证伪的硬规则。这些测试就是 P0 门槛的机器可执行形式：
//! "能重放'请求≠执行≠验证'的轨迹"这一条由 [`trajectory_request_is_not_execution_is_not_verification`]
//! 承担。

use serde_json::json;
use soca_contracts::*;

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

/// 固定的时间基准，保证测试可复现。
fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("基准时间必须是合法 RFC 3339")
}

fn at(offset_seconds: i64) -> WallClock {
    base_time().plus_seconds(offset_seconds)
}

const BOOT_A: &str = "00000000-0000-4000-8000-0000000000a1";
const BOOT_B: &str = "00000000-0000-4000-8000-0000000000b2";

fn boot_a() -> BootId {
    BootId::parse(BOOT_A).expect("固定 UUID 必须可解析")
}

fn boot_b() -> BootId {
    BootId::parse(BOOT_B).expect("固定 UUID 必须可解析")
}

fn event_uuid() -> EventId {
    EventId::parse("11111111-1111-4111-8111-111111111111").expect("固定 UUID 必须可解析")
}

fn permission_scope(max: ActionLevel) -> PermissionScope {
    PermissionScope {
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略引用"),
        max_action_level: max,
    }
}

fn window(offset_start: i64, offset_end: i64) -> TimeWindow {
    TimeWindow::new(at(offset_start), at(offset_end)).expect("合法时间窗")
}

fn uncertainty_none() -> Uncertainty {
    Uncertainty {
        probability: None,
        notes: vec!["传感器质量不足".to_string()],
    }
}

fn sample_envelope() -> Envelope {
    Envelope::new(
        event_uuid(),
        SourceId::new("device:file-watcher").expect("固定来源"),
        1,
        boot_a(),
        827,
        TaskId::new("task:42").expect("固定任务"),
        Vec::new(),
        at(0),
        Monotonic::new(boot_a(), 1_000_000),
        Provenance::Sensor {
            adapter: SourceId::new("device:file-watcher").expect("固定适配器"),
        },
        PayloadRef::Blob {
            blob_ref: BlobRef::new("blob:obs-193").expect("固定对象"),
            media_type: MediaType::new("application/json").expect("固定媒体类型"),
            bytes: 512,
            sha256: Sha256Hex::of_bytes(b"soca-observation-193"),
        },
        permission_scope(ActionLevel::A1),
        DataClass::Personal,
        None,
        IdempotencyKey::new("idem:task-42-seq-827").expect("固定幂等键"),
    )
}

fn sample_prediction() -> Prediction {
    Prediction::new(
        PredictionRef::new("prediction:pred-7").expect("固定预测引用"),
        "已生成文件的内容哈希".to_string(),
        "与草稿哈希一致，且文件在授权目录内可见".to_string(),
        Expectation::VersionEquals {
            subject_ref: "已生成文件的内容哈希".to_string(),
            expected: "sha256:new".to_string(),
        },
        window(0, 60),
        vec!["哈希不一致".to_string(), "文件不存在".to_string()],
        uncertainty_none(),
    )
    .expect("合法预测")
}

fn sample_intent(risk: ActionLevel) -> ActionIntent {
    ActionIntent::new(
        ActionId::new("action:64").expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new("D:\\资料\\摘要").expect("固定范围"),
        json!({ "path": "D:\\资料\\摘要\\summary.md", "bytes": 2048 }),
        vec!["目标目录已授权".to_string()],
        sample_prediction().prediction_ref,
        risk,
        ResourceCost {
            est_ram_bytes: 1 << 20,
            est_tokens: 0,
            est_millis: 50,
        },
        UnitId::new("unit:file-summary:07").expect("固定单元"),
    )
    .expect("合法动作意图")
}

fn sample_permit(intent: &ActionIntent) -> ExecutionPermit {
    ExecutionPermit::issue_for(
        intent,
        PermitId::new("permit:9").expect("固定许可"),
        SubjectId::new("user:local").expect("固定主体"),
        at(0),
        300,
        1,
        BudgetRef::new("budget:task-42").expect("固定预算"),
        PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
        CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
        None,
    )
    .expect("合法执行许可")
}

// ---------------------------------------------------------------------------
// 信封（§7.1）
// ---------------------------------------------------------------------------

#[test]
fn envelope_accepts_well_formed_message() {
    sample_envelope().validate().expect("样本信封必须合法");
}

#[test]
fn envelope_rejects_schema_version_drift() {
    let mut envelope = sample_envelope();
    envelope.schema_version = SCHEMA_VERSION + 1;
    assert_eq!(
        envelope.validate(),
        Err(ContractError::SchemaVersionMismatch {
            expected: SCHEMA_VERSION,
            actual: SCHEMA_VERSION + 1,
        })
    );
}

#[test]
fn envelope_rejects_monotonic_from_another_boot() {
    let mut envelope = sample_envelope();
    envelope.received_monotonic = Monotonic::new(boot_b(), 5);
    assert_eq!(envelope.validate(), Err(ContractError::BootIdMismatch));
}

#[test]
fn envelope_rejects_duplicate_and_self_causal_parents() {
    let mut duplicated = sample_envelope();
    duplicated.causal_parent_ids = vec![event_uuid(), event_uuid()];
    assert!(matches!(
        duplicated.validate(),
        Err(ContractError::DuplicateRefs {
            field: "causal_parent_ids",
            count: 2,
            ..
        })
    ));

    let mut selfish = sample_envelope();
    selfish.causal_parent_ids = vec![event_uuid()];
    assert!(matches!(
        selfish.validate(),
        Err(ContractError::SelfCausalParent(_))
    ));
}

#[test]
fn envelope_rejects_oversized_inline_payload() {
    let mut envelope = sample_envelope();
    envelope.payload_ref = PayloadRef::Inline {
        media_type: MediaType::new("text/plain").expect("固定媒体类型"),
        body: "x".repeat(MAX_SMALL_MESSAGE_ENVELOPE_BYTES + 1),
    };
    assert!(matches!(
        envelope.validate(),
        Err(ContractError::PayloadTooLarge { .. })
    ));
}

#[test]
fn envelope_ordering_is_only_defined_within_one_stream_and_boot() {
    let first = sample_envelope();
    let mut later = sample_envelope();
    later.source_sequence = 828;
    assert_eq!(first.ordering_with(&later), Some(std::cmp::Ordering::Less));

    // 同源、同 boot 但不同世代：流已重建，序列号不再可比（§11.2）。
    let mut new_epoch = sample_envelope();
    new_epoch.source_epoch = 2;
    assert_eq!(first.ordering_with(&new_epoch), None);

    // 不同 boot：单调时间不可比（§7.1）。
    let mut new_boot = sample_envelope();
    new_boot.boot_id = boot_b();
    assert_eq!(first.ordering_with(&new_boot), None);

    // 不同来源：不假定全局先后。
    let mut other_source = sample_envelope();
    other_source.source_id = SourceId::new("device:screen").expect("固定来源");
    assert_eq!(first.ordering_with(&other_source), None);
}

#[test]
fn monotonic_never_compares_across_boots() {
    let a = Monotonic::new(boot_a(), 1_000);
    let b = Monotonic::new(boot_b(), 2_000);
    assert_eq!(a.compare_same_boot(&b), None);
    assert_eq!(a.elapsed_since_same_boot(&b), None);

    let earlier = Monotonic::new(boot_a(), 400);
    assert_eq!(a.compare_same_boot(&earlier), Some(std::cmp::Ordering::Greater));
    assert_eq!(
        a.elapsed_since_same_boot(&earlier),
        Some(std::time::Duration::from_nanos(600))
    );
}

// ---------------------------------------------------------------------------
// 来源信任等级（§6.1、§11.1）
// ---------------------------------------------------------------------------

#[test]
fn only_explicit_user_channels_carry_instruction_authority() {
    let chat = Provenance::User {
        channel: UserChannel::Chat,
    };
    assert!(chat.is_instruction_authority());

    // 屏幕/文档内容属于数据。
    let screen = Provenance::Sensor {
        adapter: SourceId::new("device:screen").expect("固定来源"),
    };
    assert!(!screen.is_instruction_authority());

    // ASR 转写是派生物：说话者身份不自动可信，不能替代图形审批（§11.1、§14）。
    let asr = Provenance::Derived {
        source_event_id: event_uuid(),
        model_version: ModelVersion::new("sha256:asr-weights").expect("固定模型版本"),
        transform: DerivationKind::Asr,
    };
    assert!(!asr.is_instruction_authority());
    assert_eq!(asr.original_event(), Some(&event_uuid()));

    // 工具输出同样是数据。
    let tool = Provenance::Tool {
        tool_id: ToolId::new("fs.read").expect("固定工具"),
        action_id: ActionId::new("action:1").expect("固定动作"),
    };
    assert!(!tool.is_instruction_authority());
}

// ---------------------------------------------------------------------------
// 概率校准（§3.2）
// ---------------------------------------------------------------------------

#[test]
fn model_self_report_never_becomes_a_probability() {
    let self_report = ModelSelfReport {
        reported_value: 0.93,
        model_version: ModelVersion::new("sha256:llm-weights").expect("固定模型版本"),
        rationale: "我很有把握".to_string(),
    };
    let probability = self_report.into_uncalibrated("文件哈希一致".to_string(), window(0, 60));

    assert_eq!(probability.calibration, CalibrationSource::Uncalibrated);
    assert_eq!(probability.value, None, "自评分数不得被当成概率值");
    probability.validate().expect("显式 unknown 是合法的");
}

#[test]
fn uncalibrated_probability_cannot_carry_a_value() {
    let forged = CalibratedProbability {
        subject: "文件哈希一致".to_string(),
        horizon: window(0, 60),
        model_version: ModelVersion::new("sha256:llm-weights").expect("固定模型版本"),
        calibration: CalibrationSource::Uncalibrated,
        value: Some(0.93),
    };
    assert_eq!(
        forged.validate(),
        Err(ContractError::UncalibratedProbability)
    );
}

#[test]
fn measured_calibration_requires_samples_and_a_valid_range() {
    let no_samples = CalibratedProbability {
        subject: "x".to_string(),
        horizon: window(0, 60),
        model_version: ModelVersion::new("sha256:llm-weights").expect("固定模型版本"),
        calibration: CalibrationSource::Measured {
            sample_count: 0,
            brier: 0.11,
            ece: 0.03,
            method: "isotonic".to_string(),
        },
        value: Some(0.5),
    };
    assert_eq!(
        no_samples.validate(),
        Err(ContractError::EmptyCalibrationSamples)
    );

    let out_of_range = CalibratedProbability {
        subject: "x".to_string(),
        horizon: window(0, 60),
        model_version: ModelVersion::new("sha256:llm-weights").expect("固定模型版本"),
        calibration: CalibrationSource::Measured {
            sample_count: 200,
            brier: 0.11,
            ece: 0.03,
            method: "isotonic".to_string(),
        },
        value: Some(1.4),
    };
    assert!(matches!(
        out_of_range.validate(),
        Err(ContractError::ProbabilityOutOfRange { .. })
    ));
}

// ---------------------------------------------------------------------------
// 预测与假设（§6.3、§7.2）
// ---------------------------------------------------------------------------

#[test]
fn prediction_requires_falsifiable_failure_conditions() {
    let unfalsifiable = Prediction::new(
        PredictionRef::new("prediction:pred-8").expect("固定预测引用"),
        "整理结果".to_string(),
        "会变好".to_string(),
        Expectation::Absent {
            subject_ref: "整理结果".to_string(),
        },
        window(0, 60),
        Vec::new(),
        uncertainty_none(),
    );
    assert_eq!(
        unfalsifiable,
        Err(ContractError::MissingRefs {
            field: "prediction.failure_conditions"
        })
    );
}

#[test]
fn a_prediction_cannot_expect_a_different_object_than_it_names() {
    // "预测 A、检查 B"这种错位必须被拒绝，否则后验判定会静默通过。
    let mismatched = Prediction::new(
        PredictionRef::new("prediction:pred-8").expect("固定预测引用"),
        "已生成文件的内容哈希".to_string(),
        "与草稿哈希一致".to_string(),
        Expectation::VersionEquals {
            subject_ref: "另一个对象".to_string(),
            expected: "sha256:new".to_string(),
        },
        window(0, 60),
        vec!["哈希不一致".to_string()],
        uncertainty_none(),
    );
    assert!(matches!(
        mismatched,
        Err(ContractError::ExpectationSubjectMismatch { .. })
    ));
}

#[test]
fn prediction_rejects_uncalibrated_probability_payload() {
    let uncertainty = Uncertainty {
        probability: Some(CalibratedProbability {
            subject: "哈希一致".to_string(),
            horizon: window(0, 60),
            model_version: ModelVersion::new("sha256:llm-weights").expect("固定模型版本"),
            calibration: CalibrationSource::Uncalibrated,
            value: Some(0.9),
        }),
        notes: Vec::new(),
    };
    let result = Prediction::new(
        PredictionRef::new("prediction:pred-9").expect("固定预测引用"),
        "哈希一致".to_string(),
        "无变化".to_string(),
        Expectation::VersionEquals {
            subject_ref: "哈希一致".to_string(),
            expected: "sha256:abc".to_string(),
        },
        window(0, 60),
        vec!["哈希改变".to_string()],
        uncertainty,
    );
    assert_eq!(result, Err(ContractError::UncalibratedProbability));
}

#[test]
fn hypothesis_rejects_evidence_on_both_sides() {
    let shared = EvidenceRef::new("obs:193").expect("固定证据引用");
    let contradiction = Hypothesis {
        claim: "文件未被修改".to_string(),
        supporting: vec![shared.clone()],
        against: vec![shared],
        unknowns: Vec::new(),
        proposed_by: UnitId::new("unit:file-summary:07").expect("固定单元"),
    };
    assert!(matches!(
        contradiction.validate(),
        Err(ContractError::ContradictoryEvidence { .. })
    ));

    let bare = Hypothesis {
        claim: "文件未被修改".to_string(),
        supporting: Vec::new(),
        against: Vec::new(),
        unknowns: Vec::new(),
        proposed_by: UnitId::new("unit:file-summary:07").expect("固定单元"),
    };
    assert!(bare.is_bare_assertion());
}

// ---------------------------------------------------------------------------
// 动作意图（§6.3、§12.1、§12.2）
// ---------------------------------------------------------------------------

#[test]
fn action_intent_requires_structured_parameters() {
    let raw_shell = ActionIntent::new(
        ActionId::new("action:65").expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new("D:\\资料").expect("固定范围"),
        json!("rm -rf /"),
        Vec::new(),
        sample_prediction().prediction_ref,
        ActionLevel::A2,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 0,
        },
        UnitId::new("unit:file-summary:07").expect("固定单元"),
    );
    assert_eq!(
        raw_shell,
        Err(ContractError::ParametersNotStructured { actual: "string" })
    );
}

#[test]
fn action_intent_rejects_shell_style_tools() {
    let shell = ActionIntent::new(
        ActionId::new("action:66").expect("固定动作"),
        ToolId::new("shell.exec").expect("固定工具"),
        ResourceScope::new("D:\\资料").expect("固定范围"),
        json!({ "command": "dir" }),
        Vec::new(),
        sample_prediction().prediction_ref,
        ActionLevel::A2,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 0,
        },
        UnitId::new("unit:file-summary:07").expect("固定单元"),
    );
    assert!(matches!(
        shell,
        Err(ContractError::ForbiddenToolInV1 { .. })
    ));
}

#[test]
fn action_intent_refuses_a4_from_the_cognitive_loop() {
    let privilege_escalation = ActionIntent::new(
        ActionId::new("action:67").expect("固定动作"),
        ToolId::new("policy.self_update").expect("固定工具"),
        ResourceScope::new("self").expect("固定范围"),
        json!({ "grant": "admin" }),
        Vec::new(),
        sample_prediction().prediction_ref,
        ActionLevel::A4,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 0,
        },
        UnitId::new("unit:file-summary:07").expect("固定单元"),
    );
    assert_eq!(
        privilege_escalation,
        Err(ContractError::ForbiddenInCognitiveLoop { level: "A4" })
    );
}

// ---------------------------------------------------------------------------
// 执行许可（§12.2）
// ---------------------------------------------------------------------------

#[test]
fn permit_authorizes_the_exact_intent_it_was_issued_for() {
    let intent = sample_intent(ActionLevel::A1);
    let permit = sample_permit(&intent);

    permit
        .authorizes(&intent, at(10), 0)
        .expect("参数一致的动作用许可放行");
    intent
        .risk
        .is_allowed_in_cognitive_loop()
        .then_some(())
        .expect("A1 允许由认知循环发起");
}

#[test]
fn permit_rejects_parameter_tampering() {
    let intent = sample_intent(ActionLevel::A1);
    let permit = sample_permit(&intent);

    // 审批时给的是 2048 字节，执行时改成 4096 字节。
    let tampered = ActionIntent::new(
        intent.action_id.clone(),
        intent.tool_id.clone(),
        intent.object_scope.clone(),
        json!({ "path": "D:\\资料\\摘要\\summary.md", "bytes": 4096 }),
        intent.preconditions.clone(),
        intent.prediction_ref.clone(),
        intent.risk,
        intent.cost,
        intent.proposed_by.clone(),
    )
    .expect("参数形态仍然合法");

    assert!(intent.parameters_digest() != tampered.parameters_digest());
    assert_eq!(
        permit.authorizes(&tampered, at(10), 0),
        Err(ContractError::PermitMismatch {
            field: "parameters_digest"
        })
    );

    // 参数 key 顺序不同但内容相同，摘要必须一致（规范化依赖 serde_json 的排序 map）。
    let reordered = ActionIntent::new(
        intent.action_id.clone(),
        intent.tool_id.clone(),
        intent.object_scope.clone(),
        json!({ "bytes": 2048, "path": "D:\\资料\\摘要\\summary.md" }),
        intent.preconditions.clone(),
        intent.prediction_ref.clone(),
        intent.risk,
        intent.cost,
        intent.proposed_by.clone(),
    )
    .expect("参数形态仍然合法");
    assert_eq!(intent.parameters_digest(), reordered.parameters_digest());
}

#[test]
fn permit_rejects_expiry_and_exhaustion() {
    let intent = sample_intent(ActionLevel::A1);
    let permit = sample_permit(&intent);

    assert!(matches!(
        permit.authorizes(&intent, at(301), 0),
        Err(ContractError::PermitExpired { .. })
    ));
    assert_eq!(
        permit.authorizes(&intent, at(10), 1),
        Err(ContractError::PermitExhausted { max_uses: 1 })
    );
}

#[test]
fn permit_rejects_scope_drift() {
    let intent = sample_intent(ActionLevel::A1);
    let permit = sample_permit(&intent);

    let other_scope = ActionIntent::new(
        intent.action_id.clone(),
        intent.tool_id.clone(),
        ResourceScope::new("D:\\其他目录").expect("固定范围"),
        intent.parameters.clone(),
        intent.preconditions.clone(),
        intent.prediction_ref.clone(),
        intent.risk,
        intent.cost,
        intent.proposed_by.clone(),
    )
    .expect("参数形态仍然合法");

    assert_eq!(
        permit.authorizes(&other_scope, at(10), 0),
        Err(ContractError::PermitMismatch {
            field: "object_scope"
        })
    );
}

#[test]
fn a2_and_a3_permits_require_an_approval_id() {
    for level in [ActionLevel::A2, ActionLevel::A3] {
        let intent = sample_intent(level);
        let without_approval = ExecutionPermit::issue_for(
            &intent,
            PermitId::new("permit:10").expect("固定许可"),
            SubjectId::new("user:local").expect("固定主体"),
            at(0),
            300,
            1,
            BudgetRef::new("budget:task-42").expect("固定预算"),
            PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
            CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
            None,
        );
        assert_eq!(
            without_approval,
            Err(ContractError::MissingApproval {
                level: level.as_str()
            })
        );

        let with_approval = ExecutionPermit::issue_for(
            &intent,
            PermitId::new("permit:11").expect("固定许可"),
            SubjectId::new("user:local").expect("固定主体"),
            at(0),
            300,
            1,
            BudgetRef::new("budget:task-42").expect("固定预算"),
            PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
            CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
            Some(ApprovalId::new("approval:3").expect("固定审批")),
        )
        .expect("带审批的许可必须可签发");
        assert_eq!(with_approval.approval_id.unwrap().as_str(), "approval:3");
    }
}

#[test]
fn a4_permits_cannot_be_issued_at_all() {
    let forged = ExecutionPermit::new(
        PermitId::new("permit:12").expect("固定许可"),
        SubjectId::new("user:local").expect("固定主体"),
        ToolId::new("policy.self_update").expect("固定工具"),
        ResourceScope::new("self").expect("固定范围"),
        Sha256Hex::of_bytes(b"{}"),
        at(0),
        at(300),
        1,
        BudgetRef::new("budget:task-42").expect("固定预算"),
        PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
        CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
        Some(ApprovalId::new("approval:4").expect("固定审批")),
        ActionLevel::A4,
    );
    assert_eq!(
        forged,
        Err(ContractError::ForbiddenInCognitiveLoop { level: "A4" })
    );
}

#[test]
fn approval_requirement_matches_the_documented_table() {
    assert_eq!(
        ActionLevel::A0.approval_requirement(),
        ApprovalRequirement::TaskScopeAndBudget
    );
    assert_eq!(
        ActionLevel::A1.approval_requirement(),
        ApprovalRequirement::StandingScopeGrant
    );
    assert_eq!(
        ActionLevel::A2.approval_requirement(),
        ApprovalRequirement::PreviewOrPerTaskApproval
    );
    assert_eq!(
        ActionLevel::A3.approval_requirement(),
        ApprovalRequirement::PerActionHumanApproval
    );
    assert_eq!(
        ActionLevel::A4.approval_requirement(),
        ApprovalRequirement::ForbiddenInCognitiveLoop
    );

    assert!(ActionLevel::A2.requires_approval_id());
    assert!(ActionLevel::A3.requires_approval_id());
    assert!(!ActionLevel::A0.requires_approval_id());
    assert!(!ActionLevel::A1.requires_approval_id());
}

// ---------------------------------------------------------------------------
// 数据状态（§7.2）
// ---------------------------------------------------------------------------

fn all_data_states() -> Vec<DataState> {
    let intent = sample_intent(ActionLevel::A1);
    let permit = sample_permit(&intent);
    let receipt = ActionReceipt {
        action_id: intent.action_id.clone(),
        permit_id: permit.permit_id.clone(),
        status: CommitStatus::Completed,
        recorded_at: at(5),
        detail: "已写入临时文件并原子替换".to_string(),
        observed_target_version: None,
    };
    let outcome = OutcomeVerified::new(
        intent.action_id.clone(),
        intent.prediction_ref.clone(),
        Verdict::Supported,
        vec![EvidenceRef::new("obs:194").expect("固定证据引用")],
    )
    .expect("合法判定");

    vec![
        DataState::Observation(Observation {
            subject: "文件版本".to_string(),
            value: "sha256:abc".to_string(),
            body_ref: None,
            evidence_ref: EvidenceRef::new("obs:193").expect("固定证据引用"),
            derived_from: Vec::new(),
            observed_by: UnitId::new("unit:file-summary:07").expect("固定单元"),
        }),
        DataState::Hypothesis(Hypothesis {
            claim: "文件未被修改".to_string(),
            supporting: vec![EvidenceRef::new("obs:193").expect("固定证据引用")],
            against: Vec::new(),
            unknowns: Vec::new(),
            proposed_by: UnitId::new("unit:file-summary:07").expect("固定单元"),
        }),
        DataState::Prediction(sample_prediction()),
        DataState::ActionIntent(intent),
        DataState::ExecutionPermit(permit),
        DataState::ActionReceipt(receipt),
        DataState::OutcomeVerified(outcome),
    ]
}

#[test]
fn only_execution_permit_may_trigger_a_side_effect() {
    let states = all_data_states();
    assert_eq!(states.len(), 7, "七类数据状态必须全部覆盖");

    for state in &states {
        let expected = state.kind() == DataStateKind::ExecutionPermit;
        assert_eq!(
            state.may_trigger_side_effect(),
            expected,
            "{:?} 的副作用权限判定不符合 §7.2",
            state.kind()
        );
        state.validate().expect("样本状态自身必须合法");
    }

    let permitted: Vec<_> = states
        .iter()
        .filter(|state| state.may_trigger_side_effect())
        .map(DataState::kind)
        .collect();
    assert_eq!(permitted, vec![DataStateKind::ExecutionPermit]);
}

#[test]
fn receipt_is_not_verification_and_verification_grants_no_permission() {
    let states = all_data_states();

    let receipt = states
        .iter()
        .find_map(|state| match state {
            DataState::ActionReceipt(receipt) => Some(receipt.clone()),
            _ => None,
        })
        .expect("样本必须包含回执");
    assert!(
        !receipt.is_postcondition_verified(),
        "回执不等于后置条件验证成功（§7.2）"
    );

    let outcome = states
        .iter()
        .find_map(|state| match state {
            DataState::OutcomeVerified(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .expect("样本必须包含判定");
    assert!(
        !outcome.grants_new_permission(),
        "验证结果不自动追加新权限（§7.2）"
    );
}

#[test]
fn outcome_verified_requires_at_least_one_observation() {
    let intent = sample_intent(ActionLevel::A1);
    let result = OutcomeVerified::new(
        intent.action_id.clone(),
        intent.prediction_ref.clone(),
        Verdict::Inconclusive,
        Vec::new(),
    );
    assert_eq!(
        result,
        Err(ContractError::MissingRefs {
            field: "outcome_verified.observation_refs"
        })
    );
}

// ---------------------------------------------------------------------------
// P0 门槛：请求 ≠ 执行 ≠ 验证
// ---------------------------------------------------------------------------

#[test]
fn trajectory_request_is_not_execution_is_not_verification() {
    // 1. 观测进来。它只是数据，不能推动任何副作用。
    let observation = DataState::Observation(Observation {
        subject: "授权目录中的目标文件".to_string(),
        value: "当前版本 sha256:abc".to_string(),
        body_ref: None,
        evidence_ref: EvidenceRef::new("obs:193").expect("固定证据引用"),
        derived_from: Vec::new(),
        observed_by: UnitId::new("unit:file-summary:07").expect("固定单元"),
    });
    assert!(!observation.may_trigger_side_effect());

    // 2. 单元写下动作前预测：没有失败条件的预测构造不出来。
    let prediction = sample_prediction();
    assert_eq!(prediction.failure_conditions.len(), 2);
    assert!(!DataState::Prediction(prediction.clone()).may_trigger_side_effect());

    // 3. 单元提出动作意图。它仍然不能执行任何事。
    let intent = sample_intent(ActionLevel::A2);
    assert!(!DataState::ActionIntent(intent.clone()).may_trigger_side_effect());

    // 4. 没有许可，就没有任何东西可以交给执行代理。许可必须显式签发。
    let permit = ExecutionPermit::issue_for(
        &intent,
        PermitId::new("permit:9").expect("固定许可"),
        SubjectId::new("user:local").expect("固定主体"),
        at(0),
        300,
        1,
        BudgetRef::new("budget:task-42").expect("固定预算"),
        PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
        CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
        Some(ApprovalId::new("approval:3").expect("固定审批")),
    )
    .expect("A2 带审批后可签发");

    // 5. 只有许可可以触发副作用，且它只授权这一次具体动作。
    assert!(DataState::ExecutionPermit(permit.clone()).may_trigger_side_effect());
    permit
        .authorizes(&intent, at(1), 0)
        .expect("许可授权这次动作");
    assert!(
        permit.authorizes(&intent, at(1), 1).is_err(),
        "max_uses=1 的许可不能授权第二次"
    );

    // 6. 许可被消费成回执。回执不是验证。
    let receipt = DataState::ExecutionPermit(permit)
        .into_receipt(ActionReceipt {
            action_id: intent.action_id.clone(),
            permit_id: PermitId::new("permit:9").expect("固定许可"),
            status: CommitStatus::Completed,
            recorded_at: at(2),
            detail: "已写入并原子替换".to_string(),
            observed_target_version: None,
        })
        .expect("许可消费成功");
    assert!(!receipt.is_postcondition_verified());

    // 7. 重新观测目标文件，才产生后置条件判定。
    let outcome = OutcomeVerified::new(
        intent.action_id.clone(),
        intent.prediction_ref.clone(),
        Verdict::Supported,
        vec![EvidenceRef::new("obs:195").expect("固定证据引用")],
    )
    .expect("判定必须基于新观测");
    assert_eq!(outcome.verdict, Verdict::Supported);
    assert!(
        !outcome.grants_new_permission(),
        "验证通过也不自动追加新权限"
    );

    // 8. 只有许可能被消费；观测、意图、回执都不行。
    assert!(
        DataState::ActionIntent(intent.clone())
            .into_receipt(ActionReceipt {
                action_id: intent.action_id.clone(),
                permit_id: PermitId::new("permit:9").expect("固定许可"),
                status: CommitStatus::Completed,
                recorded_at: at(3),
                detail: String::new(),
                observed_target_version: None,
            })
            .is_err(),
        "动作意图不能冒充许可被消费"
    );
}

#[test]
fn receipt_must_reference_the_permit_that_was_consumed() {
    let intent = sample_intent(ActionLevel::A1);
    let permit = sample_permit(&intent);
    let mismatched = DataState::ExecutionPermit(permit).into_receipt(ActionReceipt {
        action_id: intent.action_id.clone(),
        permit_id: PermitId::new("permit:999").expect("固定许可"),
        status: CommitStatus::Completed,
        recorded_at: at(2),
        detail: String::new(),
        observed_target_version: None,
    });
    assert!(mismatched.is_err());
}

// ---------------------------------------------------------------------------
// 数据类别与出站（§8）
// ---------------------------------------------------------------------------

#[test]
fn private_data_may_never_egress_to_the_cloud_by_class() {
    assert_eq!(
        DataClass::Public.cloud_egress(),
        EgressVerdict::RequiresPolicyApproval
    );
    for class in [DataClass::Personal, DataClass::Sensitive, DataClass::Secret] {
        assert_eq!(
            class.cloud_egress(),
            EgressVerdict::Denied,
            "{} 不得因任何资源压力而上云",
            class.as_str()
        );
    }

    let envelope = sample_envelope();
    assert_eq!(envelope.data_class, DataClass::Personal);
    assert_eq!(envelope.cloud_egress_verdict(), EgressVerdict::Denied);
}

#[test]
fn envelope_permission_scope_caps_the_action_level() {
    let envelope = sample_envelope();
    envelope
        .authorize_level(ActionLevel::A0)
        .expect("A0 在 A1 范围内");
    envelope
        .authorize_level(ActionLevel::A1)
        .expect("A1 在 A1 范围内");
    assert!(envelope.authorize_level(ActionLevel::A2).is_err());
}

// ---------------------------------------------------------------------------
// 单元生命周期（§9.2）
// ---------------------------------------------------------------------------

fn sample_unit() -> UnitSnapshot {
    UnitSnapshot {
        unit_id: UnitId::new("unit:file-summary:07").expect("固定单元"),
        kind: UnitKind::Leaf,
        schema_version: SCHEMA_VERSION,
        scope: Scope {
            domain: DomainId::new("document-summary").expect("固定领域"),
            task_contract: TaskContractVersion::new("summary-v1").expect("固定合同"),
        },
        goal_refs: vec![GoalId::new("goal:42").expect("固定目标")],
        belief_revision: 18,
        belief_snapshot_ref: BlobRef::new("blob:belief-18").expect("固定对象"),
        evidence_refs: vec![
            EvidenceRef::new("obs:193").expect("固定证据引用"),
            EvidenceRef::new("tool-result:64").expect("固定证据引用"),
        ],
        relation_refs: vec![RelationRef::new("relation:source-lineage-8").expect("固定关系")],
        strategy_version: StrategyVersion::new("summary-policy-v1").expect("固定策略"),
        model_profile_ref: ModelProfileRef::new("profile:reasoning-local").expect("固定画像"),
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略"),
        budget_ref: BudgetRef::new("budget:task-42").expect("固定预算"),
        pending_action_ids: Vec::new(),
        last_applied_sequence: 827,
        state: UnitState::Cold,
    }
}

#[test]
fn unit_snapshot_matches_the_documented_field_set() {
    let unit = sample_unit();
    unit.validate().expect("样本单元必须合法");

    let encoded = serde_json::to_value(&unit).expect("必须可序列化为 JSON");
    let object = encoded.as_object().expect("快照必须是 JSON 对象");

    let mut actual: Vec<&str> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();
    let mut expected = UNIT_SNAPSHOT_FIELDS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "快照字段集合与 §3.2 不一致；增删字段必须提升 SCHEMA_VERSION"
    );

    let roundtrip: UnitSnapshot = serde_json::from_value(encoded).expect("必须可反序列化");
    assert_eq!(roundtrip, unit);
}

#[test]
fn unit_snapshot_carries_no_handles_or_secrets() {
    let encoded = serde_json::to_string(&sample_unit()).expect("必须可序列化");
    for forbidden in [
        "handle", "pointer", "password", "secret", "api_key", "weights", "gpu_ptr", "fn_ptr",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "快照中出现了疑似句柄或密钥的字段名：{forbidden}"
        );
    }
}

#[test]
fn unit_snapshot_rejects_duplicate_refs_and_version_drift() {
    let mut duplicated = sample_unit();
    duplicated.evidence_refs = vec![
        EvidenceRef::new("obs:193").expect("固定证据引用"),
        EvidenceRef::new("obs:193").expect("固定证据引用"),
    ];
    assert!(matches!(
        duplicated.validate(),
        Err(ContractError::DuplicateRefs {
            field: "unit.evidence_refs",
            ..
        })
    ));

    let mut stale = sample_unit();
    stale.schema_version = SCHEMA_VERSION + 1;
    assert!(matches!(
        stale.validate(),
        Err(ContractError::SchemaVersionMismatch { .. })
    ));
}

#[test]
fn unit_must_checkpoint_before_going_cold() {
    let mut unit = sample_unit();

    unit.transition_to(UnitState::Loading).expect("COLD -> LOADING");
    unit.transition_to(UnitState::Ready).expect("LOADING -> READY");

    // §9.2 的状态图要求 READY 先进入 CHECKPOINTING 才能降温。
    assert_eq!(
        unit.transition_to(UnitState::Cold),
        Err(ContractError::LifecycleViolation {
            from: "READY",
            to: "COLD"
        })
    );

    unit.transition_to(UnitState::Checkpointing)
        .expect("READY -> CHECKPOINTING");
    unit.transition_to(UnitState::Cold)
        .expect("CHECKPOINTING -> COLD（无未决动作）");
    assert_eq!(unit.state, UnitState::Cold);
}

#[test]
fn unknown_side_effects_block_unit_unload_until_handed_over() {
    let mut unit = sample_unit();
    unit.transition_to(UnitState::Loading).expect("COLD -> LOADING");
    unit.transition_to(UnitState::Ready).expect("LOADING -> READY");
    unit.transition_to(UnitState::Checkpointing)
        .expect("READY -> CHECKPOINTING");

    // 有一个动作在执行后被取消，副作用是否发生未知（§7.3 的 UNKNOWN_COMMIT）。
    unit.pending_action_ids = vec![ActionId::new("action:70").expect("固定动作")];

    assert_eq!(
        unit.transition_to(UnitState::Cold),
        Err(ContractError::UnresolvedPendingActions {
            to: "COLD",
            count: 1,
            sample: "action:70".to_string(),
        })
    );

    // 移交给在线动作账之后才允许降温。
    let handed_over = unit.drain_pending_for_action_ledger();
    assert_eq!(handed_over.len(), 1);
    unit.transition_to(UnitState::Cold)
        .expect("未决动作移交后可以降温");
}

#[test]
fn unit_can_always_be_faulted_or_quarantined_but_not_the_reverse_freely() {
    for state in UnitState::ALL {
        if state != UnitState::Faulted {
            assert!(
                state.can_transition_to(UnitState::Faulted),
                "{state:?} 必须能进入 FAULTED（§9.2 任意状态）"
            );
        }
        if state != UnitState::Quarantined {
            assert!(
                state.can_transition_to(UnitState::Quarantined),
                "{state:?} 必须能进入 QUARANTINED（§9.2 任意状态）"
            );
        }
        assert!(
            !state.can_transition_to(state),
            "{state:?} 迁移到自身不构成状态变化"
        );
    }

    // 隔离单元只能回到冷态，由注册表重新走一次带权限重验的加载流程。
    assert!(UnitState::Quarantined.can_transition_to(UnitState::Cold));
    assert!(!UnitState::Quarantined.can_transition_to(UnitState::Running));
    assert!(!UnitState::Cold.is_hot());
    assert!(UnitState::Running.is_hot());
}

// ---------------------------------------------------------------------------
// 标识与前缀
// ---------------------------------------------------------------------------

#[test]
fn ids_reject_wrong_prefixes_and_whitespace() {
    assert!(matches!(
        EvidenceRef::new("unit:file-summary:07"),
        Err(ContractError::MalformedId {
            kind: "evidence_ref",
            ..
        })
    ));
    assert!(EvidenceRef::new("obs:193").is_ok());
    assert!(EvidenceRef::new("tool-result:64").is_ok());
    assert!(matches!(
        UnitId::new("file-summary:07"),
        Err(ContractError::MalformedId { .. })
    ));
    assert!(matches!(
        SourceId::new("  device:screen  "),
        Err(ContractError::SurroundingWhitespace { .. })
    ));
    assert!(matches!(
        TaskId::new(""),
        Err(ContractError::EmptyField { field: "task_id" })
    ));
    assert!(matches!(
        EventId::parse("not-a-uuid"),
        Err(ContractError::MalformedUuid { .. })
    ));
    assert!(matches!(
        Sha256Hex::parse("ABC"),
        Err(ContractError::MalformedDigest { .. })
    ));
    assert_eq!(Sha256Hex::of_bytes(b"").as_str().len(), Sha256Hex::LEN);
}
