//! §4.1 L4 的调度器与 §12.1 的全局暂停。
//!
//! 两件事放在一个文件里，是因为它们是同一件事的两半：调度器决定"该不该跑"，而暂停是
//! 它必须最先看的那个输入。§12.1 的原话是：
//!
//! > 全局暂停应**先**撤销尚未消费的执行授权、**停止采集和外发**，**再**取消模型任务。
//!
//! 这一项之前，暂停**只**挡住了新许可——系统照样出去读文件、照样调模型。
//! 而用户按下暂停，期望的是"现在停"。

use soca_contracts::{
    ActionLevel, Approval, ApprovalId, CapabilityPolicyRef, DataClass, ExplorationQuota,
    GoalBudget, GrantScope, ModelBackend, ModelBudget, ModelVersion, OutputSchema,
    PermissionScope, RetryWhen, SelectionPolicy, SubjectId, UserChannel, WallClock,
};
use soca_core::{
    ActionBroker, AdvanceStep, CoreError, Resources, RoundOutcome, ScheduleOutcome, Scheduler,
    SimulatedOs, Subject,
};
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

fn cap(name: &str) -> CapabilityPolicyRef {
    CapabilityPolicyRef::new(name).expect("固定能力策略")
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
        // 返回空提案的应答源。有一条测试要证明"暂停之后连模型都调不动"，而那需要
        // 一次**本来会成功**的咨询——空脚本会让它在暂停之前就先失败，那样就什么也没证明。
        Box::new(DeterministicTransport::from_fn(|_| {
            Ok(soca_contracts::ModelOutput {
                schema_version: soca_contracts::MODEL_OUTPUT_SCHEMA_VERSION,
                model_version: ModelVersion::new("sha256:test-model").expect("固定模型版本"),
                usage: soca_contracts::TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                proposals: Vec::new(),
                claims_finished: false,
            })
        })),
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

fn delegate(subject: &mut Subject, level: ActionLevel) -> soca_contracts::GoalId {
    let goal_id = subject
        .delegate(
            "把摘要写进已授权目录",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: cap(CAP),
                max_action_level: level,
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

// ---------------------------------------------------------------------------
// 调度
// ---------------------------------------------------------------------------

#[test]
fn the_scheduler_stops_when_there_is_nothing_left_to_do() {
    // 没有目标时一轮就报结束，而不是空转满请求的轮数——空转会把审计账塞满一样的记录。
    let mut subject = subject();
    let report = Scheduler::default()
        .run(
            &mut subject,
            &Resources::Unknown,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(0),
        )
        .expect("跑一段");

    assert!(
        matches!(report.outcome, ScheduleOutcome::Finished { rounds: 1 }),
        "实际：{:?}",
        report.outcome
    );
    assert_eq!(report.rounds.len(), 1);
}

#[test]
fn the_scheduler_stops_at_its_round_budget() {
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");

    let report = Scheduler {
        max_rounds: 2,
        idle_limit: 8,
    }
    .run(
            &mut subject,
            &Resources::Unknown,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(2),
        )
    .expect("跑一段");

    assert!(
        matches!(report.outcome, ScheduleOutcome::RoundsSpent { rounds: 2 }),
        "实际：{:?}",
        report.outcome
    );
    assert_eq!(report.rounds.len(), 2, "逐轮记录要与结论对得上");
}

#[test]
fn the_scheduler_backs_off_when_rounds_make_no_progress() {
    // 一个够不到证据门槛的结论：每一轮都重新核一遍，每一轮都不达标。
    //
    // 这正是退避要挡的事——继续跑只会把审计账塞满一模一样的记录，而 §12.3 说审计只放
    // 最小元数据，正是因为它假设每条记录都对应一次真实发生的事。
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");

    let report = Scheduler {
        max_rounds: 16,
        idle_limit: 3,
    }
    .run(
            &mut subject,
            &Resources::Unknown,
            &SelectionPolicy::default(),
            ActionLevel::A2,
            at(2),
        )
    .expect("跑一段");

    assert!(
        matches!(
            report.outcome,
            ScheduleOutcome::BackedOff {
                rounds: 3,
                idle_rounds: 3
            }
        ),
        "实际：{:?}",
        report.outcome
    );
    // 而它是"没达标"，不是"没有候选"——两者都停在退避上，但含义不同：
    // 前者说明该去补证据，后者说明该去换个方向。
    assert_eq!(report.rounds.len(), 3);
    assert!(
        report.rounds.iter().all(|round| matches!(
            &round.outcome,
            RoundOutcome::Idle
        )),
        "每一轮都该是空转：{:?}",
        report.rounds
    );
}

#[test]
fn a_paused_subject_runs_no_rounds_at_all() {
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject.policy_mut().pause("用户按下暂停");

    let report = Scheduler::default()
        .run(
            &mut subject,
            &Resources::Unknown,
            &SelectionPolicy::default(),
            ActionLevel::A1,
            at(1),
        )
        .expect("跑一段");

    match &report.outcome {
        ScheduleOutcome::Paused { reason, rounds } => {
            assert_eq!(*rounds, 0, "一轮也不该跑");
            assert!(reason.contains("暂停"), "理由要说得出是谁停的：{reason}");
        }
        other => panic!("实际：{other:?}"),
    }
    assert!(report.rounds.is_empty());
}

#[test]
fn a_refusal_caused_by_pausing_does_not_throw_the_work_away() {
    // 暂停是**可逆**的（§12.1）。而"可逆"不只是说锁会打开——恢复之后那份还没做的工作
    // 得**还在**。
    //
    // 这与"范围越界"造成的拒绝不同：那一种是**此路不通**，要放宽范围必须重新委托，
    // 所以把动作取走是对的；而暂停只是**此刻不通**。两者混成一档的话，暂停会顺手把待办
    // 丢掉，用户恢复之后发现该做的事没了，而账上只有一条"被拒绝"——一次静默的放弃。
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");
    subject.request_write(WATCHED, "内容", at(2)).expect("投递");
    assert_eq!(subject.pending_actions(), 1);

    subject.policy_mut().pause("用户按下暂停");
    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(3))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step: AdvanceStep::Refused { reason, retry_when },
        } => {
            assert!(reason.contains("暂停"), "实际：{reason}");
            // §13.1 的"可重试条件"：暂停是**此刻不通**，而"此刻"会过去。
            // 报成 `Never` 的话，调度器会把这个目标收工掉——而用户只是按了一下暂停。
            assert_eq!(retry_when, RetryWhen::WhenUnpaused, "暂停要给的是'等一等'");
        }
        other => panic!("实际：{other:?}"),
    }

    assert_eq!(
        subject.pending_actions(),
        1,
        "暂停挡住了它，但不该把它丢掉——恢复之后还要接着做"
    );
    assert_eq!(
        subject.store().action_count().expect("动作账"),
        0,
        "而它确实没有执行"
    );

    // 恢复之后它真的能接着走：这一次不再是被拒绝。
    subject.policy_mut().resume();
    assert!(
        !matches!(
            subject
                .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(4))
                .expect("跑一轮")
                .outcome,
            RoundOutcome::Advanced {
                step: AdvanceStep::Refused { .. }
            }
        ),
        "恢复之后不该还是拒绝"
    );
}

