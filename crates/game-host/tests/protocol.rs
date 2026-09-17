//! 游戏宿主协议回归测试（工单 ENG-06 宿主侧；验收 GM-04、GM-07 的协议部分）。
//!
//! 本文件用 [`ProtocolProbeEngine`] 作为引擎。它**不是迷宫也不是扫雷**，规则行为是任意的，
//! 只用来确定性地触发协议分支。真实引擎的规则一致性属于 ENG-07/ENG-08，不在本文件覆盖范围。

use soca_contracts::*;
use soca_game_host::*;
use soca_storage::Store;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

const PROBE_SEED: u64 = 987_654_321;

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("固定基准时间")
}

fn at(seconds: i64) -> WallClock {
    base_time().plus_seconds(seconds)
}

fn session_id() -> PublicId {
    PublicId::new("session-public-8").expect("固定会话")
}

fn episode_id() -> PublicId {
    PublicId::new("episode-public-104").expect("固定回合")
}

fn temp_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("临时目录");
    let store = Store::open(dir.path().join("soca.db"), at(0)).expect("打开存储");
    (dir, store)
}

fn forward() -> GameAction {
    GameAction::Move(MoveAction { op: MoveOp::Forward })
}

fn set_flag(row: u8, column: u8, flagged: bool) -> GameAction {
    GameAction::Flag(FlagAction {
        op: SetFlagOp::SetFlag,
        row,
        column,
        flagged,
    })
}

fn request(
    request_id: &str,
    expected_observation_id: &PublicId,
    topology_epoch: u64,
    action: GameAction,
) -> ActionRequest {
    ActionRequest {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: ActionRequestTag::ActionRequest,
        request_id: PublicId::new(request_id).expect("固定请求"),
        episode_id: episode_id(),
        expected_observation_id: expected_observation_id.clone(),
        actor_id: PublicId::new("unit:mines:frontier-2").expect("固定单元"),
        topology_epoch,
        permit_id: PublicId::new("permit-game-87").expect("固定许可"),
        action,
    }
}

/// 起一个回合，返回宿主与初始观测。
fn start<'a>(
    store: &'a mut Store,
    game: GameKind,
    limit: u8,
) -> (GameHost<'a>, GameObservation) {
    let mut host = GameHost::new(store, HostConfig::default());
    let observation = host
        .start_episode(
            session_id(),
            episode_id(),
            game,
            PROBE_SEED,
            9,
            &ProbeFactory::new(limit),
        )
        .expect("开局");
    (host, observation)
}

// ---------------------------------------------------------------------------
// 幂等（GM-04）
// ---------------------------------------------------------------------------

#[test]
fn a_replayed_request_never_advances_the_world_twice() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);

    let first = request("action-87", &observation.observation_id, 9, forward());
    let receipt = host.submit(&first, at(1)).expect("首次提交");
    assert_eq!(receipt.status, ReceiptStatus::Applied);
    let after_first = host.observe(&episode_id()).expect("观测");
    assert_eq!(after_first.step_index, 1);

    // 同一请求 ID、同一载荷重发：返回原回执，且不推进世界。
    let replay = host.submit(&first, at(2)).expect("重发");
    assert_eq!(replay, receipt, "必须返回当初那一份回执");
    assert_eq!(
        host.observe(&episode_id()).expect("观测").step_index,
        1,
        "重发不得产生第二次世界步"
    );
}

#[test]
fn the_same_request_id_with_a_different_payload_is_a_conflict() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);

    let first = request("action-87", &observation.observation_id, 9, forward());
    host.submit(&first, at(1)).expect("首次提交");

    // 同一标识、不同动作：这不是重试，是两次不同的意图抢一个标识。
    let mut conflicting = first.clone();
    conflicting.action = GameAction::Move(MoveAction {
        op: MoveOp::TurnLeft,
    });
    let receipt = host.submit(&conflicting, at(2)).expect("冲突提交");
    assert_eq!(receipt.status, ReceiptStatus::Rejected);
    assert_eq!(receipt.code, ReceiptCode::IdempotencyConflict);
    assert_eq!(
        host.observe(&episode_id()).expect("观测").step_index,
        1,
        "冲突请求不得推进世界"
    );
}

