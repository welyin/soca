//! 冷热单元与生命周期回归测试（§9.1、§9.2）。
//!
//! 这里要证明的不是"状态机能跑"，而是四件在真实系统里最容易做错的事：
//! 冷单元唤醒后必须补上它错过的世界；权限失效的单元不能被悄悄唤醒；
//! 未决动作不能被"卸载"掉；冷态与游标必须跨重启存活。

use soca_contracts::*;
use soca_core::*;
use soca_storage::Store;
use tempfile::TempDir;

const BOOT: &str = "00000000-0000-4000-8000-0000000000a1";
const CAP: &str = "cap:read-selected-folder";
const PROFILE: &str = "profile:reasoning-local";

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

fn unit_id(name: &str) -> UnitId {
    UnitId::new(name).expect("固定单元")
}

fn task() -> TaskId {
    TaskId::new("task:42").expect("固定任务")
}

fn cold_snapshot(unit: &UnitId, cursor: u64) -> UnitSnapshot {
    UnitSnapshot {
        unit_id: unit.clone(),
        kind: UnitKind::Leaf,
        schema_version: SCHEMA_VERSION,
        scope: Scope {
            domain: DomainId::new("document-summary").expect("固定领域"),
            task_contract: TaskContractVersion::new("summary-v1").expect("固定合同"),
        },
        goal_refs: vec![GoalId::new("goal:42").expect("固定目标")],
        belief_revision: 18,
        belief_snapshot_ref: BlobRef::new("blob:belief-18").expect("固定对象"),
        evidence_refs: Vec::new(),
        relation_refs: Vec::new(),
        strategy_version: StrategyVersion::new("summary-policy-v1").expect("固定策略"),
        model_profile_ref: ModelProfileRef::new(PROFILE).expect("固定画像"),
        capability_policy_ref: CapabilityPolicyRef::new(CAP).expect("固定能力策略"),
        budget_ref: BudgetRef::new("budget:task-42").expect("固定预算"),
        pending_action_ids: Vec::new(),
        last_applied_sequence: cursor,
        state: UnitState::Cold,
    }
}

fn policy() -> WakePolicy {
    WakePolicy::allowing([CAP], [PROFILE])
}

fn temp_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("临时目录");
    let store = Store::open(dir.path().join("soca.db"), at(0)).expect("打开存储");
    (dir, store)
}

fn observation(sequence: u64) -> Envelope {
    let adapter = SourceId::new("device:file-watcher").expect("固定来源");
    Envelope::new(
        EventId::generate(),
        adapter.clone(),
        1,
        boot(),
        sequence,
        task(),
        Vec::new(),
        at(0),
        Monotonic::new(boot(), sequence.saturating_mul(1_000_000)),
        Provenance::Sensor { adapter },
        PayloadRef::Blob {
            blob_ref: BlobRef::new(format!("blob:obs-{sequence}")).expect("固定对象"),
            media_type: MediaType::new("application/json").expect("固定媒体类型"),
            bytes: 64,
            sha256: Sha256Hex::of_bytes(&sequence.to_le_bytes()),
        },
        PermissionScope {
            capability_policy_ref: CapabilityPolicyRef::new(CAP).expect("固定能力策略"),
            max_action_level: ActionLevel::A1,
        },
        DataClass::Personal,
        None,
        IdempotencyKey::new(format!("idem:{sequence}")).expect("固定幂等键"),
    )
}

// ---------------------------------------------------------------------------
// 登记与唤醒
// ---------------------------------------------------------------------------

