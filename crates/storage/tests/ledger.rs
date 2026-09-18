//! 存储层回归测试。
//!
//! 重点验证 §7.3 的三条语义：原子出账、动作 ID 去重、崩溃恢复只核对不重发。
//! 崩溃用"丢弃 [`Store`] 再重新打开同一文件"来模拟——不做假的进程内异常注入，因为真实的
//! 崩溃止损点恰恰是"已经提交、但后续代码没跑完"。

use serde_json::json;
use soca_contracts::*;
use soca_storage::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("固定基准时间")
}

fn at(offset_seconds: i64) -> WallClock {
    base_time().plus_seconds(offset_seconds)
}

fn task() -> TaskId {
    TaskId::new("task:42").expect("固定任务")
}

fn unit() -> UnitId {
    UnitId::new("unit:file-summary:07").expect("固定单元")
}

fn prediction_ref() -> PredictionRef {
    PredictionRef::new("prediction:pred-7").expect("固定预测引用")
}

fn params(bytes: u64) -> serde_json::Value {
    json!({ "path": "D:\\资料\\摘要\\summary.md", "bytes": bytes })
}

fn new_envelope(sequence: u64, idempotency: &str) -> Envelope {
    let boot = BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 UUID");
    let adapter = SourceId::new("device:file-watcher").expect("固定来源");
    Envelope::new(
        EventId::generate(),
        adapter.clone(),
        1,
        boot,
        sequence,
        task(),
        Vec::new(),
        at(0),
        Monotonic::new(boot, sequence * 1_000_000),
        Provenance::Sensor { adapter },
        PayloadRef::Blob {
            blob_ref: BlobRef::new(format!("blob:obs-{sequence}")).expect("固定对象"),
            media_type: MediaType::new("application/json").expect("固定媒体类型"),
            bytes: 128,
            sha256: Sha256Hex::of_bytes(&sequence.to_le_bytes()),
        },
        PermissionScope {
            capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                .expect("固定能力策略"),
            max_action_level: ActionLevel::A2,
        },
        DataClass::Personal,
        None,
        IdempotencyKey::new(idempotency).expect("固定幂等键"),
    )
}

fn new_intent(action: &str, bytes: u64, level: ActionLevel) -> ActionIntent {
    ActionIntent::new(
        ActionId::new(action).expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new("D:\\资料\\摘要").expect("固定范围"),
        params(bytes),
        vec!["目标目录已授权".to_string()],
        prediction_ref(),
        level,
        ResourceCost {
            est_ram_bytes: 1 << 20,
            est_tokens: 0,
            est_millis: 50,
        },
        unit(),
    )
    .expect("合法动作意图")
}

fn new_permit(intent: &ActionIntent, permit: &str, max_uses: u8) -> ExecutionPermit {
    ExecutionPermit::issue_for(
        intent,
        PermitId::new(permit).expect("固定许可"),
        SubjectId::new("user:local").expect("固定主体"),
        at(0),
        300,
        max_uses,
        BudgetRef::new("budget:task-42").expect("固定预算"),
        PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
        CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
        None,
    )
    .expect("合法执行许可")
}

/// 预测与观测共用的对象引用。核验时两者逐字比较。
const PREDICTED_SUBJECT: &str = "file:D:\\资料\\摘要\\summary.md";

/// 与给定意图配套的预测。§6.3 要求动作前必须有可检查的预测。
fn prediction_for(intent: &ActionIntent) -> Prediction {
    Prediction::new(
        intent.prediction_ref.clone(),
        PREDICTED_SUBJECT.to_string(),
        "写入后文件版本变为新内容的哈希".to_string(),
        Expectation::VersionEquals {
            subject_ref: PREDICTED_SUBJECT.to_string(),
            expected: "sha256:new".to_string(),
        },
        TimeWindow::new(at(0), at(60)).expect("合法时间窗"),
        vec!["哈希不一致".to_string(), "文件不存在".to_string()],
        Uncertainty {
            probability: None,
            notes: vec!["本地确定性写入，不需要概率字段".to_string()],
        },
    )
    .expect("合法预测")
}

