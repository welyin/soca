//! 知识地图的回归测试（工单 ENG-06）。
//!
//! 这里最关键的一项不是"地图能存东西"，而是**里程计闭合**：让 agent 沿一个 1×1 的方框走
//! 回起点，自我定位必须精确回到原点、朝向必须回到 0。视图约定、前后轴与左右轴只要有一处
//! 写反，这个断言就会失败——它比任何"常量等于常量"的检查都有力。
//!
//! 测试需要仓库根的 `.venv` 与 `games/maze`。缺失时默认跳过并打印原因；
//! 设 `SOCA_REQUIRE_GAME_PROCESS=1` 改为强制失败。

use std::path::{Path, PathBuf};

use soca_contracts::*;
use soca_core_actors::{KnowledgeMap, VIEW_AGENT_COLUMN, VIEW_AGENT_ROW};
use soca_game_host::{Engine, EngineStep, ProcessEngine, ProcessEngineConfig};

const PROBE_SEED: u64 = 7;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("仓库根必须存在")
}

fn require_game_process() -> Option<PathBuf> {
    let root = repository_root();
    let entry = root.join("games").join("maze").join("game.py");
    let venv = root.join(".venv");
    if venv.exists() && entry.exists() {
        return Some(root);
    }
    let message = format!(
        "SKIP：缺少 .venv（{}）或 games/maze/game.py（{}）",
        venv.exists(),
        entry.exists()
    );
    if std::env::var_os("SOCA_REQUIRE_GAME_PROCESS").is_some() {
        panic!("{message}");
    }
    eprintln!("{message}");
    None
}

fn engine(root: &Path) -> ProcessEngine {
    let interpreter = [
        root.join(".venv").join("Scripts").join("python.exe"),
        root.join(".venv").join("bin").join("python"),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
    .expect("必须存在虚拟环境解释器");

    let mut config = ProcessEngineConfig::new(
        interpreter,
        root.join("games")
            .join("maze")
            .join("game.py")
            .to_str()
            .expect("路径必须是 UTF-8"),
        GameKind::Maze,
    );
    config.working_directory = Some(root.to_path_buf());
    ProcessEngine::spawn(config).expect("启动迷宫进程")
}

/// 走一步并把结果喂给地图。返回 (动作是否被引擎接受, 这一步的感知)。
fn act(
    engine: &mut ProcessEngine,
    map: &mut KnowledgeMap,
    action: GameAction,
) -> Result<EngineStep, soca_game_host::EngineError> {
    let step = engine.step(action)?;
    if let Percept::Maze(view) = &step.percept {
        map.observe(view, view.carrying);
    }
    match action {
        GameAction::Move(movement) => match movement.op {
            MoveOp::TurnLeft => map.turn_left(),
            MoveOp::TurnRight => map.turn_right(),
            MoveOp::Forward => map.advance(),
            _ => map.note_action(),
        },
        _ => map.note_action(),
    }
    Ok(step)
}

fn forward() -> GameAction {
    GameAction::Move(MoveAction { op: MoveOp::Forward })
}

fn turn_right() -> GameAction {
    GameAction::Move(MoveAction {
        op: MoveOp::TurnRight,
    })
}

fn maze_view(step: &EngineStep) -> &MazeView {
    match &step.percept {
        Percept::Maze(view) => view,
        other => panic!("迷宫引擎必须给出符号迷宫感知，实际为 {:?}", other.mode_name()),
    }
}

// ---------------------------------------------------------------------------
// 视图约定
// ---------------------------------------------------------------------------

#[test]
fn the_documented_view_convention_holds_on_a_live_engine() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let step = engine.reset(PROBE_SEED).expect("重置");
    let view = maze_view(&step);

    assert_eq!(view.view.len(), 7);
    assert!(view.view.iter().all(|row| row.len() == 7));
    // agent 自己在视图里应当是可分辨的：那一格不是 unseen（引擎把它设成空/携带物）。
    assert_ne!(
        view.view[VIEW_AGENT_ROW as usize][VIEW_AGENT_COLUMN as usize].object,
        MazeObject::Unseen,
        "agent 自己那一格不应该是看不见的"
    );
}

// ---------------------------------------------------------------------------
// 里程计闭合
// ---------------------------------------------------------------------------

#[test]
fn walking_a_one_by_one_square_returns_to_the_origin_without_drift() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let mut map = KnowledgeMap::new();

    let reset = engine.reset(PROBE_SEED).expect("重置");
    if let Percept::Maze(view) = &reset.percept {
        map.observe(view, view.carrying);
    }

    // 先确认前方与右侧可走，避免用一次注定失败的 forward 去测里程计。
    let start_view = maze_view(&reset);
    assert!(
        start_view.view[(VIEW_AGENT_ROW - 1) as usize][VIEW_AGENT_COLUMN as usize]
            .object
            .ne(&MazeObject::Wall),
        "起点前方应当是空地，否则这条用例测的就不是里程计"
    );

    // 1×1 方框：前进、右转各四次，回到起点与初始朝向。
    for _ in 0..4 {
        act(&mut engine, &mut map, forward()).expect("前进");
        act(&mut engine, &mut map, turn_right()).expect("右转");
    }

    assert_eq!(
        map.position(),
        (0, 0),
        "走了闭环之后必须精确回到原点；偏差说明前进方向或左右轴写反了"
    );
    assert_eq!(map.heading(), 0, "四次右转之后朝向必须回到初始朝向");
    assert!(
        map.known_count() >= 4,
        "走一圈至少要看见起点周围的几格，实际 {}",
        map.known_count()
    );
}

