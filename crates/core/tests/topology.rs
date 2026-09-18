//! §17 的"拓扑数量"那一行。
//!
//! 那一行是：
//!
//! > 同需求异资源输出不同叶/簇/协调数；**迁移有 epoch、fencing、状态恢复和回滚**，
//! > **主体身份不因压力被合并**。
//!
//! 前半句由 `core-topology` 负责（它全套都在、也有自己的测试）。后半句的三样在此之前
//! **一样都没跑过**：契约层有十档状态机、存储层有 `open_scale_transaction` 与 `commit_route`
//! 的 CAS，而没有任何东西把它们按顺序走一遍。
//!
//! 【还没做的一处】fencing token 只在 §6.2 的注释里出现过，`ExecutionPermit` 上没有这个字段。
//! 也就是说：路由切换之后，一个**在切换之前签发**的许可仍然能被执行代理接受。这一格还没补。

use soca_contracts::{
    ActionLevel, CapabilityPolicyRef, DataClass, ExplorationQuota, GoalBudget, ModelBackend,
    ModelBudget, ModelReservation, ModelVersion, PermissionScope, PlannerPolicy, PlanState,
    ResourceEnvelope, ScaleTransactionState, SelectionPolicy, SubjectId, UserChannel, WallClock,
    LEAF_PROFILES,
};
use soca_core::{ActionBroker, ScaleRun, SimulatedOs, Subject};
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
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000e5").expect("固定 boot"),
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

/// 一份可行的拓扑计划。
///
/// **需求固定、资源变化**——这正是 §17 那一行前半句的构造方式："同需求异资源输出不同
/// 叶/簇/协调数"。需求写死成"要很多叶"，于是实际拿到多少完全由资源决定；
/// 把需求也给成最小档的话，两份计划会一模一样，而那条测试就什么都没测。
///
/// 顺带一提：需求是**上限**而不是目标——§2 说"不因空闲 RAM 多就生成无任务角色"，
/// 所以资源再多也不会超过需求。
fn plan(ram_mib: u64) -> soca_contracts::TopologyPlan {
    let envelope = ResourceEnvelope {
        ram_limit_mib: ram_mib,
        cpu_slots: 4,
        gpu_allocatable_mib: 0,
        telemetry_age_seconds: 0.0,
    };
    soca_core_topology::plan_topology(
        &envelope,
        &ModelReservation::default(),
        u64::from(*LEAF_PROFILES.last().expect("至少有一档")),
        &PlannerPolicy::default(),
    )
    .expect("规划")
}

/// 一个醒着、干过活的单元。
fn running_subject() -> Subject {
    let mut subject = subject();
    subject.register_unit(at(1)).expect("登记");
    subject.wake(at(1)).expect("唤醒");
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
    subject
        .observe(WATCHED, DataClass::Personal, at(2))
        .expect("观测");
    subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(3))
        .expect("跑一轮");
    subject
}

// ---------------------------------------------------------------------------
// 前半句：同需求异资源输出不同拓扑
// ---------------------------------------------------------------------------

#[test]
fn different_resources_produce_different_topologies_and_the_subject_count_never_moves() {
    // "同需求异资源输出不同叶/簇/协调数"——而**主体数不随之变化**。
    //
    // 最后半句是这一行里最要紧的一条：压力下自动合并两个主体会把两个记忆域并成一个，
    // 而那是**不可逆**的信息损失。
    let small = plan(2_048);
    let large = plan(65_536);

    assert_eq!(small.state, PlanState::Running);
    assert_eq!(large.state, PlanState::Running);
    assert!(
        large.leaves > small.leaves,
        "资源多的那一份该给出更多叶：{} vs {}",
        small.leaves,
        large.leaves
    );
    assert_eq!(small.subjects, 1);
    assert_eq!(large.subjects, 1, "资源变多不该多出主体");
}

// ---------------------------------------------------------------------------
// 后半句：迁移
// ---------------------------------------------------------------------------

#[test]
fn a_migration_bumps_the_epoch_and_moves_the_state_across() {
    // 主路径：PLANNED → … → DONE，而路由的世代真的换了。
    let mut subject = running_subject();
    let before = subject.route_epoch().expect("读路由");
    assert!(
        before.is_none(),
        "还没迁移过的主体没有路由记录：{before:?}"
    );

    let run = subject.scale(&plan(65_536), at(10)).expect("迁移");
    match run {
        ScaleRun::Committed {
            old_epoch,
            new_epoch,
            ..
        } => {
            assert_eq!(old_epoch, 1, "第一次迁移从初始世代出发");
            assert_eq!(new_epoch, 2);
        }
        other => panic!("该提交：{other:?}"),
    }

    let route = subject.route_epoch().expect("读路由").expect("有路由");
    assert_eq!(route.get(), 2, "世代要真的切换了");
    assert!(subject.is_awake(), "搬过去之后单元要醒着");

    // 再迁一次：世代**只能往上走**（§6.3："需要回退时创建更大的世代"）。
    let again = subject.scale(&plan(8_192), at(11)).expect("再迁一次");
    assert!(matches!(again, ScaleRun::Committed { new_epoch: 3, .. }), "{again:?}");
}