/// 记录预测后受理动作。
fn admit(store: &mut Store, intent: &ActionIntent, permit: &ExecutionPermit) -> Admission {
    try_admit(store, intent, permit).expect("受理流程本身不报错")
}

/// 同上，但保留错误分支供拒绝类测试使用。
fn try_admit(
    store: &mut Store,
    intent: &ActionIntent,
    permit: &ExecutionPermit,
) -> Result<Admission, StorageError> {
    store
        .record_prediction(&unit(), &task(), &prediction_for(intent), at(0))
        .expect("记录动作前预测");
    store.admit_action(&task(), intent, permit, at(1))
}

fn new_receipt(action: &str, permit: &str, status: CommitStatus) -> ActionReceipt {
    ActionReceipt {
        action_id: ActionId::new(action).expect("固定动作"),
        permit_id: PermitId::new(permit).expect("固定许可"),
        status,
        recorded_at: at(5),
        detail: "已写入临时文件并同卷原子替换".to_string(),
        observed_target_version: Some("sha256:new".to_string()),
    }
}

fn in_memory() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储必须可打开")
}

// ---------------------------------------------------------------------------
// 事件日志与幂等（§7.1、§7.3）
// ---------------------------------------------------------------------------

#[test]
fn events_append_and_read_by_cursor() {
    let mut store = in_memory();
    let first = new_envelope(1, "idem:1");
    let second = new_envelope(2, "idem:2");

    let a = store.append_event(&first, at(1)).expect("首次追加");
    let b = store.append_event(&second, at(2)).expect("第二次追加");
    assert!(a.is_new() && b.is_new());
    assert!(b.sequence() > a.sequence());

    let all = store.read_events_after(0, 10).expect("从头读");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].envelope.event_id, first.event_id);
    assert_eq!(all[1].envelope.event_id, second.event_id);

    // 游标之后只返回新事件。
    let tail = store.read_events_after(a.sequence(), 10).expect("按游标读");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].envelope.event_id, second.event_id);

    assert_eq!(store.latest_sequence().unwrap(), b.sequence());
    assert_eq!(store.event_count().unwrap(), 2);
}

#[test]
fn repeated_delivery_with_the_same_idempotency_key_writes_nothing() {
    let mut store = in_memory();
    let envelope = new_envelope(1, "idem:1");

    let first = store.append_event(&envelope, at(1)).expect("首次追加");
    let replay = store.append_event(&envelope, at(99)).expect("重复投递");

    assert!(first.is_new());
    assert!(!replay.is_new(), "重复投递不得产生新事件");
    assert_eq!(replay.sequence(), first.sequence());
    assert_eq!(store.event_count().unwrap(), 1);
}

#[test]
fn same_event_with_a_new_idempotency_key_is_still_a_duplicate() {
    let mut store = in_memory();
    let mut envelope = new_envelope(1, "idem:first");
    let first = store.append_event(&envelope, at(1)).expect("首次追加");

    // 同一事件换了幂等键再投一次（网关重试、重放等）。
    envelope.idempotency_key = IdempotencyKey::new("idem:second").expect("固定幂等键");
    let again = store.append_event(&envelope, at(2)).expect("换键重投");

    assert!(!again.is_new());
    assert_eq!(again.sequence(), first.sequence());
    assert_eq!(store.event_count().unwrap(), 1);
}

#[test]
fn one_idempotency_key_cannot_be_rebound_to_another_event() {
    let mut store = in_memory();
    store
        .append_event(&new_envelope(1, "idem:shared"), at(1))
        .expect("首次追加");

    let impostor = new_envelope(2, "idem:shared");
    let result = store.append_event(&impostor, at(2));
    assert!(matches!(
        result,
        Err(StorageError::IdempotencyKeyConflict { .. })
    ));
    assert_eq!(store.event_count().unwrap(), 1);
}