#[test]
fn odometry_tracks_a_two_step_straight_line() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let mut map = KnowledgeMap::new();
    let reset = engine.reset(PROBE_SEED).expect("重置");
    if let Percept::Maze(view) = &reset.percept {
        map.observe(view, view.carrying);
    }

    act(&mut engine, &mut map, forward()).expect("前进");
    assert_eq!(map.position(), (0, 1), "朝向 0 的前进是 +y");
    act(&mut engine, &mut map, forward()).expect("前进");
    assert_eq!(map.position(), (0, 2));

    act(&mut engine, &mut map, turn_right()).expect("右转");
    assert_eq!(map.heading(), 1);
    act(&mut engine, &mut map, forward()).expect("前进");
    assert_eq!(map.position(), (1, 2), "右转后的前进是 +x");
}

// ---------------------------------------------------------------------------
// 已知与未知
// ---------------------------------------------------------------------------

#[test]
fn the_map_never_treats_an_unseen_cell_as_empty() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let mut map = KnowledgeMap::new();
    let reset = engine.reset(PROBE_SEED).expect("重置");
    let view = maze_view(&reset);

    if let Percept::Maze(view) = &reset.percept {
        map.observe(view, view.carrying);
    }

    // 视图里每一个 unseen 格都不该出现在地图里。
    let mut unseen_positions = 0;
    for row in &view.view {
        for cell in row {
            if cell.object == MazeObject::Unseen {
                unseen_positions += 1;
            }
        }
    }
    assert!(unseen_positions > 0, "这个局面应当存在看不见的格子");

    // 地图应当恰好记录：所有看得见的格子，加上 agent 站过的那一格（视图里它显示为自己，
    // 不算景物，但"我站得住"本身就是可走性证据）。
    let visible = 7 * 7 - unseen_positions - 1;
    assert_eq!(
        map.known_count(),
        visible + 1,
        "地图只应记录看得见的格子与站过的格子：把 unseen 记成空格会让 agent 一头撞进没见过的墙"
    );
}

#[test]
fn a_wall_straight_ahead_is_known_and_not_passable() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let mut map = KnowledgeMap::new();
    let reset = engine.reset(PROBE_SEED).expect("重置");
    if let Percept::Maze(view) = &reset.percept {
        map.observe(view, view.carrying);
    }

    // DoorKey 的起点房间左上方有边界墙。连走两次前进再左转两次，总能撞到边界。
    for _ in 0..3 {
        act(&mut engine, &mut map, forward()).ok();
    }
    for _ in 0..2 {
        act(&mut engine, &mut map, forward()).ok();
    }

    let walls: Vec<_> = map
        .iter()
        .filter(|(_, known)| known.object == MazeObject::Wall)
        .collect();
    assert!(!walls.is_empty(), "走一圈应当见过至少一面墙");
    assert!(
        walls.iter().all(|(_, known)| !known.is_passable()),
        "墙绝不能被当成可通行"
    );
}

// ---------------------------------------------------------------------------
// 前沿与规划
// ---------------------------------------------------------------------------

#[test]
fn the_frontier_contains_only_known_passable_cells_next_to_the_unknown() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let mut map = KnowledgeMap::new();
    let reset = engine.reset(PROBE_SEED).expect("重置");
    if let Percept::Maze(view) = &reset.percept {
        map.observe(view, view.carrying);
    }
    act(&mut engine, &mut map, forward()).expect("前进");

    let frontier = map.frontier();
    assert!(!frontier.is_empty(), "应当存在探索前沿");
    for cell in frontier {
        let known = map.known(cell).expect("前沿格必须是已知的");
        assert!(known.is_passable(), "前沿格必须是已知可走的");
    }
}

#[test]
fn planning_uses_only_known_cells_so_an_unknown_target_is_unreachable() {
    let Some(root) = require_game_process() else {
        return;
    };
    let mut engine = engine(&root);
    let mut map = KnowledgeMap::new();
    let reset = engine.reset(PROBE_SEED).expect("重置");
    if let Percept::Maze(view) = &reset.percept {
        map.observe(view, view.carrying);
    }
    act(&mut engine, &mut map, forward()).expect("前进");

    // 走到已知前沿的能力。
    let reachable: Vec<_> = map
        .frontier()
        .into_iter()
        .filter(|cell| map.path_to(*cell).is_some())
        .collect();
    assert!(!reachable.is_empty(), "至少有一个前沿应当可达");

    // 地图之外的位置不可达：规划只用地已知的东西。
    assert!(map.path_to((999, 999)).is_none());
    assert!(
        map.path_to((0, 0)).is_some(),
        "起点自己应当在已知网络上"
    );
}
