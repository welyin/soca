//! 拓扑世代、迁移事务与消息去重的回归测试（实施规格 §6、§7；工单 ENG-05 的 CAS 与幂等部分）。
//!
//! 三件最容易做错、也最难在事后发现的事：
//!
//! 1. 世代被"以新压旧"地覆盖，于是两个 actor 同时持有可执行所有权；
//! 2. 迁移在提交点之后被回滚，于是旧 actor 复活并继续写入；
//! 3. 消息去重键与动作幂等键混用，于是"重复投递"和"重复副作用"互相掩护。

use soca_contracts::*;
use soca_storage::{Store, StorageError};
use tempfile::TempDir;

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("固定基准时间")
}

fn at(seconds: i64) -> WallClock {
    base_time().plus_seconds(seconds)
}

fn subject() -> SubjectId {
    SubjectId::new("subject:main").expect("固定主体")
}

fn graph(version: &str) -> BlobRef {
    BlobRef::new(format!("blob:graph-{version}")).expect("固定对象")
}

fn plan() -> TopologyPlan {
    TopologyPlan {
        state: PlanState::Running,
        leaves: 64,
        clusters: 8,
        coordinators: 0,
        subjects: 1,
        hot_leaves: 16,
        workers: 4,
        llm_parallel_calls: 1,
        ram_required_mib: 2838,
        ram_limit_mib: 8192,
        reason: PlanReason::Admitted,
    }
}

fn transaction(id: &str, old: u64, new: u64) -> ScaleTransaction {
    ScaleTransaction {
        transaction_id: TransactionId::new(id).expect("固定事务"),
        subject_id: subject(),
        old_epoch: TopologyEpoch::try_from(old).expect("合法世代"),
        new_epoch: TopologyEpoch::try_from(new).expect("合法世代"),
        state: ScaleTransactionState::Planned,
        plan: plan(),
        resource_reservation_id: ReservationId::new("reservation:1").expect("固定预约"),
        deadline: at(300),
    }
}

fn temp_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("临时目录");
    let store = Store::open(dir.path().join("soca.db"), at(0)).expect("打开存储");
    (dir, store)
}

// ---------------------------------------------------------------------------
// 世代 CAS（§6.2、§6.3）
// ---------------------------------------------------------------------------

#[test]
fn the_route_epoch_advances_only_through_a_matching_cas() {
    let (_dir, mut store) = temp_store();
    let route = store
        .ensure_route(&subject(), &graph("v1"), at(0))
        .expect("建立路由");
    assert_eq!(route.current_epoch, TopologyEpoch::INITIAL);

    let committed = store
        .commit_route(
            &subject(),
            TopologyEpoch::INITIAL,
            TopologyEpoch::try_from(2).expect("合法世代"),
            &graph("v2"),
            at(1),
        )
        .expect("世代匹配，应当提交");
    assert_eq!(committed.current_epoch.get(), 2);
    assert_eq!(committed.graph_ref, graph("v2"));

    // 旧世代再提交一次：说明另一个迁移已经提交过，必须失败而不是覆盖。
    let stale = store.commit_route(
        &subject(),
        TopologyEpoch::INITIAL,
        TopologyEpoch::try_from(3).expect("合法世代"),
        &graph("v3"),
        at(2),
    );
    assert!(matches!(
        stale,
        Err(StorageError::EpochMismatch {
            expected: 1,
            actual: 2,
            ..
        })
    ));

    // 失败不得留下任何痕迹。
    assert_eq!(
        store.route(&subject()).unwrap().unwrap().current_epoch.get(),
        2
    );
    assert_eq!(
        store.route(&subject()).unwrap().unwrap().graph_ref,
        graph("v2")
    );
}

#[test]
fn committing_a_route_without_one_is_rejected() {
    let (_dir, mut store) = temp_store();
    let result = store.commit_route(
        &subject(),
        TopologyEpoch::INITIAL,
        TopologyEpoch::try_from(2).expect("合法世代"),
        &graph("v2"),
        at(1),
    );
    assert!(matches!(result, Err(StorageError::RouteNotFound { .. })));
}