#[test]
fn a_registered_unit_wakes_with_the_events_it_missed() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    assert!(
        store
            .register_unit(&cold_snapshot(&unit, 0), at(0))
            .expect("登记")
    );

    // 单元冷着的时候，世界里发生了三件事。
    for sequence in 1..=3 {
        store
            .append_event(&observation(sequence), at(sequence as i64))
            .expect("追加事件");
    }

    let mut registry = UnitRegistry::new(2);
    let outcome = registry
        .wake(&mut store, &unit, &policy(), at(10))
        .expect("唤醒");

    let WakeOutcome::Ready {
        snapshot,
        missed_events,
    } = outcome
    else {
        panic!("权限与画像都有效，应当进入 READY");
    };
    assert_eq!(snapshot.state, UnitState::Ready);
    assert_eq!(missed_events.len(), 3, "冷启动必须补上游标之后的事件");
    assert_eq!(missed_events[0].sequence, 1);
    assert_eq!(missed_events[2].sequence, 3);

    // 目录里也已经是 READY：重启后不会被当成冷的，也不会丢掉游标。
    let persisted = store.unit(&unit).unwrap().unwrap();
    assert_eq!(persisted.state, UnitState::Ready);
    assert_eq!(persisted.last_applied_sequence, 0);
    assert_eq!(store.hot_unit_count().unwrap(), 1);
    assert_eq!(registry.hot_count(), 1);
}

#[test]
fn waking_an_already_hot_unit_changes_nothing() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();
    store.append_event(&observation(1), at(1)).unwrap();

    let mut registry = UnitRegistry::new(2);
    registry.wake(&mut store, &unit, &policy(), at(10)).unwrap();
    store.append_event(&observation(2), at(2)).unwrap();

    let outcome = registry.wake(&mut store, &unit, &policy(), at(11)).unwrap();
    assert!(matches!(outcome, WakeOutcome::AlreadyHot { .. }));
    assert_eq!(registry.hot_count(), 1);
    // 已经是热的，不该再补一次历史。
    let WakeOutcome::AlreadyHot { snapshot } = outcome else {
        unreachable!()
    };
    assert_eq!(snapshot.last_applied_sequence, 0);
}

#[test]
fn a_unit_whose_capability_was_revoked_cannot_be_woken() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    // 能力策略已被撤回（§12.2：撤回立即生效）。
    let revoked = WakePolicy::allowing(Vec::<String>::new(), [PROFILE]);
    let outcome = registry.wake(&mut store, &unit, &revoked, at(10)).unwrap();

    let WakeOutcome::Refused { reason } = outcome else {
        panic!("能力策略失效必须拒绝唤醒");
    };
    assert!(reason.contains(CAP), "拒绝原因必须点明是哪个策略失效");
    assert_eq!(registry.hot_count(), 0);
    // 目录里必须还是冷的：拒绝不能留下"看起来已经加载"的痕迹。
    assert_eq!(store.unit(&unit).unwrap().unwrap().state, UnitState::Cold);
    assert_eq!(store.hot_unit_count().unwrap(), 0);
}

#[test]
fn a_unit_whose_model_profile_is_gone_cannot_be_woken() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    let no_models = WakePolicy::allowing([CAP], Vec::<String>::new());
    let outcome = registry.wake(&mut store, &unit, &no_models, at(10)).unwrap();

    let WakeOutcome::Refused { reason } = outcome else {
        panic!("模型画像不可用必须拒绝唤醒");
    };
    assert!(reason.contains(PROFILE));
    assert_eq!(registry.hot_count(), 0);
}

#[test]
fn the_default_wake_policy_refuses_everything() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    // 忘记配置的后果必须是拒绝唤醒，而不是悄悄放行（§12.2 失败关闭）。
    let outcome = registry
        .wake(&mut store, &unit, &WakePolicy::default(), at(10))
        .unwrap();
    assert!(matches!(outcome, WakeOutcome::Refused { .. }));
    assert_eq!(registry.hot_count(), 0);
}

#[test]
fn a_unit_left_hot_by_a_crash_cannot_be_woken_over() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");

    // 模拟上一次运行：目录里留着 RUNNING，进程却已经没了。
    let mut stranded = cold_snapshot(&unit, 0);
    stranded.state = UnitState::Running;
    store.save_unit(&stranded, at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    let outcome = registry.wake(&mut store, &unit, &policy(), at(10)).unwrap();
    let WakeOutcome::Refused { reason } = outcome else {
        panic!("不能直接覆盖上一次运行的残留状态");
    };
    assert!(reason.contains("RUNNING"));
}