#[test]
fn an_unknown_outcome_truncates_instead_of_replaying() {
    let (_dir, mut store) = temp_store();

    // 模拟上一次运行：请求已登记，宿主在 step 与回执之间崩溃。
    let probe = request("action-87", &PublicId::new("obs-public-27").expect("固定观测"), 9, forward());
    assert!(store.record_game_request(&probe, at(0)).expect("登记").permits_step());

    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);
    // 观测 ID 与登记时的不同，但幂等判定先于新鲜度判定：结局未知优先。
    assert_ne!(observation.observation_id.as_str(), "obs-public-27");

    let receipt = host.submit(&probe, at(1)).expect("提交");
    assert_eq!(
        receipt.status,
        ReceiptStatus::UnknownCommit,
        "登记在册却没有回执，只能判定结局未知"
    );
    assert_eq!(receipt.code, ReceiptCode::EngineUnavailable);
    assert!(receipt.observation_id.is_none(), "结局未知时不得给出新观测");
    assert!(host.is_finished(&episode_id()), "未验证可恢复的回合标为截断");
    assert!(host.is_truncated(&episode_id()));
    assert_eq!(host.observe(&episode_id()).expect("观测").outcome, Outcome::InfrastructureError);
}

// ---------------------------------------------------------------------------
// 新鲜度（GM-04）
// ---------------------------------------------------------------------------

#[test]
fn an_expired_observation_is_rejected_instead_of_being_applied_to_a_new_board() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);

    let first = request("action-87", &observation.observation_id, 9, forward());
    host.submit(&first, at(1)).expect("首次提交");

    // 提交者仍然依据开局那一帧。
    let stale = request("action-88", &observation.observation_id, 9, forward());
    let receipt = host.submit(&stale, at(2)).expect("提交");
    assert_eq!(receipt.status, ReceiptStatus::Rejected);
    assert_eq!(receipt.code, ReceiptCode::StaleObservation);
    assert_eq!(
        host.observe(&episode_id()).expect("观测").step_index,
        1,
        "过期观测不得推进世界"
    );
}

#[test]
fn an_old_topology_epoch_is_rejected_after_migration() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);

    // 迁移提交后世代前进。
    host.advance_epoch(&episode_id(), 10).expect("推进世代");

    let stale = request("action-87", &observation.observation_id, 9, forward());
    let receipt = host.submit(&stale, at(1)).expect("提交");
    assert_eq!(receipt.code, ReceiptCode::StaleTopology);

    // 新世代的同一动作被接受。
    let fresh = request("action-88", &observation.observation_id, 10, forward());
    assert_eq!(
        host.submit(&fresh, at(2)).expect("提交").status,
        ReceiptStatus::Applied
    );
}

#[test]
fn a_request_after_the_episode_ends_is_rejected() {
    let (_dir, mut store) = temp_store();
    let (mut host, mut observation) = start(&mut store, GameKind::Maze, 1);

    let first = request("action-87", &observation.observation_id, 9, forward());
    host.submit(&first, at(1)).expect("提交");
    observation = host.observe(&episode_id()).expect("观测");
    assert!(observation.terminated);
    assert!(host.is_finished(&episode_id()));

    let after = request("action-88", &observation.observation_id, 9, forward());
    let receipt = host.submit(&after, at(2)).expect("提交");
    assert_eq!(receipt.code, ReceiptCode::EpisodeFinished);
}

#[test]
fn a_malformed_request_does_not_advance_the_world() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);

    // 世代 0 无法与"未迁移"区分，属语义非法。
    let malformed = request("action-87", &observation.observation_id, 0, forward());
    let receipt = host.submit(&malformed, at(1)).expect("提交");
    assert_eq!(receipt.code, ReceiptCode::InvalidAction);
    assert_eq!(host.observe(&episode_id()).expect("观测").step_index, 0);
}