#[test]
fn an_epoch_cannot_be_zero_and_cannot_go_backwards_in_a_transaction() {
    // 0 无法与"未迁移"区分，因此类型层就拒绝。
    assert!(TopologyEpoch::try_from(0).is_err());
    assert_eq!(TopologyEpoch::INITIAL.get(), 1);

    // 迁移事务的新世代必须严格晚于旧世代。回退要创建更大的世代，不能重用旧的 fencing token。
    let backwards = transaction("transaction:1", 5, 5);
    assert!(matches!(
        backwards.validate(),
        Err(ContractError::InvalidEpoch(_))
    ));
    let backwards = transaction("transaction:2", 5, 4);
    assert!(matches!(
        backwards.validate(),
        Err(ContractError::InvalidEpoch(_))
    ));
    assert!(transaction("transaction:3", 5, 6).validate().is_ok());
}

// ---------------------------------------------------------------------------
// 迁移事务状态机（§6.2）
// ---------------------------------------------------------------------------

#[test]
fn a_migration_transaction_walks_the_documented_path() {
    let (_dir, mut store) = temp_store();
    let opened = transaction("transaction:1", 1, 2);
    store.open_scale_transaction(&opened, at(0)).expect("打开事务");

    for next in [
        ScaleTransactionState::Reserved,
        ScaleTransactionState::Draining,
        ScaleTransactionState::Snapshotted,
        ScaleTransactionState::ShadowReady,
        ScaleTransactionState::RouteCommitted,
        ScaleTransactionState::Retiring,
        ScaleTransactionState::Done,
    ] {
        store
            .advance_scale_transaction(&opened.transaction_id, next, at(1))
            .unwrap_or_else(|error| panic!("推进到 {next:?} 应当成功：{error}"));
    }

    let restored = store
        .scale_transaction(&opened.transaction_id)
        .expect("读取事务")
        .expect("事务必须存在");
    assert_eq!(restored.state, ScaleTransactionState::Done);
    assert_eq!(restored.old_epoch.get(), 1);
    assert_eq!(restored.new_epoch.get(), 2);
    assert_eq!(restored.plan, plan(), "计划原文必须可读回");
    assert!(!restored.is_before_commit());
}

#[test]
fn a_transaction_cannot_skip_stages_or_roll_back_after_commit() {
    let (_dir, mut store) = temp_store();
    let opened = transaction("transaction:1", 1, 2);
    store.open_scale_transaction(&opened, at(0)).expect("打开事务");

    // 跳步：PLANNED 不能直接进入 DRAINING。
    let skipped = store.advance_scale_transaction(
        &opened.transaction_id,
        ScaleTransactionState::Draining,
        at(1),
    );
    assert!(matches!(
        skipped,
        Err(StorageError::IllegalTransactionTransition {
            from: "PLANNED",
            to: "DRAINING",
            ..
        })
    ));

    // 提交前可以回滚。
    store
        .advance_scale_transaction(
            &opened.transaction_id,
            ScaleTransactionState::RolledBack,
            at(2),
        )
        .expect("提交前回滚");

    // 已终结的事务不能再推进。
    let after_terminal = store.advance_scale_transaction(
        &opened.transaction_id,
        ScaleTransactionState::Reserved,
        at(3),
    );
    assert!(matches!(
        after_terminal,
        Err(StorageError::IllegalTransactionTransition {
            from: "ROLLED_BACK",
            ..
        })
    ));
}

#[test]
fn a_committed_transaction_can_only_move_forward_or_compensate() {
    let (_dir, mut store) = temp_store();
    let opened = transaction("transaction:1", 1, 2);
    store.open_scale_transaction(&opened, at(0)).expect("打开事务");
    for next in [
        ScaleTransactionState::Reserved,
        ScaleTransactionState::Draining,
        ScaleTransactionState::Snapshotted,
        ScaleTransactionState::ShadowReady,
        ScaleTransactionState::RouteCommitted,
    ] {
        store
            .advance_scale_transaction(&opened.transaction_id, next, at(1))
            .expect("推进");
    }

    // 提交点之后回滚会让旧 actor 复活并继续写入，这是 §6.3 明令禁止的。
    let rollback = store.advance_scale_transaction(
        &opened.transaction_id,
        ScaleTransactionState::RolledBack,
        at(2),
    );
    assert!(matches!(
        rollback,
        Err(StorageError::IllegalTransactionTransition { .. })
    ));

    // 只能前进或进入补偿。
    store
        .advance_scale_transaction(
            &opened.transaction_id,
            ScaleTransactionState::Recovering,
            at(3),
        )
        .expect("提交后补偿");
    store
        .advance_scale_transaction(&opened.transaction_id, ScaleTransactionState::Done, at(4))
        .expect("补偿完成");
}