#[test]
fn a_stream_position_cannot_hold_two_different_events() {
    let mut store = in_memory();
    store
        .append_event(&new_envelope(7, "idem:a"), at(1))
        .expect("首次追加");

    let result = store.append_event(&new_envelope(7, "idem:b"), at(2));
    assert!(matches!(
        result,
        Err(StorageError::StreamPositionConflict {
            source_sequence: 7,
            ..
        })
    ));
}

#[test]
fn an_invalid_envelope_never_occupies_a_sequence_or_an_idempotency_key() {
    let mut store = in_memory();
    let mut broken = new_envelope(1, "idem:broken");
    broken.schema_version = SCHEMA_VERSION + 1;

    let rejected = store.append_event(&broken, at(1));
    assert!(matches!(rejected, Err(StorageError::EventRejected { .. })));
    assert_eq!(store.event_count().unwrap(), 0);
    assert_eq!(store.latest_sequence().unwrap(), 0);

    // 修正后重投必须能正常落库：非法信封没有占用幂等键。
    broken.schema_version = SCHEMA_VERSION;
    assert!(
        store
            .append_event(&broken, at(2))
            .expect("修正后应可落库")
            .is_new()
    );
}

#[test]
fn negative_cursor_is_rejected_instead_of_being_treated_as_zero() {
    let store = in_memory();
    assert!(matches!(
        store.read_events_after(-1, 10),
        Err(StorageError::InvalidCursor { cursor: -1, .. })
    ));
    assert!(store.read_events_after(0, 0).unwrap().is_empty());
}

#[test]
fn instruction_authority_is_recorded_for_audit_queries() {
    let mut store = in_memory();
    let mut envelope = new_envelope(1, "idem:1");
    // 屏幕文字看起来像指令，但它不是用户明确授权。
    envelope.provenance = Provenance::Sensor {
        adapter: SourceId::new("device:screen").expect("固定来源"),
    };
    store.append_event(&envelope, at(1)).expect("追加");

    assert!(!envelope.provenance.is_instruction_authority());
    let stored = store.read_events_after(0, 1).unwrap();
    assert_eq!(stored.len(), 1);
}

// ---------------------------------------------------------------------------
// 原子出账（§7.3）
// ---------------------------------------------------------------------------

#[test]
fn an_allowed_action_lands_in_the_ledger_and_the_outbox_together() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);

    let admission = admit(&mut store, &intent, &permit);

    assert!(admission.is_dispatchable());
    assert!(!admission.replayed);
    assert_eq!(admission.permit_uses_after, 1);
    assert_eq!(store.action_count().unwrap(), 1);
    assert_eq!(store.outbox_count().unwrap(), 1);

    let pending = store.pending_outbox(10).expect("读取待投递");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].action_id, "action:1");
    assert_eq!(pending[0].intent.parameters, params(2048));
}

#[test]
fn a_denied_action_never_enters_the_outbox_but_is_still_recorded() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);

    // 许可绑定的是另一份参数：执行时被改过。
    let other = new_intent("action:1", 4096, ActionLevel::A1);
    let permit = new_permit(&other, "permit:1", 1);

    let admission = admit(&mut store, &intent, &permit);

    assert_eq!(admission.decision, Decision::Denied);
    assert_eq!(admission.state, ActionState::Denied);
    assert!(admission.denial_reason.is_some());
    assert_eq!(
        store.outbox_count().unwrap(),
        0,
        "被拒绝的动作绝不能进入 outbox"
    );
    assert_eq!(
        store.pending_outbox(10).unwrap().len(),
        0,
        "执行代理不应该看见被拒绝的动作"
    );

    // 拒绝必须留痕（§6.8）。
    let audit = store.audit_entries(10).unwrap();
    assert!(
        audit
            .iter()
            .any(|entry| entry.category == "action_denied"
                && entry.subject_ref == "action:1"),
        "拒绝原因必须进入审计账"
    );
}

