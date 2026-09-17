//! 确定性闭环与轨迹重放回归测试（P0 门槛）。
//!
//! §16 给出的 P0 通过条件只有一句话："能重放'请求≠执行≠验证'的轨迹"。本文件把它拆成
//! 一组可判定的断言，并且刻意让每一步都停在半路一次：只有每一步独立可调用，故障注入点
//! 才能被精确表达。

use serde_json::json;
use soca_contracts::*;
use soca_core::*;
use soca_storage::{Store, StorageError};
use tempfile::TempDir;

const SUBJECT_PATH: &str = "D:\\资料\\摘要\\summary.md";
const PERMITTED_SCOPE: &str = "D:\\资料\\摘要";
const BOOT: &str = "00000000-0000-4000-8000-0000000000a1";

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("固定基准时间")
}

fn at(seconds: i64) -> WallClock {
    base_time().plus_seconds(seconds)
}

fn boot() -> BootId {
    BootId::parse(BOOT).expect("固定 UUID")
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

fn subject_ref() -> String {
    format!("file:{SUBJECT_PATH}")
}

fn temp_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("临时目录");
    let store = Store::open(dir.path().join("soca.db"), at(0)).expect("打开存储");
    (dir, store)
}

fn intent(action: &str, content: &str, level: ActionLevel) -> ActionIntent {
    ActionIntent::new(
        ActionId::new(action).expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new(PERMITTED_SCOPE).expect("固定范围"),
        json!({ "path": SUBJECT_PATH, "content": content }),
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

fn permit_for(intent: &ActionIntent, permit: &str, max_uses: u8) -> ExecutionPermit {
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
        if intent.risk.requires_approval_id() {
            Some(ApprovalId::new("approval:3").expect("固定审批"))
        } else {
            None
        },
    )
    .expect("合法执行许可")
}

fn broker_with(content: &str) -> ActionBroker {
    let mut os = SimulatedOs::new();
    os.seed(&subject_ref(), content);
    ActionBroker::new(os)
}

// ---------------------------------------------------------------------------
// 一轮完整闭环（§6）
// ---------------------------------------------------------------------------

#[test]
fn a_write_round_keeps_request_execution_and_verification_as_separate_facts() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);

    let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
    let report = session
        .run_write_round(&write, &permit, at(0))
        .expect("闭环应完整跑通");

    // 请求：只是一个被放行的意图，本身没有改变任何东西。
    assert!(report.admission.is_dispatchable());
    assert!(!report.admission.replayed);

    // 执行：产生了回执，但回执不等于验证。
    let receipt = report.receipt.expect("应拿到回执");
    assert_eq!(receipt.status, CommitStatus::Completed);
    assert!(
        !receipt.is_postcondition_verified(),
        "回执不得被当成后置条件验证"
    );

    // 验证：来自动作之后的一次新观测。
    let outcome = report.outcome.expect("应有后置条件判定");
    assert_eq!(outcome.verdict, Verdict::Supported);
    assert_eq!(outcome.observation_refs.len(), 1);
    assert!(!outcome.grants_new_permission());

    // 观测确实发生在动作之后，且值变了。
    let before = &report.observation_before.observation;
    let after = &report.observation_after.expect("应有动作后观测").observation;
    assert_ne!(before.value, after.value, "写入之后版本必须变化");
    assert_eq!(
        before.value,
        Sha256Hex::of_bytes("原始内容".as_bytes()).to_string()
    );
    assert_eq!(after.value, Sha256Hex::of_bytes("新的摘要内容".as_bytes()).to_string());

    // 世界确实只被改了一次。
    assert_eq!(broker.os().applications(), 1);
    assert_eq!(broker.os().read(&subject_ref()).unwrap().writes, 1);
}

#[test]
fn a_request_that_fails_authorization_never_reaches_the_os() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let signed = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&signed, "permit:1", 1);

    // 批准的是"新的摘要内容"，执行时换成了别的内容：参数摘要一变，许可立即失效。
    let tampered = intent("action:1", "被篡改的内容", ActionLevel::A2);

    let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
    let result = session.run_write_round(&tampered, &permit, at(0));

    assert!(matches!(result, Err(CoreError::AdmissionDenied { .. })));
    assert_eq!(broker.os().applications(), 0, "被拒绝的请求不得产生副作用");
    assert!(
        broker.os().attempts().is_empty(),
        "执行代理根本不应该被调用"
    );
    assert_eq!(broker.os().read(&subject_ref()).unwrap().content, "原始内容");
    // 拒绝也要留痕（§6.8）。
    assert!(
        store
            .audit_entries(20)
            .unwrap()
            .iter()
            .any(|entry| entry.category == "action_denied")
    );
}