#[test]
fn an_action_from_another_domain_is_rejected() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);

    // 迷宫回合收到扫雷动作。
    let wrong = request("action-87", &observation.observation_id, 9, set_flag(0, 0, true));
    let receipt = host.submit(&wrong, at(1)).expect("提交");
    assert_eq!(receipt.code, ReceiptCode::InvalidAction);
    assert_eq!(host.observe(&episode_id()).expect("观测").step_index, 0);
}

// ---------------------------------------------------------------------------
// 违规预算与截断（GM-07）
// ---------------------------------------------------------------------------

#[test]
fn consecutive_violations_truncate_the_episode_instead_of_being_retried_forever() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);
    let limit = HostConfig::default().max_consecutive_violations;

    // 连续发合法但过期的请求，直到用满违规预算。
    for index in 0..limit {
        let stale = request(
            &format!("action-{index}"),
            &PublicId::new("obs-nonexistent").expect("固定观测"),
            9,
            forward(),
        );
        let receipt = host.submit(&stale, at(i64::from(index) + 1)).expect("提交");
        assert_eq!(receipt.code, ReceiptCode::StaleObservation);
    }

    assert!(host.is_truncated(&episode_id()), "连续违规应当截断回合");
    let final_observation = host.observe(&episode_id()).expect("观测");
    assert!(!final_observation.terminated, "协议失败不是规则判负");
    assert!(final_observation.truncated);
    assert_eq!(final_observation.outcome, Outcome::Aborted);

    // 截断之后不再接受任何请求。
    let after = request("action-after", &observation.observation_id, 9, forward());
    assert_eq!(
        host.submit(&after, at(99)).expect("提交").code,
        ReceiptCode::EpisodeFinished
    );
}

#[test]
fn a_successful_step_clears_the_violation_counter() {
    let (_dir, mut store) = temp_store();
    let (mut host, _) = start(&mut store, GameKind::Maze, 8);

    let limit = HostConfig::default().max_consecutive_violations;
    for index in 0..limit - 1 {
        let stale = request(
            &format!("action-stale-{index}"),
            &PublicId::new("obs-nonexistent").expect("固定观测"),
            9,
            forward(),
        );
        host.submit(&stale, at(i64::from(index) + 1)).expect("提交");
    }
    assert!(!host.is_truncated(&episode_id()));

    // 一次成功的推理把连续违规清零，回合不该因为远处的旧错误被拖垮。
    let observation = host.observe(&episode_id()).expect("观测");
    let good = request("action-good", &observation.observation_id, 9, forward());
    assert_eq!(
        host.submit(&good, at(50)).expect("提交").status,
        ReceiptStatus::Applied
    );
    assert!(!host.is_truncated(&episode_id()));
}

#[test]
fn a_natural_end_is_not_a_truncation() {
    let (_dir, mut store) = temp_store();
    let (mut host, mut observation) = start(&mut store, GameKind::Maze, 2);

    for index in 0..2 {
        let step = request(
            &format!("action-{index}"),
            &observation.observation_id,
            9,
            forward(),
        );
        host.submit(&step, at(i64::from(index) + 1)).expect("提交");
        observation = host.observe(&episode_id()).expect("观测");
    }

    assert!(observation.terminated, "达到步数上限是规则自然终局");
    assert!(!observation.truncated);
    assert_eq!(observation.outcome, Outcome::Won);
    assert_eq!(observation.reward, 1.0);
    assert!(!host.is_truncated(&episode_id()));
}

// ---------------------------------------------------------------------------
// 无变化（§11.3）
// ---------------------------------------------------------------------------

#[test]
fn an_accepted_but_unchanging_action_is_reported_as_no_change_with_a_new_observation() {
    let (_dir, mut store) = temp_store();
    let (mut host, mut observation) = start(&mut store, GameKind::Minesweeper, 8);

    let set = request("action-87", &observation.observation_id, 9, set_flag(1, 1, true));
    assert_eq!(
        host.submit(&set, at(1)).expect("提交").status,
        ReceiptStatus::Applied
    );
    observation = host.observe(&episode_id()).expect("观测");

    // 按意图幂等设置：目标值与当前值相同，局面不变。
    let again = request("action-88", &observation.observation_id, 9, set_flag(1, 1, true));
    let receipt = host.submit(&again, at(2)).expect("提交");
    assert_eq!(receipt.status, ReceiptStatus::NoChange);
    assert_eq!(receipt.code, ReceiptCode::NoChange);
    assert!(
        receipt.observation_id.is_some(),
        "无变化仍然消耗一步并产生新观测"
    );
    assert_eq!(host.observe(&episode_id()).expect("观测").step_index, 2);
}