#[test]
fn a_unit_must_be_cold_to_enter_the_catalogue() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");

    let mut running = cold_snapshot(&unit, 0);
    running.state = UnitState::Running;
    let rejected = store.register_unit(&running, at(0));
    assert!(matches!(
        rejected,
        Err(soca_storage::StorageError::UnitMustBeColdAtRegistration { .. })
    ));
    assert_eq!(store.unit_count().unwrap(), 0);

    assert!(store.register_unit(&cold_snapshot(&unit, 0), at(1)).unwrap());
    // 重复登记不是错误，只是没有新增（幂等）。
    assert!(!store.register_unit(&cold_snapshot(&unit, 0), at(2)).unwrap());
    assert_eq!(store.unit_count().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// 唤醒请求的合并与预算（§9.2、§10.4）
// ---------------------------------------------------------------------------

#[test]
fn wake_requests_are_merged_and_kept_within_budget() {
    let mut registry = UnitRegistry::new(2);
    let units: Vec<UnitId> = (0..5)
        .map(|index| unit_id(&format!("unit:file-summary:{index:02}")))
        .collect();

    // 同一单元的并发唤醒请求必须被合并。
    assert!(registry.request_wake(&units[0]));
    assert!(!registry.request_wake(&units[0]));
    assert_eq!(registry.queued_wakes().len(), 1);

    for unit in &units[1..] {
        assert!(registry.request_wake(unit));
    }
    assert_eq!(registry.queued_wakes().len(), 5);

    // 一批最多加载 wake_concurrency 个，其余留在队列里等下一批。
    let batch = registry.take_wake_batch();
    assert_eq!(batch.len(), 2);
    assert_eq!(registry.queued_wakes().len(), 3);

    let second = registry.take_wake_batch();
    assert_eq!(second.len(), 2);
    assert_eq!(registry.take_wake_batch().len(), 1);
    assert!(registry.queued_wakes().is_empty());
}

// ---------------------------------------------------------------------------
// 降温与冷恢复（§9.2）
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_hands_pending_actions_over_and_goes_cold() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    registry.wake(&mut store, &unit, &policy(), at(10)).unwrap();
    registry
        .set_state(&mut store, &unit, UnitState::Running, at(11))
        .unwrap();

    // 两个动作已经交给执行代理，但副作用是否发生还未知。
    let first = ActionId::new("action:1").expect("固定动作");
    let second = ActionId::new("action:2").expect("固定动作");
    registry
        .note_pending_action(&mut store, &unit, &first, at(12))
        .unwrap();
    registry
        .note_pending_action(&mut store, &unit, &second, at(12))
        .unwrap();
    // 重复登记是幂等的。
    registry
        .note_pending_action(&mut store, &unit, &first, at(12))
        .unwrap();
    assert_eq!(
        registry.hot(&unit).unwrap().pending_action_ids.len(),
        2,
        "未决动作不得重复登记"
    );

    registry
        .set_state(&mut store, &unit, UnitState::Ready, at(13))
        .unwrap();
    let outcome = registry.checkpoint(&mut store, &unit, at(14)).unwrap();

    assert_eq!(outcome.snapshot.state, UnitState::Cold);
    assert_eq!(
        outcome.handed_over,
        vec!["action:1".to_string(), "action:2".to_string()],
        "未决动作必须移交给在线动作账，而不是随单元一起消失"
    );
    assert!(outcome.snapshot.pending_action_ids.is_empty());
    assert_eq!(registry.hot_count(), 0);
    assert_eq!(store.unit(&unit).unwrap().unwrap().state, UnitState::Cold);
    assert_eq!(store.hot_unit_count().unwrap(), 0);

    // 再降温一次应当报"不在热表中"，而不是静默成功。
    assert!(matches!(
        registry.checkpoint(&mut store, &unit, at(15)),
        Err(CoreError::UnitNotHot { .. })
    ));
}