#[test]
fn an_unavailable_broker_refuses_instead_of_degrading_to_allow() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");
    // §12.2：Broker 故障、策略不可读、审计写失败、磁盘满或审批过期时，默认拒绝新副作用。
    broker.set_unavailable("策略文件不可读");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);

    let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
    let report = session
        .run_write_round(&write, &permit, at(0))
        .expect("受理与投递本身应当有确定结果");

    assert!(matches!(
        report.dispatch,
        DispatchOutcome::Refused { .. }
    ));
    assert!(!report.dispatch.did_apply());
    assert_eq!(broker.os().applications(), 0);
    assert!(report.receipt.is_none());
    assert!(report.outcome.is_none());
}

// ---------------------------------------------------------------------------
// 只有执行许可能到 OS（§7.2、§12.2）
// ---------------------------------------------------------------------------

#[test]
fn none_of_the_other_six_data_states_can_reach_the_os() {
    let mut broker = broker_with("原始内容");
    let write = intent("action:1", "新的摘要内容", ActionLevel::A1);
    let good_permit = permit_for(&write, "permit:1", 1);

    let observation = Observation {
        subject: subject_ref(),
        value: "某个版本".to_string(),
        evidence_ref: EvidenceRef::new("obs:1").expect("固定证据"),
        derived_from: Vec::new(),
        observed_by: unit(),
    };
    let receipt = ActionReceipt {
        action_id: write.action_id.clone(),
        permit_id: good_permit.permit_id.clone(),
        status: CommitStatus::Completed,
        recorded_at: at(0),
        detail: String::new(),
        observed_target_version: None,
    };
    let outcome = OutcomeVerified::new(
        write.action_id.clone(),
        prediction_ref(),
        Verdict::Supported,
        vec![EvidenceRef::new("obs:2").expect("固定证据")],
    )
    .expect("合法判定");
    let hypothesis = Hypothesis {
        claim: "文件未被修改".to_string(),
        supporting: vec![EvidenceRef::new("obs:1").expect("固定证据")],
        against: Vec::new(),
        unknowns: Vec::new(),
        proposed_by: unit(),
    };
    let prediction = Prediction::new(
        prediction_ref(),
        subject_ref(),
        "版本不变".to_string(),
        Expectation::VersionEquals {
            subject_ref: subject_ref(),
            expected: "某个版本".to_string(),
        },
        TimeWindow::new(at(0), at(60)).expect("合法时间窗"),
        vec!["版本变化".to_string()],
        Uncertainty {
            probability: None,
            notes: Vec::new(),
        },
    )
    .expect("合法预测");

    let blocked = vec![
        DataState::Observation(observation),
        DataState::Hypothesis(hypothesis),
        DataState::Prediction(prediction),
        DataState::ActionIntent(write.clone()),
        DataState::ActionReceipt(receipt),
        DataState::OutcomeVerified(outcome),
    ];

    for state in &blocked {
        assert!(
            !state.may_trigger_side_effect(),
            "{:?} 不应具有副作用权限",
            state.kind()
        );
        let result = broker.submit(state, &write, at(0));
        assert!(
            matches!(result, Err(BrokerError::NotAnExecutionPermit { .. })),
            "{:?} 不应被接受为执行触发条件",
            state.kind()
        );
    }

    assert_eq!(broker.os().applications(), 0);
    assert!(
        broker.os().attempts().is_empty(),
        "模拟 OS 一次都不应被调用"
    );

    // 对照组：把许可包成 DataState 就能通过权限判定这一关（但许可绑定的是别的动作，
    // 所以仍然会被参数摘要挡住）。
    let other = intent("action:9", "另一份内容", ActionLevel::A1);
    let refusal = broker
        .submit(&DataState::ExecutionPermit(good_permit), &other, at(0))
        .expect("许可本身是合法的触发条件");
    assert!(matches!(refusal, BrokerOutcome::Refused { .. }));
    assert_eq!(broker.os().applications(), 0);
}

// ---------------------------------------------------------------------------
// 崩溃与恢复（§7.3）
// ---------------------------------------------------------------------------

