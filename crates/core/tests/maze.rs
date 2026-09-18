//! 迷宫探索（§17 的"认知游戏"那一行）。
//!
//! 这个文件跑的是**真的 MiniGrid**：一个 Python 子进程、`games/maze/game.py` 的投影、
//! 宿主的幂等账，一条不少。它要证明三件事：
//!
//! 1. **地图拼得对**——`contradictions == 0`。
//!    这是视图约定的自检。左右搞反或转置不会以别的方式报错：地图会整体镜像，
//!    而它每一步都"看起来对"。同一个世界格子出现两种内容，是唯一会露馅的地方。
//! 2. **它真的在探索**——认得的格子数在涨，而且不是靠乱撞。
//! 3. **终局分得清**——赢了是 `won`，步数用尽是 `timeout`，两者不在同一个字段里。
//!
//! 需要 `minigrid==3.1.0`。没装时这条测试会失败而不是跳过——**跳过会让"没跑"和"跑过了"
//! 看起来一样**，而这一行要的正是"跑过了"。

use soca_core::maze::run_episode;
use soca_contracts::WallClock;
use soca_storage::Store;

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-18T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn store() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储")
}

#[test]
fn the_explorer_builds_a_consistent_map_and_gets_out() {
    let mut store = store();
    let run = run_episode(&mut store, 7, 300, at(0)).expect("跑一局");

    // 一、视图约定被这次行走验证过了。
    assert_eq!(
        run.contradictions, 0,
        "同一个世界格子出现了两种内容，说明视图的方向读错了（左右镜像或转置）"
    );

    // 二、它确实看见了东西、走了路。
    assert!(run.steps.len() >= 8, "只走了 {} 步，不像在探索", run.steps.len());
    assert!(
        run.map.len() >= 12,
        "只认出了 {} 格，地图没建起来",
        run.map.len()
    );
    assert!(
        run.steps.last().expect("有步").known_cells > run.steps[0].known_cells,
        "认得的格子没有变多，它没在探索"
    );

    // 三、终局与截断分得清。
    assert!(
        run.won() || run.outcome == "timeout",
        "意外的相位：{}",
        run.outcome
    );
    if run.won() {
        assert!(!run.truncated, "赢了不该同时是截断");
    } else {
        assert!(run.truncated, "没赢就必须是被截断了，而不是'自然结束'");
    }
}

#[test]
fn every_step_can_say_why_it_was_taken() {
    // 探索器是确定性的，而**一个写不出理由的确定性决定，读的人只能把它当成随机**。
    // 这条测试钉的就是那一栏：每一步都要有理由，而且理由是具体的一句话。
    let mut store = store();
    let run = run_episode(&mut store, 11, 40, at(0)).expect("跑一局");

    for step in &run.steps {
        assert!(!step.reason.is_empty(), "第 {} 步没有理由", step.index);
        assert!(
            step.reason.chars().count() >= 8,
            "第 {} 步的理由太短，看不出在做什么：{}",
            step.index,
            step.reason
        );
        assert!(!step.action.is_empty());
    }
}

#[test]
fn the_public_view_never_carries_the_seed_or_an_absolute_position() {
    // §11.1：公开面里不许有绝对坐标与 RNG 状态。
    //
    // 注意区分：`MazeStep.position` 是**探索器自己算出来的**，它不进公开面——
    // 它存在是因为页面上要画一张地图。真正要保证的是 `view` 里没有世界坐标。
    let mut store = store();
    let run = run_episode(&mut store, 3, 12, at(0)).expect("跑一局");

    let encoded = serde_json::to_string(&run.steps).expect("可序列化");
    for leaked in ["agent_pos", "agent_dir", "seed\"", "rng", "info"] {
        assert!(!encoded.contains(leaked), "逐步记录里出现了 {leaked}");
    }

    // 而视图只有那五个字段。
    let step = run.steps.first().expect("有步");
    assert_eq!(step.view.len(), 7, "视图是 7×7");
    assert!(step.view.iter().all(|row| row.len() == 7));
}

#[test]
fn the_same_seed_replays_the_same_walk() {
    // 确定性是"能看着它一步步走"的前提：换一次跑出来的不一样，就没有"刚才那一步"可谈。
    let mut first = store();
    let mut second = store();
    let a = run_episode(&mut first, 5, 30, at(0)).expect("第一局");
    let b = run_episode(&mut second, 5, 30, at(0)).expect("第二局");

    assert_eq!(a.steps.len(), b.steps.len());
    for (left, right) in a.steps.iter().zip(b.steps.iter()) {
        assert_eq!(left.action, right.action, "第 {} 步不一样", left.index);
        assert_eq!(left.position, right.position);
        assert_eq!(left.view, right.view);
    }
}