#[test]
fn a_cold_unit_cannot_be_migrated_and_the_attempt_rolls_back() {
    // 一个没在跑的单元**没有状态可搬**，所以那次迁移什么也没换。
    //
    // 让它在没有快照的情况下照样提交，等于把"移位"变成"丢东西"：新世代在跑，
    // 而它手上的状态是空的。
    let mut subject = subject();
    subject.register_unit(at(1)).expect("登记");
    // 注意：**没有唤醒**。
    assert!(!subject.is_awake());

    let run = subject.scale(&plan(65_536), at(10)).expect("迁移");
    match run {
        ScaleRun::RolledBack { reached, .. } => {
            assert_eq!(reached, "DRAINING", "它停在停止新租约那一档");
        }
        other => panic!("该回滚：{other:?}"),
    }
    // 而路由**没有被推动**：它还停在初始世代上。回滚的意思是"什么也没换"，
    // 不是"把路由删掉"——删掉之后这个主体看起来像从来没登记过。
    assert_eq!(
        subject.route_epoch().expect("读路由").map(|epoch| epoch.get()),
        Some(1)
    );
}

#[test]
fn a_paused_plan_is_refused_before_a_transaction_is_even_opened() {
    // 照一份暂停的计划迁移，等于把"还没准备好"变成"已经在跑"。
    //
    // 而它**在开事务之前**就被挡住了：挡在事务之后的话，账上会多出一条
    // 从没打算跑完的事务，而清理它又是一件事。
    let mut subject = running_subject();
    let starved = plan(64);
    assert_eq!(starved.state, PlanState::Paused);

    let refused = subject.scale(&starved, at(10));
    assert!(refused.is_err(), "暂停的计划不该被迁移：{refused:?}");
    assert!(
        subject
            .route_epoch()
            .expect("读路由")
            .is_none(),
        "拒绝时不该已经开了事务"
    );
}

#[test]
fn a_plan_that_would_merge_subjects_is_refused() {
    // §17："主体身份**不因压力被合并**。"
    //
    // 规划器自己永远只会给出 1 个主体，所以这一条测的是**守卫本身**：有人递进来一份
    // 主体数不是 1 的计划时，能不能挡住。少了这条守卫，"不因压力被合并"就只是规划器的
    // 一个巧合，而不是一条规则。
    let mut subject = running_subject();
    let mut merged = plan(65_536);
    merged.subjects = 2;

    let refused = subject.scale(&merged, at(10));
    let message = match refused {
        Err(error) => error.to_string(),
        Ok(other) => panic!("主体数被改的计划不该被迁移：{other:?}"),
    };
    assert!(message.contains("主体"), "{message}");
}

#[test]
fn two_migrations_cannot_both_hold_the_same_epoch() {
    // §6.2 的 CAS："**不做'以新压旧'的宽容处理**：两个迁移同时提交会让旧 actor 继续持有
    // 可执行所有权。"
    //
    // 这道闸有两层，而**先响的是数据库那一层**：`scale_transactions` 上的
    // `UNIQUE(subject_id, new_epoch)` 让"同一主体同一世代"只能有一个事务在飞
    // （存储层的注释写得很明白：这是让数据库保证，而不是靠调用方的纪律）。
    //
    // 本测试走的是第一层。第二层（`commit_route` 的 CAS）挡的是"两笔都开出来之后
    // 才有一笔提交"那种更晚才暴露的情形，它由 `storage/tests/topology_state.rs` 钉着。
    let mut subject = running_subject();
    let first = subject
        .begin_migration(&plan(65_536), at(10))
        .expect("第一笔开得起来");

    let second = subject.begin_migration(&plan(8_192), at(11));
    assert!(second.is_err(), "同一世代上不该开出第二笔：{second:?}");

    // 第一笔照常收尾。
    let run = subject
        .finish_migration(first, at(12))
        .expect("收尾");
    assert!(run.committed(), "{run:?}");
    assert_eq!(
        subject.route_epoch().expect("读路由").expect("有路由").get(),
        2,
        "世代只该前进一次"
    );

    // 而现在它可以再开一笔了——世代已经往前走，唯一键不再冲突。
    let third = subject.begin_migration(&plan(8_192), at(13));
    assert!(third.is_ok(), "世代往前之后就该能再开：{third:?}");
}

#[test]
fn a_rolled_back_transaction_says_which_stage_it_stopped_at() {
    // 回滚报告要说清"走到哪一档才发现不行"——它决定了要清理到什么地方。
    // 只说"失败了"的话，操作员得自己去看事务表。
    let mut subject = subject();
    subject.register_unit(at(1)).expect("登记");

    let run = subject.scale(&plan(65_536), at(10)).expect("迁移");
    let (transaction_id, reached, reason) = match run {
        ScaleRun::RolledBack {
            transaction_id,
            reached,
            reason,
        } => (transaction_id, reached, reason),
        other => panic!("该回滚：{other:?}"),
    };
    assert_eq!(reached, ScaleTransactionState::Draining.as_str());
    assert!(!reason.is_empty(), "理由不给空");

    // 而事务表上它确实以 `ROLLED_BACK` 收尾，不是停在半路。
    //
    // 报告里带着事务标识，所以这一步照着它去查就行——**不需要猜**，
    // 也不需要"把最近一条读出来"。
    let transaction = subject
        .store()
        .scale_transaction(
            &soca_contracts::TransactionId::new(transaction_id).expect("标识合法"),
        )
        .expect("读事务")
        .expect("存在");
    assert_eq!(transaction.state, ScaleTransactionState::RolledBack);
}