#[test]
fn an_interrupted_write_is_never_silently_resent() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);

    // 副作用已经应用，但调用方拿不到回执——§7.3 的 UNKNOWN_COMMIT 场景。
    let mut broker = ActionBroker::new(SimulatedOs::with_faults(FaultPlan {
        interrupt_after_application: Some(1),
        fail_before_application: None,
    }));
    broker.os_mut().seed(&subject_ref(), "原始内容");

    {
        let mut store = Store::open(&path, at(0)).expect("打开存储");
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        let interrupted = session
            .run_write_round(&write, &permit, at(0))
            .expect("结果未知本身是确定的结果");
        assert!(matches!(
            interrupted.dispatch,
            DispatchOutcome::Interrupted { .. }
        ));
        assert!(interrupted.receipt.is_none(), "断电不产生回执");
        assert!(interrupted.outcome.is_none(), "没有回执就没有可验证的后置条件");
        // 世界已经被改过了。
        assert_eq!(broker.os().applications(), 1);
        // 进程在这里崩溃。
    }

    let mut store = Store::open(&path, at(3600)).expect("重新打开");
    let report = store.recover(at(3600)).expect("恢复");

    assert!(report.needs_recovery_task());
    assert_eq!(report.unknown_commits.len(), 1);
    assert!(
        report.resendable.is_empty(),
        "已投递的动作绝不能出现在可重投清单里"
    );
    assert!(
        store.pending_outbox(10).unwrap().is_empty(),
        "投递者看不到未知提交"
    );

    // 重放读到的事实是"结果未知"，而不是"完成"。
    let trajectory = Trajectory::load(&store, &task(), 50).expect("重放");
    assert!(trajectory.is_consistent());
    assert!(trajectory.touched_the_world());
    let only = &trajectory.actions[0];
    assert_eq!(only.record.state, soca_storage::ActionState::UnknownCommit);
    assert!(only.dispatched_at.is_some());
    assert!(only.receipt.is_none());
}

#[test]
fn a_crash_before_dispatch_leaves_the_action_safely_resendable() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);
    let mut broker = broker_with("原始内容");

    {
        let mut store = Store::open(&path, at(0)).expect("打开存储");
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        // §6.3 走到动作前就停：预测已记录，动作已进入 outbox，但从未投递。
        session.observe(&subject_ref(), at(0)).expect("观测");
        assert!(
            session
                .predict(
                    &Prediction::new(
                        prediction_ref(),
                        subject_ref(),
                        format!("{} 的版本已更新", subject_ref()),
                        Expectation::VersionEquals {
                            subject_ref: subject_ref(),
                            expected: Sha256Hex::of_bytes("新的摘要内容".as_bytes()).to_string(),
                        },
                        TimeWindow::new(at(0), at(60)).unwrap(),
                        vec!["版本不符".to_string()],
                        Uncertainty { probability: None, notes: Vec::new() },
                    )
                    .unwrap(),
                    at(0)
                )
                .expect("记录预测")
        );
        let admission = session.admit(&write, &permit, at(0)).expect("受理");
        assert!(admission.is_dispatchable());
        // 进程在这里崩溃。
    }

    let mut store = Store::open(&path, at(3600)).expect("重新打开");
    let report = store.recover(at(3600)).expect("恢复");

    assert!(!report.needs_recovery_task());
    assert_eq!(report.resendable.len(), 1);
    assert_eq!(report.resendable[0].action_id, "action:1");
    assert_eq!(broker.os().applications(), 0, "模拟 OS 从未被调用");
}

// ---------------------------------------------------------------------------
// 轨迹重放（§7.3）
// ---------------------------------------------------------------------------