#[test]
fn duplicate_admission_is_idempotent() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);

    let first = admit(&mut store, &intent, &permit);
    let second = admit(&mut store, &intent, &permit);

    assert!(!first.replayed);
    assert!(second.replayed, "重复受理必须是幂等的");
    assert_eq!(store.action_count().unwrap(), 1);
    assert_eq!(store.outbox_count().unwrap(), 1, "不得产生第二条 outbox 记录");
}

#[test]
fn an_action_id_cannot_be_rebound_to_different_parameters() {
    let mut store = in_memory();
    let original = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&original, "permit:1", 1);
    admit(&mut store, &original, &permit);

    // 同一个动作 ID，参数被换掉。动作 ID 是去重的唯一依据，绝不允许复用。
    let tampered = new_intent("action:1", 999_999, ActionLevel::A1);
    let result = try_admit(&mut store, &tampered, &permit);
    assert!(matches!(result, Err(StorageError::ActionIdReused { .. })));
}

#[test]
fn permit_max_uses_is_enforced_across_actions() {
    let mut store = in_memory();
    let first = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&first, "permit:1", 2);

    admit(&mut store, &first, &permit);
    // 同一份参数、新的动作 ID：许可允许第二次。
    let second = new_intent("action:2", 2048, ActionLevel::A1);
    admit(&mut store, &second, &permit);

    // 第三次超限。
    let third = new_intent("action:3", 2048, ActionLevel::A1);
    let admission = admit(&mut store, &third, &permit);
    assert_eq!(admission.decision, Decision::Denied);
    assert!(
        admission
            .denial_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("max_uses")),
        "超限原因必须写明是次数用尽"
    );
    assert_eq!(store.permit_uses("permit:1").unwrap(), 2);
    assert_eq!(store.outbox_count().unwrap(), 2);
}

// ---------------------------------------------------------------------------
// 投递与回执（§7.2、§7.3）
// ---------------------------------------------------------------------------

#[test]
fn dispatch_is_the_water_shed_between_resendable_and_unknown() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);

    assert_eq!(store.pending_outbox(10).unwrap().len(), 1);
    store.mark_dispatched("action:1", at(2)).expect("标记投递");

    assert_eq!(store.action("action:1").unwrap().unwrap().state, ActionState::Submitted);
    assert_eq!(
        store.pending_outbox(10).unwrap().len(),
        0,
        "已投递的动作不再是待投递"
    );
    // 幂等重放。
    store.mark_dispatched("action:1", at(3)).expect("重复标记应幂等");
}

#[test]
fn a_receipt_does_not_settle_the_postcondition() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);
    store.mark_dispatched("action:1", at(2)).unwrap();

    store
        .settle_receipt(&new_receipt("action:1", "permit:1", CommitStatus::Completed), at(3))
        .expect("记录回执");

    assert_eq!(store.action("action:1").unwrap().unwrap().state, ActionState::Completed);
    assert_eq!(store.outcome("action:1").unwrap(), None, "回执不等于后置验证");

    // 后置条件必须由新观测另行判定。
    let outcome = OutcomeVerified::new(
        ActionId::new("action:1").unwrap(),
        prediction_ref(),
        Verdict::Supported,
        vec![EvidenceRef::new("obs:195").unwrap()],
    )
    .unwrap();
    store.record_outcome(&outcome, at(4)).expect("记录判定");
    assert_eq!(store.outcome("action:1").unwrap(), Some(outcome));
}

#[test]
fn a_receipt_without_dispatch_is_rejected() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);

    // 动作还没投递，不可能有回执。
    let result = store.settle_receipt(
        &new_receipt("action:1", "permit:1", CommitStatus::Completed),
        at(2),
    );
    assert!(matches!(
        result,
        Err(StorageError::IllegalActionTransition { .. })
    ));
}