#[test]
fn one_epoch_admits_only_one_transaction_per_subject() {
    let (_dir, mut store) = temp_store();
    store
        .open_scale_transaction(&transaction("transaction:1", 1, 2), at(0))
        .expect("第一个事务");

    let duplicate = store.open_scale_transaction(&transaction("transaction:2", 1, 2), at(1));
    assert!(matches!(
        duplicate,
        Err(StorageError::TransactionAlreadyExists { new_epoch: 2, .. })
    ));

    // 另一个世代仍然可以开新事务。
    store
        .open_scale_transaction(&transaction("transaction:3", 2, 3), at(2))
        .expect("不同世代不冲突");
}

// ---------------------------------------------------------------------------
// 消息去重（§6.3）
// ---------------------------------------------------------------------------

#[test]
fn message_dedup_is_keyed_by_unit_and_event() {
    let (_dir, mut store) = temp_store();
    let first = UnitId::new("unit:file-summary:07").expect("固定单元");
    let second = UnitId::new("unit:file-summary:08").expect("固定单元");
    let event = EventId::parse("11111111-1111-4111-8111-111111111111").expect("固定事件");

    assert!(store.mark_event_processed(&first, &event, None, at(0)).unwrap());
    assert!(
        !store
            .mark_event_processed(&first, &event, None, at(1))
            .unwrap(),
        "同一单元处理同一条事件只能记一次"
    );
    // 不同单元各自处理同一条广播事件是正常的：去重键是 (unit_id, event_id)，
    // 不是单独的 event_id。
    assert!(store.mark_event_processed(&second, &event, None, at(2)).unwrap());

    assert!(store.has_processed(&first, &event).unwrap());
    assert_eq!(store.processed_event_count().unwrap(), 2);
}