#[test]
fn replay_reads_the_ledger_without_touching_the_os() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);
    {
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        session
            .run_write_round(&write, &permit, at(0))
            .expect("闭环完成");
    }

    let applications_before = broker.os().applications();
    let writes_before = broker.os().read(&subject_ref()).unwrap().writes;
    broker.os_mut().reset_attempts();

    let trajectory = Trajectory::load(&store, &task(), 50).expect("重放");

    // 重放的全部输入只有存储句柄：它拿不到执行代理，也就没有第二次执行的可能。
    assert!(
        broker.os().attempts().is_empty(),
        "重放不得调用模拟 OS 一次"
    );
    assert_eq!(broker.os().applications(), applications_before);
    assert_eq!(broker.os().read(&subject_ref()).unwrap().writes, writes_before);

    // 但重放确实读回了全部事实。
    assert!(trajectory.is_consistent());
    assert_eq!(trajectory.observations.len(), 2, "动作前后各一次观测");
    assert_eq!(trajectory.predictions.len(), 1);
    assert_eq!(trajectory.action_count(), 1);

    let only = &trajectory.actions[0];
    assert_eq!(only.record.action_id, "action:1");
    assert!(only.prediction.is_some(), "重放要能看到动作前的预测原文");
    assert!(only.dispatched_at.is_some());
    assert_eq!(
        only.receipt.as_ref().unwrap().status,
        CommitStatus::Completed
    );
    assert_eq!(only.outcome.as_ref().unwrap().verdict, Verdict::Supported);
    assert!(!only.receipt.as_ref().unwrap().is_postcondition_verified());

    // 再重放一次必须得到完全相同的结果。
    let again = Trajectory::load(&store, &task(), 50).expect("再次重放");
    assert_eq!(trajectory, again);
}

#[test]
fn replay_reports_violations_instead_of_hiding_them() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);
    {
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        session
            .run_write_round(&write, &permit, at(0))
            .expect("闭环完成");
    }

    let consistent = Trajectory::load(&store, &task(), 50).expect("重放");
    assert!(consistent.violations().is_empty());

    // 人为构造一份自相矛盾的轨迹：有回执但从未投递。
    let mut broken = consistent.clone();
    broken.actions[0].dispatched_at = None;
    assert_eq!(
        broken.violations(),
        vec![ReplayViolation::ExecutedWithoutDispatch {
            action_id: "action:1".to_string()
        }]
    );

    // 有后置判定但没有回执。
    let mut missing_receipt = consistent.clone();
    missing_receipt.actions[0].receipt = None;
    assert!(
        missing_receipt
            .violations()
            .contains(&ReplayViolation::OutcomeWithoutReceipt {
                action_id: "action:1".to_string()
            })
    );

    // 被拒绝的动作不该有回执。
    let mut denied_with_receipt = consistent.clone();
    denied_with_receipt.actions[0].record.decision = soca_storage::Decision::Denied;
    assert!(
        denied_with_receipt
            .violations()
            .contains(&ReplayViolation::DeniedActionHasReceipt {
                action_id: "action:1".to_string()
            })
    );

    // 放行的动作找不到预测。
    let mut orphan = consistent.clone();
    orphan.actions[0].prediction = None;
    assert!(
        orphan
            .violations()
            .contains(&ReplayViolation::AllowedActionWithoutPrediction {
                action_id: "action:1".to_string()
            })
    );
}

#[test]
fn replay_can_be_scoped_to_a_single_task() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);
    {
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        session
            .run_write_round(&write, &permit, at(0))
            .expect("闭环完成");
    }

    let other = TaskId::new("task:99").expect("固定任务");
    let empty = Trajectory::load(&store, &other, 50).expect("另一个任务的重放");
    assert_eq!(empty.action_count(), 0);
    assert!(empty.observations.is_empty());
    assert!(!empty.touched_the_world());
}

// ---------------------------------------------------------------------------
// 判定规则（§6.7）
// ---------------------------------------------------------------------------

#[test]
fn a_prediction_that_does_not_hold_is_refuted_by_a_new_observation() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 1);
    let subject = subject_ref();
    let expected = Sha256Hex::of_bytes("新的摘要内容".as_bytes()).to_string();

    let prediction = Prediction::new(
        prediction_ref(),
        subject.clone(),
        format!("{subject} 的版本变为 {expected}"),
        Expectation::VersionEquals {
            subject_ref: subject.clone(),
            expected: expected.clone(),
        },
        TimeWindow::new(at(0), at(60)).expect("合法时间窗"),
        vec![format!("{subject} 的版本不是 {expected}")],
        Uncertainty {
            probability: None,
            notes: Vec::new(),
        },
    )
    .expect("合法预测");

    {
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        session.observe(&subject, at(0)).expect("动作前观测");
        session.predict(&prediction, at(0)).expect("记录预测");
        session.admit(&write, &permit, at(0)).expect("受理");
        let dispatched = session.dispatch(&write, &permit, at(0)).expect("投递");
        let DispatchOutcome::Receipted(receipt) = dispatched else {
            panic!("本用例不注入故障，应当拿到回执");
        };
        // 后置条件判定必须基于一条既有的回执（§7.2）。
        session.settle(&receipt, at(0)).expect("记录回执");
    }

    // 回执之后，外部世界又发生了变化：目标文件被另一个进程覆盖。
    broker.os_mut().seed(&subject, "被别人改过的内容");

    let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
    let after = session.observe(&subject, at(1)).expect("动作后观测");
    let outcome = session
        .verify(
            &write.action_id,
            &prediction.prediction_ref,
            &prediction.expectation,
            &after.observation,
            at(1),
        )
        .expect("判定");

    assert_eq!(outcome.verdict, Verdict::Refuted);
}