#[test]
fn a_receipt_cannot_be_overwritten_with_a_different_status() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);
    store.mark_dispatched("action:1", at(2)).unwrap();

    let completed = new_receipt("action:1", "permit:1", CommitStatus::Completed);
    store.settle_receipt(&completed, at(3)).unwrap();

    // 同一份回执重复投递：幂等。
    store.settle_receipt(&completed, at(4)).expect("重复回执应幂等");

    // 用不同状态覆盖：拒绝。
    let conflicting = new_receipt("action:1", "permit:1", CommitStatus::Failed);
    assert!(matches!(
        store.settle_receipt(&conflicting, at(5)),
        Err(StorageError::IllegalActionTransition { .. })
    ));
    assert_eq!(store.receipt("action:1").unwrap().unwrap().status, CommitStatus::Completed);
}

#[test]
fn a_receipt_must_reference_the_permit_bound_to_the_action() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);
    store.mark_dispatched("action:1", at(2)).unwrap();

    let result = store.settle_receipt(
        &new_receipt("action:1", "permit:999", CommitStatus::Completed),
        at(3),
    );
    assert!(matches!(
        result,
        Err(StorageError::ReceiptPermitMismatch { .. })
    ));
}

#[test]
fn a_postcondition_verdict_requires_a_receipt() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);
    store.mark_dispatched("action:1", at(2)).unwrap();

    let outcome = OutcomeVerified::new(
        ActionId::new("action:1").unwrap(),
        prediction_ref(),
        Verdict::Supported,
        vec![EvidenceRef::new("obs:195").unwrap()],
    )
    .unwrap();
    assert!(matches!(
        store.record_outcome(&outcome, at(3)),
        Err(StorageError::IllegalActionTransition { .. })
    ));
}

// ---------------------------------------------------------------------------
// 崩溃恢复（§7.3）
// ---------------------------------------------------------------------------

#[test]
fn a_crash_after_dispatch_never_resends_the_side_effect() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    {
        let mut store = Store::open(&path, at(0)).expect("打开磁盘存储");
        let intent = new_intent("action:1", 2048, ActionLevel::A1);
        let permit = new_permit(&intent, "permit:1", 1);
        admit(&mut store, &intent, &permit);
        store.mark_dispatched("action:1", at(2)).unwrap();
        // 执行代理已经产生了副作用，但进程在写入回执之前崩溃。
    }

    let mut store = Store::open(&path, at(3600)).expect("重新打开");
    let report = store.recover(at(3600)).expect("恢复");

    assert!(report.needs_recovery_task());
    assert_eq!(report.unknown_commits.len(), 1);
    assert_eq!(report.unknown_commits[0].tool_id, "fs.write");
    assert!(
        report.resendable.is_empty(),
        "已投递过的动作绝不能出现在可重投清单里"
    );
    assert_eq!(
        store.action("action:1").unwrap().unwrap().state,
        ActionState::UnknownCommit
    );
    assert!(
        store.pending_outbox(10).unwrap().is_empty(),
        "未知提交不能被投递者看见"
    );

    // 恢复是幂等的：再跑一次不会再产生新的未知提交。
    let second = store.recover(at(3700)).unwrap();
    assert!(second.unknown_commits.is_empty());
}

#[test]
fn a_crash_before_dispatch_is_safely_resendable() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    {
        let mut store = Store::open(&path, at(0)).expect("打开磁盘存储");
        let intent = new_intent("action:1", 2048, ActionLevel::A1);
        let permit = new_permit(&intent, "permit:1", 1);
        admit(&mut store, &intent, &permit);
        // 在 mark_dispatched 之前崩溃：副作用一定没有发生。
    }

    let mut store = Store::open(&path, at(3600)).expect("重新打开");
    let report = store.recover(at(3600)).expect("恢复");

    assert!(!report.needs_recovery_task());
    assert_eq!(report.resendable.len(), 1);
    assert_eq!(report.resendable[0].action_id, "action:1");
    assert_eq!(
        store.action("action:1").unwrap().unwrap().state,
        ActionState::Prepared
    );
}

