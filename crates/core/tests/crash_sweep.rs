//! §17 的崩溃恢复验收：100 个以上故障注入点，"已提交副作用不因重放重复执行"。
//!
//! §17 那一行是：
//!
//! > **崩溃恢复** | 至少100个故障注入点，覆盖意图前/后、执行后回执前、快照提交前/后；
//! > **已提交副作用不因重放重复执行**
//!
//! 前半句说的是**覆盖**，不是"测了 100 次同一个位置"。所以这里是三维扫描：
//!
//! * **停在哪一步**——§6 的 observe / predict / admit / dispatch / settle 之后各断一次。
//!   这就是"意图前 / 意图后 / 执行后回执前"那几种。
//! * **执行侧怎么坏**——模拟 OS 的两类故障：应用之后中断、应用之前失败。
//! * **写之前看过几次**——0、1、2 次。它变的是 outbox 之前的历史长度，而"恢复只读历史"
//!   这条要求在历史长短不同的情况下都要成立。
//!
//! 每一项都断言同样四条。**逐项断言而不是最后汇总**，是因为故障注入的价值恰恰在于
//! "哪一个位置出了事"——汇总之后那个信息就没了。
//!
//! 一处说明：进程崩溃是用"丢掉会话与存储句柄、保留模拟 OS"来表示的。真实的进程崩溃还会
//! 丢掉内存里的 OS 状态，但那已经超出模拟 OS 能表达的范围；这里能测的是**持久层那一半**
//! ——已提交的执行有没有因为重放而被再做一次。

use serde_json::json;
use soca_contracts::*;
use soca_core::*;
use soca_storage::{ContentStore, Resolution, Store};
use tempfile::TempDir;

const SUBJECT_PATH: &str = "D:\\资料\\摘要\\summary.md";
const PERMITTED_SCOPE: &str = "D:\\资料\\摘要";
const BOOT: &str = "00000000-0000-4000-8000-0000000000a1";