#[test]
fn evaluation_is_inconclusive_when_the_observation_is_about_another_object() {
    let expectation = Expectation::VersionEquals {
        subject_ref: subject_ref(),
        expected: "abc".to_string(),
    };
    let other_object = Observation {
        subject: "file:D:\\别的目录\\other.md".to_string(),
        value: "abc".to_string(),
        evidence_ref: EvidenceRef::new("obs:9").expect("固定证据"),
        derived_from: Vec::new(),
        observed_by: unit(),
    };
    // §6.7：若受外部变化影响则标记 inconclusive，而不是勉强给出支持或否定。
    assert_eq!(evaluate(&expectation, &other_object), Verdict::Inconclusive);

    let absent = Observation {
        subject: subject_ref(),
        value: ABSENT_VALUE.to_string(),
        evidence_ref: EvidenceRef::new("obs:10").expect("固定证据"),
        derived_from: Vec::new(),
        observed_by: unit(),
    };
    assert_eq!(evaluate(&expectation, &absent), Verdict::Refuted);
    assert_eq!(
        evaluate(
            &Expectation::Absent {
                subject_ref: subject_ref()
            },
            &absent
        ),
        Verdict::Supported
    );
}

// ---------------------------------------------------------------------------
// 动作级幂等（§7.3）
// ---------------------------------------------------------------------------

#[test]
fn the_simulated_os_applies_each_action_id_at_most_once() {
    let mut os = SimulatedOs::new();
    os.seed(&subject_ref(), "原始内容");
    let write = intent("action:1", "新的摘要内容", ActionLevel::A1);

    let first = os.execute(&write);
    assert!(first.applied());
    assert_eq!(os.applications(), 1);

    // 同一动作 ID 再投一次（至少一次投递语义下的必然结果）。
    let second = os.execute(&write);
    assert!(matches!(
        second,
        AttemptOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(os.applications(), 1, "同一动作 ID 不得产生第二次副作用");
    assert_eq!(os.read(&subject_ref()).unwrap().writes, 1);
    assert_eq!(os.attempts().len(), 2, "两次尝试都要留痕");
}

#[test]
fn a_persisted_action_id_cannot_be_reused_for_another_write() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let write = intent("action:1", "新的摘要内容", ActionLevel::A2);
    let permit = permit_for(&write, "permit:1", 2);
    {
        let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
        session
            .run_write_round(&write, &permit, at(0))
            .expect("闭环完成");
    }

    // 换一份内容但沿用同一个动作 ID。
    let reused = intent("action:1", "另一份内容", ActionLevel::A2);
    let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
    let result = session.admit(&reused, &permit, at(1));
    assert!(matches!(
        result,
        Err(CoreError::Storage(StorageError::ActionIdReused { .. }))
    ));
}

// ---------------------------------------------------------------------------
// 观测来源（§6.1、§11.1）
// ---------------------------------------------------------------------------

#[test]
fn observations_are_recorded_as_data_and_never_as_instructions() {
    let (_dir, mut store) = temp_store();
    let mut broker = broker_with("原始内容");

    let mut session = Session::new(&mut store, &mut broker, unit(), task(), boot());
    let record = session.observe(&subject_ref(), at(0)).expect("观测");

    let event = &store
        .events_for_task(&task(), 10)
        .expect("读取事件")
        .into_iter()
        .find(|event| event.sequence == record.sequence)
        .expect("必须能读回观测事件");

    assert!(
        !event.envelope.provenance.is_instruction_authority(),
        "传感器观测不得具有指令权限"
    );
    assert_eq!(event.envelope.data_class, DataClass::Personal);
    assert_eq!(
        event.envelope.cloud_egress_verdict(),
        EgressVerdict::Denied,
        "个人数据类别不得出站（§8）"
    );
}