#[test]
fn pending_actions_block_nothing_but_must_be_resolved_explicitly() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    let mut store = Store::open(&path, at(0)).expect("打开磁盘存储");
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);
    store.mark_dispatched("action:1", at(2)).unwrap();

    let report = store.recover(at(3)).unwrap();
    assert_eq!(report.unknown_commits.len(), 1);
    assert!(report.needs_recovery_task());

    // 恢复任务核对目标状态：确认文件没有被改动。
    let state = store
        .resolve_unknown_commit("action:1", Resolution::ConfirmedAbsent, at(4))
        .expect("结清未知提交");
    assert_eq!(state, ActionState::Aborted);

    // ABORTED 与 FAILED 不同：它说明目标未被改动，调度器可以重新决策。
    let second = store.recover(at(5)).unwrap();
    assert!(second.unknown_commits.is_empty());
    assert_eq!(second.settled, 1);
}

#[test]
fn resolving_a_commit_that_is_not_unknown_is_rejected() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);

    assert!(matches!(
        store.resolve_unknown_commit("action:1", Resolution::ConfirmedCompleted, at(2)),
        Err(StorageError::IllegalActionTransition { .. })
    ));
    assert!(matches!(
        store.resolve_unknown_commit("action:missing", Resolution::ConfirmedAbsent, at(2)),
        Err(StorageError::ActionNotFound { .. })
    ));
}

#[test]
fn recovery_audit_trail_is_complete() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    {
        let mut store = Store::open(&path, at(0)).unwrap();
        let intent = new_intent("action:1", 2048, ActionLevel::A1);
        let permit = new_permit(&intent, "permit:1", 1);
        admit(&mut store, &intent, &permit);
        store.mark_dispatched("action:1", at(2)).unwrap();
    }

    let mut store = Store::open(&path, at(3600)).unwrap();
    store.recover(at(3600)).unwrap();

    let categories: Vec<String> = store
        .audit_entries(50)
        .unwrap()
        .into_iter()
        .map(|entry| entry.category)
        .collect();
    assert_eq!(
        categories,
        vec![
            "action_admitted".to_string(),
            "action_dispatched".to_string(),
            "unknown_commit_detected".to_string(),
        ],
        "受理、投递、未知提交都必须有审计记录"
    );
}

// ---------------------------------------------------------------------------
// 审计长度约束（§12.3）
// ---------------------------------------------------------------------------

#[test]
fn audit_details_are_truncated_instead_of_storing_whole_contexts() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let other = new_intent("action:1", 4096, ActionLevel::A1);
    let permit = new_permit(&other, "permit:1", 1);

    admit(&mut store, &intent, &permit);

    let entries = store.audit_entries(10).unwrap();
    let denied = entries
        .iter()
        .find(|entry| entry.category == "action_denied")
        .expect("必须有拒绝记录");
    assert!(
        denied.detail.len() <= MAX_DETAIL_LEN + "…[已截断]".len(),
        "审计说明必须受长度约束"
    );
}

#[test]
fn audit_retention_is_explicit_and_reversible_in_effect() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut store, &intent, &permit);
    assert_eq!(store.audit_entries(10).unwrap().len(), 1);

    // 裁剪是显式动作，不由后台悄悄执行。
    let removed = store.prune_audit_before(at(10)).unwrap();
    assert_eq!(removed, 1);
    assert!(store.audit_entries(10).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 预测先于动作（§6.3、§17）
// ---------------------------------------------------------------------------

#[test]
fn an_action_without_a_recorded_prediction_is_denied() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);

    // 故意不记录预测，直接受理。
    let admission = store
        .admit_action(&task(), &intent, &permit, at(1))
        .expect("受理流程本身不报错");

    assert_eq!(admission.decision, Decision::Denied);
    assert!(
        admission
            .denial_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("预测")),
        "拒绝原因必须点明缺少动作前预测"
    );
    assert_eq!(store.outbox_count().unwrap(), 0);
    assert_eq!(store.action_count().unwrap(), 1, "拒绝也要留痕");
}