#[test]
fn message_dedup_is_independent_from_action_idempotency() {
    let (_dir, mut store) = temp_store();
    let unit = UnitId::new("unit:file-summary:07").expect("固定单元");
    let event = EventId::parse("22222222-2222-4222-8222-222222222222").expect("固定事件");

    // 一条事件被处理过一次，但它可能产生过零个或多个动作。
    assert!(store.mark_event_processed(&unit, &event, Some("action:1"), at(0)).unwrap());
    assert!(!store.mark_event_processed(&unit, &event, Some("action:2"), at(1)).unwrap());
    // 动作账里没有任何记录：去重表不是动作账。
    assert_eq!(store.action_count().unwrap(), 0);
    assert_eq!(store.outbox_count().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// 崩溃与重启（§6.3）
// ---------------------------------------------------------------------------

#[test]
fn route_epoch_transactions_and_dedup_survive_a_restart() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");
    let event = EventId::parse("33333333-3333-4333-8333-333333333333").expect("固定事件");
    let unit = UnitId::new("unit:file-summary:07").expect("固定单元");

    {
        let mut store = Store::open(&path, at(0)).expect("打开存储");
        store
            .ensure_route(&subject(), &graph("v1"), at(0))
            .expect("建立路由");
        store
            .commit_route(
                &subject(),
                TopologyEpoch::INITIAL,
                TopologyEpoch::try_from(2).expect("合法世代"),
                &graph("v2"),
                at(1),
            )
            .expect("提交新世代");

        let opened = transaction("transaction:1", 1, 2);
        store.open_scale_transaction(&opened, at(2)).expect("打开事务");
        store
            .advance_scale_transaction(
                &opened.transaction_id,
                ScaleTransactionState::Reserved,
                at(3),
            )
            .expect("预留");

        store
            .mark_event_processed(&unit, &event, None, at(4))
            .expect("登记已处理");
        // 进程在这里崩溃。
    }

    let mut store = Store::open(&path, at(3600)).expect("重新打开");

    assert_eq!(
        store.route(&subject()).unwrap().unwrap().current_epoch.get(),
        2,
        "世代必须跨重启保留"
    );
    assert_eq!(
        store
            .scale_transaction(&transaction("transaction:1", 1, 2).transaction_id)
            .unwrap()
            .unwrap()
            .state,
        ScaleTransactionState::Reserved,
        "未完成的事务必须能续做，而不是被当成没有发生过"
    );
    assert!(store.has_processed(&unit, &event).unwrap());
    assert!(
        !store
            .mark_event_processed(&unit, &event, None, at(3601))
            .unwrap(),
        "重启后重复投递仍然必须被去重"
    );

    // 续做未完成的事务。
    store
        .advance_scale_transaction(
            &transaction("transaction:1", 1, 2).transaction_id,
            ScaleTransactionState::Draining,
            at(3602),
        )
        .expect("重启后继续推进");
}

// ---------------------------------------------------------------------------
// 状态修订（乐观并发）
// ---------------------------------------------------------------------------

#[test]
fn state_revision_only_moves_forward() {
    let (_dir, mut store) = temp_store();
    let unit = UnitId::new("unit:file-summary:07").expect("固定单元");

    assert!(matches!(
        store.bump_revision(&unit, at(0)),
        Err(StorageError::UnitNotFound { .. })
    ));

    register(&mut store, &unit);
    assert_eq!(store.bump_revision(&unit, at(1)).unwrap(), 1);
    assert_eq!(store.bump_revision(&unit, at(2)).unwrap(), 2);
    assert_eq!(
        store.instance(&unit).unwrap().unwrap().state_revision,
        2,
        "修订号必须落库，影子恢复才有可比对的依据"
    );
    // 快照写入不改变所有权信息。
    assert_eq!(
        store.instance(&unit).unwrap().unwrap().topology_epoch,
        TopologyEpoch::INITIAL
    );
}

#[test]
fn instances_are_listed_per_subject() {
    let (_dir, mut store) = temp_store();
    for index in 0..3 {
        let unit = UnitId::new(format!("unit:file-summary:{index:02}")).expect("固定单元");
        register(&mut store, &unit);
    }

    let listed = store.instances_of_subject(&subject(), 10).unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|item| item.subject_id == subject()));
    assert!(listed.iter().all(|item| item.lifecycle == UnitState::Cold));

    let other = SubjectId::new("subject:other").expect("固定主体");
    assert!(store.instances_of_subject(&other, 10).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 登记一个冷实例。本文件只关心注册信息，快照用一份最小的合法值即可。
fn register(store: &mut Store, unit: &UnitId) {
    let snapshot = UnitSnapshot {
        unit_id: unit.clone(),
        kind: UnitKind::Leaf,
        schema_version: SCHEMA_VERSION,
        scope: Scope {
            domain: DomainId::new("document-summary").expect("固定领域"),
            task_contract: TaskContractVersion::new("summary-v1").expect("固定合同"),
        },
        goal_refs: vec![GoalId::new("goal:42").expect("固定目标")],
        belief_revision: 0,
        belief_snapshot_ref: BlobRef::new("blob:belief-0").expect("固定对象"),
        evidence_refs: Vec::new(),
        relation_refs: Vec::new(),
        strategy_version: StrategyVersion::new("summary-policy-v1").expect("固定策略"),
        model_profile_ref: ModelProfileRef::new("profile:reasoning-local").expect("固定画像"),
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略"),
        budget_ref: BudgetRef::new("budget:task-42").expect("固定预算"),
        pending_action_ids: Vec::new(),
        last_applied_sequence: 0,
        state: UnitState::Cold,
    };

    let instance = UnitInstance::new(
        unit.clone(),
        subject(),
        None,
        TemplateId::new("template:file-summary").expect("固定模板"),
        PartitionKey::new("document-summary").expect("固定分片"),
    );

    assert!(
        store
            .register_instance(&snapshot, &instance, at(0))
            .expect("登记")
    );
}