#[test]
fn a_refusal_caused_by_scope_does_take_the_work_out() {
    // 上一条的对照。范围越界是**此路不通**：放宽范围要重新委托，所以取走是对的——
    // 留在队列里的话，L3 每一轮都会重新提议同一个必然被拒的动作，环路空转到额度耗尽。
    //
    // 两条一起看，才说明"该不该取走"是**按原因分的**，不是一刀切。
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A1);
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");
    subject.request_write(WATCHED, "内容", at(2)).expect("投递");

    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A2, at(3))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step: AdvanceStep::Refused { reason, retry_when },
        } => {
            assert!(reason.contains("上限"), "实际：{reason}");
            // 与暂停相反：等级超范围是**此路不通**。要提升得重新委托。
            assert_eq!(retry_when, RetryWhen::Never, "超范围等多久都不会好");
        }
        other => panic!("实际：{other:?}"),
    }
    assert_eq!(
        subject.pending_actions(),
        0,
        "此路不通的那一种要取走，否则每一轮都重新提议它"
    );
}

// ---------------------------------------------------------------------------
// §12.1 的全局暂停
// ---------------------------------------------------------------------------

#[test]
fn pausing_stops_collection_and_egress_not_just_permits() {
    // §12.1："全局暂停应先撤销尚未消费的执行授权、**停止采集和外发**，再取消模型任务。"
    //
    // 这一项之前，暂停**只**挡住了新许可：系统照样出去读文件、照样调模型。
    let mut subject = subject();
    let goal_id = delegate(&mut subject, ActionLevel::A2);

    // 暂停之前三件事都能做——不然这条测试分不出"暂停挡住了"与"本来就做不了"。
    subject
        .observe(WATCHED, DataClass::Personal, at(1))
        .expect("观测");
    subject
        .consult_model(&goal_id, OutputSchema::read_only(), at(2))
        .expect("咨询");

    subject.policy_mut().pause("用户按下暂停");

    // 采集停了。
    assert!(
        matches!(
            subject.observe(WATCHED, DataClass::Personal, at(3)),
            Err(CoreError::Paused { .. })
        ),
        "暂停要停止采集"
    );
    // 外发与模型任务停了。
    assert!(
        matches!(
            subject.consult_model(&goal_id, OutputSchema::read_only(), at(4)),
            Err(CoreError::Paused { .. })
        ),
        "暂停要停止外发、取消模型任务"
    );
    // 新许可也停了——这一条本来就有，写在这里是为了说明它只是三件里的第一件。
    assert_eq!(
        subject.store().action_count().expect("动作账"),
        0,
        "拒绝不产生副作用"
    );
}