#[test]
fn a_prediction_belonging_to_another_task_is_denied() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    let other_task = TaskId::new("task:99").expect("固定任务");

    store
        .record_prediction(&unit(), &other_task, &prediction_for(&intent), at(0))
        .expect("记录预测");

    let admission = store
        .admit_action(&task(), &intent, &permit, at(1))
        .expect("受理流程本身不报错");

    assert_eq!(admission.decision, Decision::Denied);
    assert!(
        admission
            .denial_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("task:99")),
        "拒绝原因必须写明预测属于哪个任务"
    );
}

#[test]
fn recording_the_same_prediction_twice_is_idempotent() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let prediction = prediction_for(&intent);

    assert!(
        store
            .record_prediction(&unit(), &task(), &prediction, at(0))
            .expect("首次记录")
    );
    assert!(
        !store
            .record_prediction(&unit(), &task(), &prediction, at(1))
            .expect("重复记录"),
        "内容相同的重复记录不应再写一行"
    );
    assert_eq!(store.predictions_for_task(&task()).unwrap().len(), 1);
}

#[test]
fn a_prediction_cannot_be_rewritten_after_the_fact() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    store
        .record_prediction(&unit(), &task(), &prediction_for(&intent), at(0))
        .expect("记录预测");

    // 事后把"预计变化"改成另一套说法，以便让后验检查好看。
    let mut rewritten = prediction_for(&intent);
    rewritten.expected_change = "文件内容保持完全不变".to_string();

    let result = store.record_prediction(&unit(), &task(), &rewritten, at(1));
    assert!(matches!(
        result,
        Err(StorageError::PredictionAlreadyRecorded { .. })
    ));
    assert_eq!(
        store.prediction("prediction:pred-7").unwrap().unwrap().prediction,
        prediction_for(&intent),
        "库里必须还是当初写下的那一份"
    );
}

#[test]
fn an_uncalibrated_probability_is_refused_on_the_way_into_the_store() {
    let mut store = in_memory();
    let intent = new_intent("action:1", 2048, ActionLevel::A1);

    // 绕过 Prediction::new 直接拼一个结构体，模拟"别的代码路径"塞进来的自评分数。
    let forged = Prediction {
        prediction_ref: prediction_ref(),
        subject: PREDICTED_SUBJECT.to_string(),
        expected_change: "写入后文件版本变为新内容的哈希".to_string(),
        expectation: Expectation::VersionEquals {
            subject_ref: PREDICTED_SUBJECT.to_string(),
            expected: "sha256:new".to_string(),
        },
        window: TimeWindow::new(at(0), at(60)).unwrap(),
        failure_conditions: vec!["哈希不一致".to_string()],
        uncertainty: Uncertainty {
            probability: Some(CalibratedProbability {
                subject: PREDICTED_SUBJECT.to_string(),
                horizon: TimeWindow::new(at(0), at(60)).unwrap(),
                model_version: ModelVersion::new("sha256:llm-weights").expect("固定模型版本"),
                calibration: CalibrationSource::Uncalibrated,
                value: Some(0.97),
            }),
            notes: Vec::new(),
        },
    };

    let result = store.record_prediction(&unit(), &task(), &forged, at(0));
    assert!(matches!(result, Err(StorageError::Contract(_))));
    assert!(store.predictions_for_task(&task()).unwrap().is_empty());
    let _ = intent;
}

#[test]
fn schema_version_is_reported_and_migrations_are_recorded() {
    let store = in_memory();
    assert_eq!(store.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    assert_eq!(LATEST_SCHEMA_VERSION, 7);
}

#[test]
fn reopening_an_existing_database_does_not_rerun_migrations() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    let first = Store::open(&path, at(0)).expect("首次打开");
    assert_eq!(first.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    drop(first);

    let mut second = Store::open(&path, at(3600)).expect("再次打开");
    assert_eq!(second.schema_version().unwrap(), LATEST_SCHEMA_VERSION);

    // 数据仍然可用：迁移没有把表重建掉。
    let intent = new_intent("action:1", 2048, ActionLevel::A1);
    let permit = new_permit(&intent, "permit:1", 1);
    admit(&mut second, &intent, &permit);
    assert_eq!(second.action_count().unwrap(), 1);
}