// ---------------------------------------------------------------------------
// 可见性隔离（GM-03、GM-05）
// ---------------------------------------------------------------------------

#[test]
fn the_public_observation_carries_no_hidden_world_state() {
    let (_dir, mut store) = temp_store();
    let (mut host, mut observation) = start(&mut store, GameKind::Minesweeper, 8);

    // 走一步之后再检查：初始观测与动作后的观测都必须不携带隐藏状态。
    let step = request("action-87", &observation.observation_id, 9, set_flag(0, 0, true));
    host.submit(&step, at(1)).expect("提交");
    observation = host.observe(&episode_id()).expect("观测");

    let text = serde_json::to_string(&observation).expect("可序列化");
    for forbidden in [
        "info",
        "seed",
        "rng_state",
        "mine_positions",
        "safe_cells",
        "full_map",
        "solution",
        "unwrapped",
        "grid",
    ] {
        assert!(
            !text.contains(&format!("\"{forbidden}\"")),
            "公开观测里出现了隐藏字段 {forbidden}"
        );
    }

    // seed 不进入任何数值字段。
    let value: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(value["protocol_version"], serde_json::json!(1));
    assert_eq!(value["step_index"], serde_json::json!(1));
    assert_eq!(value["reward"], serde_json::json!(0.0));
    assert!(value.get("seed").is_none());

    // 观测标识是随机 UUID，而不是可推算的步序。
    assert_eq!(observation.observation_id.as_str().len(), 4 + 36);

    // 相位一致性由宿主与契约层双方校验。
    observation.validate_semantics().expect("公开观测必须自洽");
}

#[test]
fn the_host_debug_view_does_not_print_engine_internals() {
    let (_dir, mut store) = temp_store();
    let (host, _observation) = start(&mut store, GameKind::Minesweeper, 8);
    let text = format!("{host:?}");
    assert!(text.contains("GameHost"));
    assert!(
        !text.contains(&PROBE_SEED.to_string()),
        "调试输出不得带上 seed 或引擎内部状态"
    );
}

// ---------------------------------------------------------------------------
// 私有控制面（§11.3）
// ---------------------------------------------------------------------------

#[test]
fn starting_an_episode_twice_is_rejected() {
    let (_dir, mut store) = temp_store();
    let (mut host, _observation) = start(&mut store, GameKind::Maze, 8);

    let again = host.start_episode(
        session_id(),
        episode_id(),
        GameKind::Maze,
        PROBE_SEED,
        9,
        &ProbeFactory::new(8),
    );
    assert!(matches!(again, Err(HostError::EpisodeAlreadyExists { .. })));
}

#[test]
fn observing_an_unknown_episode_is_an_error_not_an_empty_observation() {
    let (_dir, mut store) = temp_store();
    let host = GameHost::new(&mut store, HostConfig::default());
    let other = PublicId::new("episode-never-started").expect("固定回合");
    assert!(matches!(
        host.observe(&other),
        Err(HostError::UnknownEpisode { .. })
    ));
}

#[test]
fn the_ledger_records_every_settled_request() {
    let (_dir, mut store) = temp_store();
    let (mut host, observation) = start(&mut store, GameKind::Maze, 8);
    let good = request("action-87", &observation.observation_id, 9, forward());
    host.submit(&good, at(1)).expect("提交");
    drop(host);

    let entry = store
        .game_request(&episode_id(), &good.request_id)
        .expect("读取账")
        .expect("必须留痕");
    assert_eq!(entry.status, GameLedgerStatus::Applied);
    assert_eq!(entry.topology_epoch, 9);
    assert!(entry.settled_at.is_some());
    assert!(store.unsettled_game_requests(&episode_id(), 10).unwrap().is_empty());
}
