//! 真实游戏进程的集成测试：Rust 宿主 → 帧协议 → Python 迷宫进程。
//!
//! 与 `protocol.rs` 的区别在于引擎：那里用测试替身验证协议分支，这里启动真进程，
//! 验证帧格式、握手、规则版本与跨语言一致性。它需要仓库根的 `.venv`；缺失时默认跳过
//! 并打印原因，设 `SOCA_REQUIRE_GAME_PROCESS=1` 可改为强制失败（供 CI 的独立任务使用）。

use std::path::{Path, PathBuf};

use soca_contracts::*;
use soca_game_host::*;
use soca_storage::Store;
use tempfile::TempDir;

const PROBE_SEED: u64 = 7;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("仓库根必须存在")
}

fn interpreter() -> PathBuf {
    let root = repository_root();
    for candidate in [
        root.join(".venv").join("Scripts").join("python.exe"),
        root.join(".venv").join("bin").join("python"),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "找不到仓库根的 .venv 解释器。请先执行：python -m venv .venv，然后 \
         .venv/Scripts/python -m pip install -r games/adapters/minigrid/requirements.txt"
    );
}

/// 缺少 Python 环境时跳过。设 `SOCA_REQUIRE_GAME_PROCESS=1` 则强制失败。
fn require_game_process() -> Option<PathBuf> {
    let root = repository_root();
    let entry = root
        .join("games")
        .join("adapters")
        .join("minigrid")
        .join("driver.py");
    let has_venv = root.join(".venv").exists();
    if has_venv && entry.exists() {
        return Some(root);
    }
    let message = format!(
        "SKIP：缺少 .venv（{has_venv}）或 games/adapters/minigrid/driver.py（{}）。\
         这条集成测试需要独立锁定的 Python 引擎环境。",
        entry.exists()
    );
    if std::env::var_os("SOCA_REQUIRE_GAME_PROCESS").is_some() {
        panic!("{message}");
    }
    eprintln!("{message}");
    None
}

/// 指向**同一份驱动与同一份清单**，只换 `GameKind`。
///
/// 这是刻意的：`a_handshake_against_the_wrong_game_is_refused` 那条要的正是
/// "用迷宫进程冒充扫雷"——所以两边的可执行文件必须**相同**，差别只在配置里声明的种类。
fn config(root: &Path, game: GameKind) -> ProcessEngineConfig {
    let driver = root
        .join("games")
        .join("adapters")
        .join("minigrid")
        .join("driver.py");
    let manifest = root.join("games").join("door-key").join("manifest.json");
    let mut config = ProcessEngineConfig::new(
        interpreter(),
        driver.to_str().expect("路径必须是 UTF-8"),
        game,
    );
    config.args.push("--manifest".to_string());
    config.args.push(manifest.display().to_string());
    config.working_directory = Some(root.to_path_buf());
    config
}

fn at(seconds: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("固定基准时间")
        .plus_seconds(seconds)
}

// ---------------------------------------------------------------------------
// 进程与协议
// ---------------------------------------------------------------------------

#[test]
fn a_real_maze_process_completes_the_handshake() {
    let Some(root) = require_game_process() else {
        return;
    };
    let engine = ProcessEngine::spawn(config(&root, GameKind::Maze)).expect("启动迷宫进程");

    assert_eq!(engine.game(), GameKind::Maze);
    // 规则版本是**游戏**的（清单里那一条），而它是区分"哪一套规则跑出来的这一局"的依据。
    assert_eq!(engine.rules_version(), Some("door-key-8x8-v1"));
    assert!(engine.is_usable());
    // manifest 声明该适配器不可确定性恢复，引擎接口必须如实反映这一点。
    assert!(!engine.supports_snapshot());
}

#[test]
fn a_real_maze_process_projects_a_valid_public_observation() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = ProcessEngine::spawn(config(&root, GameKind::Maze)).expect("启动迷宫进程");

    let step = engine.reset(PROBE_SEED).expect("重置");
    assert!(!step.terminated);
    assert!(!step.truncated);
    assert_eq!(step.outcome, Outcome::Running);

    // 公开感知必须能通过契约层的语义校验：跨语言的一致性就靠这一条兜底。
    let observation = GameObservation {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: GameObservationTag::Observation,
        session_id: PublicId::new("session-probe").expect("固定会话"),
        episode_id: PublicId::new("episode-probe").expect("固定回合"),
        observation_id: PublicId::new("obs-probe").expect("固定观测"),
        step_index: 0,
        game: GameKind::Maze,
        terminated: step.terminated,
        truncated: step.truncated,
        outcome: step.outcome,
        reward: step.reward,
        percept: step.percept.clone(),
    };
    observation
        .validate_semantics()
        .expect("Python 侧投影出的感知必须通过 Rust 侧语义校验");

    let Percept::Maze(view) = &observation.percept else {
        panic!("迷宫进程必须给出符号迷宫感知");
    };
    assert_eq!(view.view.len(), 7);
    assert!(view.view.iter().all(|row| row.len() == 7));
    assert!(view.direction <= 3);
}