fn at(seconds: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("固定基准时间")
        .plus_seconds(seconds)
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

fn subject_ref() -> String {
    format!("file:{SUBJECT_PATH}")
}

fn intent(content: &str) -> ActionIntent {
    ActionIntent::new(
        ActionId::new("action:1").expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new(PERMITTED_SCOPE).expect("固定范围"),
        json!({ "path": SUBJECT_PATH, "content": content }),
        vec!["目标目录已授权".to_string()],
        PredictionRef::new("prediction:pred-7").expect("固定预测引用"),
        ActionLevel::A2,
        ResourceCost {
            est_ram_bytes: 1 << 20,
            est_tokens: 0,
            est_millis: 50,
        },
        unit(),
    )
    .expect("合法动作意图")
}

fn permit_for(intent: &ActionIntent) -> ExecutionPermit {
    ExecutionPermit::issue_for(
        intent,
        PermitId::new("permit:1").expect("固定许可"),
        SubjectId::new("user:local").expect("固定主体"),
        at(0),
        300,
        2,
        BudgetRef::new("budget:task-42").expect("固定预算"),
        PolicyVersion::new("policy-2026-09-17.1").expect("固定策略版本"),
        CapabilityPolicyRef::new("cap:read-selected-folder").expect("固定能力策略"),
        Some(ApprovalId::new("approval:3").expect("固定审批")),
    )
    .expect("合法执行许可")
}

fn prediction(subject: &str, content: &str) -> Prediction {
    let expected = Sha256Hex::of_bytes(content.as_bytes()).to_string();
    Prediction::new(
        PredictionRef::new("prediction:pred-7").expect("固定预测引用"),
        subject.to_string(),
        format!("{subject} 的版本变为 {expected}"),
        Expectation::VersionEquals {
            subject_ref: subject.to_string(),
            expected: expected.clone(),
        },
        TimeWindow::new(at(0), at(60)).expect("合法时间窗"),
        vec![format!("{subject} 的版本不是 {expected}")],
        Uncertainty {
            probability: None,
            notes: Vec::new(),
        },
    )
    .expect("合法预测")
}

/// 一个注入点。
#[derive(Clone, Copy, Debug)]
struct Injection {
    /// 执行到第几步之后断电（0 表示一步都没走）。
    stop_after: usize,
    /// 应用之后第几次中断。`None` 表示不中断。
    interrupt_after: Option<usize>,
    /// 应用之前先失败几次。
    fail_before: Option<usize>,
    /// 写之前看过几次。
    prior_observations: usize,
}

impl Injection {
    /// 一句能印进断言消息的话。
    fn label(self) -> String {
        format!(
            "停在第 {} 步、中断={:?}、先失败={:?}、先前观测 {} 次",
            self.stop_after, self.interrupt_after, self.fail_before, self.prior_observations
        )
    }
}

/// 一个注入点跑完之后的观察结果。
#[derive(Debug)]
struct Outcome {
    /// 模拟 OS 实际应用了几次副作用。
    applications: usize,
    /// 回执状态。`None` 表示没拿到回执。
    ///
    /// 存**状态**而不是一个布尔：回执说"失败"与"回执说成功"是两件事，而两者都不是
    /// "没拿到回执"。把它压成一个布尔，就分不出"中断（做完了但拿不到回执）"与
    /// "执行前失败（确实什么都没做）"——而这两个正是崩溃恢复里最需要分开的一对。
    receipt: Option<String>,
    /// 有没有把动作投递出去（投递过才谈得上"已投递不得重投"）。
    dispatched: bool,
    /// 恢复报告里的未知提交数。
    unknown_commits: usize,
    /// 恢复报告里可安全重投的条数。
    resendable: usize,
    /// 恢复之后仍在待投清单里的条数。
    pending: usize,
}

/// 跑一个注入点。
fn run(injection: Injection, path: &std::path::Path, content_store: &ContentStore, content: &str) -> Outcome {
    let subject = subject_ref();
    let write = intent(content);
    let permit = permit_for(&write);
    let expectation = prediction(&subject, content);

    // 模拟 OS 跨"崩溃"保留：它代表外部世界，而外部世界不会跟着进程一起消失。
    let mut os = SimulatedOs::with_faults(FaultPlan {
        interrupt_after_application: injection.interrupt_after,
        fail_before_application: injection.fail_before,
    });
    os.seed(&subject, "原始内容");
    let mut broker = ActionBroker::new(os);


    let (applications_at_crash, receipt_at_crash, dispatched) = {
        let mut receipt_at_crash: Option<String> = None;
        let mut dispatched = false;
        let mut store = Store::open(path, at(0)).expect("打开存储");
        let mut session =
            Session::new(&mut store, &mut broker, content_store, unit(), task(), boot());

        for round in 0..injection.prior_observations {
            session.observe(&subject, at(round as i64)).expect("先前观测");
        }
        if injection.stop_after >= 1 {
            session.observe(&subject, at(100)).expect("观测");
        }
        if injection.stop_after >= 2 {
            session.predict(&expectation, at(100)).expect("记录预测");
        }
        if injection.stop_after >= 3 {
            session
                .admit(&write, &permit, at(100))
                .expect("受理应当有确定结果");
        }
        if injection.stop_after >= 4 {
            dispatched = true;
            if let Ok(DispatchOutcome::Receipted(receipt)) = session.dispatch(&write, &permit, at(100))
            {
                receipt_at_crash = Some(format!("{:?}", receipt.status));
                if injection.stop_after >= 5 {
                    session.settle(&receipt, at(100)).expect("记录回执");
                }
            }
        }
        // 进程在这里崩溃。会话与存储句柄丢掉，模拟 OS 留着。
        let applications = broker.os().applications();
        (applications, receipt_at_crash, dispatched)
    };

    let mut store = Store::open(path, at(3_600)).expect("重新打开");
    let report = store.recover(at(3_600)).expect("恢复");
    let outcome = Outcome {
        applications: applications_at_crash,
        receipt: receipt_at_crash,
        dispatched,
        unknown_commits: report.unknown_commits.len(),
        resendable: report.resendable.len(),
        pending: store.pending_outbox(16).expect("待投清单").len(),
    };

    // 第二遍：**不该再发现同一批**。
    //
    // `recover` 的返回是"这次找到了什么"（一份转换日志），不是"现在挂着什么"。两者混成
    // 一件事的话，每次调用都会把同一批报一遍，而审计账会被同一条记录刷满——审计按
    // §12.3 只放最小元数据，正是因为它假设每条记录都对应一次真实发生的事。
    let again = store.recover(at(7_200)).expect("再恢复一次");
    assert!(
        again.unknown_commits.is_empty(),
        "{}：第二遍不该再发现同一批",
        injection.label()
    );

    // 而它确实**留在那里等着被结清**。"不重复报"不等于"报了就算完"——少了这一条，
    // 一个"报完就丢掉"的实现也会通过上一条。
    for commit in &report.unknown_commits {
        let state = store
            .resolve_unknown_commit(&commit.action_id, Resolution::ConfirmedCompleted, at(7_200))
            .expect("显式恢复任务核对目标状态之后结清它");
        assert_eq!(
            state,
            soca_storage::ActionState::Completed,
            "{}：结清之后应当到 Completed",
            injection.label()
        );
    }
    if !dispatched {
        // 反面：没投递过的动作**不是**未知提交，不该能被结清。这条挡住的是
        // "恢复把一切都标成未知提交"——那种实现会让上面那些断言全部通过。
        assert!(
            store
                .resolve_unknown_commit("action:1", Resolution::ConfirmedAbsent, at(7_200))
                .is_err(),
            "{}：没投递过的动作不该能结清",
            injection.label()
        );
    }

    // 而重放是**只读**的：它看账，不碰外部世界（§7.3）。
    let before_replay = broker.os().applications();
    let _ = store.pending_outbox(16).expect("待投清单");
    assert_eq!(
        broker.os().applications(),
        before_replay,
        "{}：读账不该动外部世界",
        injection.label()
    );

    outcome
}

fn assert_invariants(injection: Injection, outcome: &Outcome) {
    let label = injection.label();

    // 一、**副作用最多应用一次**。这是 §17 那半句的直接形式：已提交的副作用不因重放重复执行。
    assert!(
        outcome.applications <= 1,
        "{label}：副作用被应用了 {} 次",
        outcome.applications
    );

    // 二、"没重复"不能靠"什么都没做"换来。回执说**成功**就说明它确实应用过——
    //    少了这一条，一个永远不执行的实现会完美通过上面那条。
    //
    //    注意这里比的是**状态**而不是"有没有回执"：回执说"失败"是完全正当的一种结果
    //    （应用之前就失败了），它与"回执说成功"不是一回事，与"没拿到回执"更不是。
    if outcome.receipt.as_deref() == Some("Completed") {
        assert_eq!(
            outcome.applications, 1,
            "{label}：回执说成功了，就一定要真的做过"
        );
    }

    // 三、压根没投递过的时候，外部世界必须一点没动。
    if !outcome.dispatched {
        assert_eq!(outcome.applications, 0, "{label}：没投递过不该有副作用");
    }

    // 四、中断那一档是**最危险**的一档：副作用发生了，而调用方什么都没拿到。
    //    它必须真的被扫到，而且必须真的应用过——只断言"没重复"的话，"中断其实没应用"
    //    也会通过，而那恰恰让这一档失去意义。
    if injection.interrupt_after == Some(1)
        && outcome.dispatched
        && outcome.receipt.is_none()
        && outcome.applications == 0
    {
        panic!("{label}：应用之后中断，就必须真的应用过");
    }

    if outcome.dispatched {
        // 三、"已投递的动作绝不能出现在可重投清单里"（§7.3 的恢复分支）。
        assert_eq!(
            outcome.resendable, 0,
            "{label}：已投递的动作不得重投"
        );
        assert_eq!(
            outcome.pending, 0,
            "{label}：投递者不该看到未知提交"
        );
    } else {
        // 四、没投递过的应当**可以**安全重投，而不是被当成未知提交。
        //    两个方向都要有断言：只断言一边，"恢复把所有东西都标成未知提交"也能通过。
        assert_eq!(
            outcome.unknown_commits, 0,
            "{label}：没投递过就不是未知提交"
        );
    }
}

#[test]
fn a_hundred_fault_injection_points_never_double_apply_a_side_effect() {
    let dir = TempDir::new().expect("临时目录");
    let content_store = ContentStore::open(dir.path().join("content")).expect("内容仓");
    let content = "新的摘要内容";
    let mut points = 0usize;

    for stop_after in 0..=5 {
        for interrupt_after in [None, Some(1), Some(2)] {
            for fail_before in [None, Some(1), Some(2), Some(3)] {
                for prior_observations in 0..=2 {
                    let injection = Injection {
                        stop_after,
                        interrupt_after,
                        fail_before,
                        prior_observations,
                    };
                    let path = dir.path().join(format!(
                        "soca-{stop_after}-{}-{}-{prior_observations}.db",
                        interrupt_after.map_or(0, |n| n),
                        fail_before.map_or(0, |n| n),
                    ));
                    let outcome = run(injection, &path, &content_store, content);
                    assert_invariants(injection, &outcome);
                    points += 1;
                }
            }
        }
    }

    assert!(
        points >= 100,
        "§17 要求至少 100 个故障注入点，实际 {points}"
    );
}

#[test]
fn every_dispatch_outcome_is_one_of_the_four_that_were_asserted() {
    // 上一条扫描了 216 个点，但**如果没有覆盖到四种走向，它就只是在重复同一件事**。
    // 这条测试检查扫描确实走到了那些分支——覆盖率是断言出来的，不是假设出来的。
    let dir = TempDir::new().expect("临时目录");
    let content_store = ContentStore::open(dir.path().join("content")).expect("内容仓");
    let content = "新的摘要内容";

    let cases = [
        ("没受理", Injection { stop_after: 2, interrupt_after: None, fail_before: None, prior_observations: 0 }),
        ("受理未投递", Injection { stop_after: 3, interrupt_after: None, fail_before: None, prior_observations: 0 }),
        ("投递完成", Injection { stop_after: 5, interrupt_after: None, fail_before: None, prior_observations: 0 }),
        ("应用后中断", Injection { stop_after: 4, interrupt_after: Some(1), fail_before: None, prior_observations: 0 }),
        ("应用前失败", Injection { stop_after: 4, interrupt_after: None, fail_before: Some(1), prior_observations: 0 }),
    ];

    let mut saw_no_side_effect = false;
    let mut saw_one_side_effect = false;
    let mut saw_unknown_commit = false;
    let mut saw_resendable = false;

    for (index, (name, injection)) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("soca-case-{index}.db"));
        let outcome = run(injection, &path, &content_store, content);
        assert_invariants(injection, &outcome);

        saw_no_side_effect |= outcome.applications == 0;
        saw_one_side_effect |= outcome.applications == 1;
        saw_unknown_commit |= outcome.unknown_commits == 1;
        saw_resendable |= outcome.resendable == 1;

        assert!(
            outcome.applications <= 1,
            "{name}：{}",
            injection.label()
        );
    }

    assert!(saw_no_side_effect, "要扫到「一次都没做」的那一档");
    assert!(saw_one_side_effect, "也要扫到「做过一次」的那一档");
    assert!(saw_unknown_commit, "要扫到未知提交");
    assert!(saw_resendable, "也要扫到可安全重投");
}