#[test]
fn cold_state_and_cursor_survive_a_restart() {
    let dir = TempDir::new().expect("临时目录");
    let path = dir.path().join("soca.db");
    let unit = unit_id("unit:file-summary:07");

    {
        let mut store = Store::open(&path, at(0)).expect("打开存储");
        store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();
        for sequence in 1..=3 {
            store.append_event(&observation(sequence), at(sequence as i64)).unwrap();
        }

        let mut registry = UnitRegistry::new(2);
        let WakeOutcome::Ready {
            missed_events, ..
        } = registry.wake(&mut store, &unit, &policy(), at(10)).unwrap()
        else {
            panic!("应当唤醒");
        };
        assert_eq!(missed_events.len(), 3);

        registry
            .set_state(&mut store, &unit, UnitState::Running, at(11))
            .unwrap();
        registry.advance_cursor(&mut store, &unit, 3, at(12)).unwrap();
        registry
            .set_state(&mut store, &unit, UnitState::Ready, at(12))
            .unwrap();
        let outcome = registry.checkpoint(&mut store, &unit, at(13)).unwrap();
        assert_eq!(outcome.snapshot.last_applied_sequence, 3);
        // 进程在这里退出。
    }

    let mut store = Store::open(&path, at(100)).expect("重新打开");
    let restored = store.unit(&unit).unwrap().unwrap();
    assert_eq!(restored.state, UnitState::Cold);
    assert_eq!(restored.last_applied_sequence, 3);
    assert_eq!(restored.belief_revision, 18);
    assert_eq!(store.unit_count().unwrap(), 1);
    assert_eq!(store.hot_unit_count().unwrap(), 0, "冷态不占用热配额");

    // 冷恢复：游标已经推到末尾，不该再补一遍历史。
    let mut registry = UnitRegistry::new(2);
    let WakeOutcome::Ready {
        missed_events, ..
    } = registry.wake(&mut store, &unit, &policy(), at(100)).unwrap()
    else {
        panic!("冷恢复应当成功");
    };
    assert!(
        missed_events.is_empty(),
        "游标之后没有新事件，唤醒不应重复补历史"
    );
}

#[test]
fn the_cursor_cannot_go_backwards() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 5), at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    registry.wake(&mut store, &unit, &policy(), at(10)).unwrap();

    registry.advance_cursor(&mut store, &unit, 9, at(11)).unwrap();
    // 同一位置重复推进是允许的（幂等处理重复事件）。
    registry.advance_cursor(&mut store, &unit, 9, at(11)).unwrap();

    let backwards = registry.advance_cursor(&mut store, &unit, 4, at(12));
    assert!(matches!(
        backwards,
        Err(CoreError::CursorWentBackwards {
            current: 9,
            attempted: 4,
            ..
        })
    ));
    assert_eq!(
        store.unit(&unit).unwrap().unwrap().last_applied_sequence,
        9,
        "回退尝试不得改写已提交的游标"
    );
}

#[test]
fn state_transitions_outside_the_documented_graph_are_rejected() {
    let (_dir, mut store) = temp_store();
    let unit = unit_id("unit:file-summary:07");
    store.register_unit(&cold_snapshot(&unit, 0), at(0)).unwrap();

    let mut registry = UnitRegistry::new(2);
    registry.wake(&mut store, &unit, &policy(), at(10)).unwrap();

    // §9.2 的状态图要求 READY 先进入 CHECKPOINTING 才能降温。
    let skipped = registry.set_state(&mut store, &unit, UnitState::Cold, at(11));
    assert!(matches!(skipped, Err(CoreError::Contract(_))));
    assert_eq!(registry.hot(&unit).unwrap().state, UnitState::Ready);
}