#[test]
fn engine_only_actions_are_refused_by_the_real_process() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = ProcessEngine::spawn(config(&root, GameKind::Maze)).expect("启动迷宫进程");
    engine.reset(PROBE_SEED).expect("重置");

    // 先确认进程认为自己在跑。
    assert!(engine
        .step(GameAction::Move(MoveAction { op: MoveOp::Forward }))
        .is_ok());

    // 宿主与契约层都拦不住的"引擎自报非法"路径：这里直接绕过宿主调用引擎。
    let refused = engine.step(GameAction::Flag(FlagAction {
        op: SetFlagOp::SetFlag,
        row: 0,
        column: 0,
        flagged: true,
    }));
    assert!(
        matches!(refused, Err(EngineError::InvalidAction { .. })),
        "扫雷动作送到迷宫进程必须被判为非法，实际为 {refused:?}"
    );

    // 被拒绝之后引擎仍可用：一次非法动作不该打掉整局。
    assert!(engine.is_usable());
    assert!(engine
        .step(GameAction::Move(MoveAction { op: MoveOp::TurnLeft }))
        .is_ok());
}

#[test]
fn a_handshake_against_the_wrong_game_is_refused() {
    let Some(root) = require_game_process() else {
        return;
    };
    // 用迷宫进程冒充扫雷：握手必须失败，而不是等到第一次动作才发现。
    let refused = ProcessEngine::spawn(config(&root, GameKind::Minesweeper));
    assert!(
        refused.is_err(),
        "进程自报游戏与配置不符时必须拒绝启动"
    );
}

// ---------------------------------------------------------------------------
// 宿主驱动真实进程
// ---------------------------------------------------------------------------

#[test]
fn the_host_drives_a_real_episode_through_the_process_engine() {
    let Some(root) = require_game_process() else {
        return;
    };

    let directory = TempDir::new().expect("临时目录");
    let mut store = Store::open(directory.path().join("soca.db"), at(0)).expect("打开存储");
    let factory = ProcessFactory::new(config(&root, GameKind::Maze));
    let mut host = GameHost::new(&mut store, HostConfig::default());

    let episode = PublicId::new("episode-public-1").expect("固定回合");
    let observation = host
        .start_episode(
            PublicId::new("session-public-1").expect("固定会话"),
            episode.clone(),
            GameKind::Maze,
            PROBE_SEED,
            1,
            &factory,
        )
        .expect("开局");

    // 走三步合法动作，每一步都基于上一步的观测。
    let initial_observation_id = observation.observation_id.clone();
    let mut current = observation;
    let actions = [
        GameAction::Move(MoveAction { op: MoveOp::TurnLeft }),
        GameAction::Move(MoveAction { op: MoveOp::Forward }),
        GameAction::Move(MoveAction { op: MoveOp::Forward }),
    ];
    for (index, action) in actions.into_iter().enumerate() {
        let request = ActionRequest {
            protocol_version: GAME_PROTOCOL_VERSION,
            message_type: ActionRequestTag::ActionRequest,
            request_id: PublicId::new(format!("action-{index}")).expect("固定请求"),
            episode_id: episode.clone(),
            expected_observation_id: current.observation_id.clone(),
            actor_id: PublicId::new("unit:maze:explorer-1").expect("固定单元"),
            topology_epoch: 1,
            permit_id: PublicId::new("permit-game-1").expect("固定许可"),
            action,
        };
        let receipt = host.submit(&request, at(i64::try_from(index).unwrap() + 1)).expect("提交");
        assert_eq!(receipt.status, ReceiptStatus::Applied, "第 {index} 步应当被接受");
        current = host.observe(&episode).expect("观测");
        assert_eq!(current.step_index, u64::try_from(index).unwrap() + 1);
    }

    // 过期观测必须被拒绝，而不是把旧动作应用在新局面上。
    let stale = ActionRequest {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: ActionRequestTag::ActionRequest,
        request_id: PublicId::new("action-stale").expect("固定请求"),
        episode_id: episode.clone(),
        expected_observation_id: initial_observation_id.clone(),
        actor_id: PublicId::new("unit:maze:explorer-1").expect("固定单元"),
        topology_epoch: 1,
        permit_id: PublicId::new("permit-game-1").expect("固定许可"),
        action: GameAction::Move(MoveAction { op: MoveOp::Forward }),
    };
    assert_eq!(
        host.submit(&stale, at(99)).expect("提交").code,
        ReceiptCode::StaleObservation
    );

    drop(host);

    // 每一步都必须在账上留痕，且都带拓扑世代。
    let entry = store
        .game_request(&episode, &PublicId::new("action-0").expect("固定请求"))
        .expect("读取账")
        .expect("必须留痕");
    assert_eq!(entry.status, GameLedgerStatus::Applied);
    assert_eq!(entry.topology_epoch, 1);
    assert_eq!(store.game_ledger_count().unwrap(), 4);
    assert!(
        store
            .unsettled_game_requests(&episode, 10)
            .unwrap()
            .is_empty(),
        "没有未结算的请求"
    );
}