#[test]
fn a_paused_subject_refuses_new_observations_as_a_refusal_not_an_error() {
    // 暂停期间那条观测候选走到推进那一步时，应当记成**拒绝**而不是抛错。
    // 抛错会让环路中断在一句"内部错误"上，而真正的原因（有人按了暂停）看不出来。
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject.policy_mut().pause("用户按下暂停");

    // 暂停之前没有观测，所以第一轮会提出观测请求。
    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(1))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced {
            step: AdvanceStep::Refused { reason, retry_when },
        } => {
            assert!(reason.contains("暂停"), "实际：{reason}");
            assert_eq!(retry_when, RetryWhen::WhenUnpaused);
        }
        other => panic!("实际：{other:?}"),
    }
}

#[test]
fn a_paused_subject_still_accepts_the_users_own_words() {
    // 暂停停的是**系统出去做事**，不是用户说话。
    //
    // §17 的"弹性"那一行要求"保持**取消/审批**可响应"——把用户输入与审批也一并停掉，
    // 会让暂停变成一个关不掉的东西：用户既不能取消，也不能批准，只能重启进程。
    let mut subject = subject();
    subject.policy_mut().pause("用户按下暂停");

    subject
        .delegate(
            "暂停期间用户仍然可以说话",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: cap(CAP),
                max_action_level: ActionLevel::A2,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(1),
            None,
        )
        .expect("委托仍然受理");

    let approval = Approval::new(
        ApprovalId::new("approval:paused").expect("固定审批"),
        owner(),
        ActionLevel::A2,
        UserChannel::ApprovalUi,
        at(2),
        None,
        1,
    )
    .expect("合法批准");
    subject.grant_approval(&approval, at(2)).expect("记下批准");
    assert_eq!(
        subject.usable_approvals(at(2)).expect("查").len(),
        1,
        "批准也要记得下——否则暂停之后连'批了但还没执行'都做不到"
    );
}

#[test]
fn resuming_lets_the_subject_work_again() {
    // 暂停是**可逆**的，否则用户不敢用它。而"恢复"只作用于之后：暂停期间该发生而没发生的
    // 事不会自动补做（§12.1 末段："已发生副作用只能核对、补偿或由用户处理，不能承诺倒转现实"）。
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject.policy_mut().pause("用户按下暂停");
    assert!(subject.observe(WATCHED, DataClass::Personal, at(1)).is_err());

    subject.policy_mut().resume();
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("恢复之后又能观测");
}

#[test]
fn the_grant_scope_survives_a_pause() {
    // 暂停不是撤回。撤回之后再恢复需要**重新授予**（§12.1），而暂停期间授权一直在，
    // 恢复之后立刻可用。把两者混起来，会让"暂停一下"变成"要重新授权一遍"。
    let mut subject = subject();
    delegate(&mut subject, ActionLevel::A2);
    subject.policy_mut().pause("用户按下暂停");
    subject.policy_mut().resume();

    let granted = subject.granted_capabilities();
    assert_eq!(granted.len(), 1);
    assert!(
        subject.policy().grant_scope(granted[0]).is_some(),
        "范围也还在"
    );
    // 而它确实还是那个范围——不是被暂停顺手抹成了"不限定"。
    assert!(!subject
        .policy()
        .grant_scope(granted[0])
        .expect("有范围")
        .is_unbounded());

    // 顺带钉一下这个类型还在导出的位置上：测试用的是它，不是某个等价的字符串。
    let _ = GrantScope::anywhere();
}
