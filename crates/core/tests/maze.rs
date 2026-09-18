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

use soca_core::maze::{play_through_actions, run_episode, RunPath};
use soca_contracts::{
    ModelBackend, ModelBudget, ModelVersion, SubjectId, WallClock,
};
use soca_core::{ActionBroker, SimulatedOs, Subject};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-18T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn store() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储")
}

/// 一个带守望文件的主体。守望文件不是摆设：簇会围绕它提候选，于是游戏动作
/// **真的**要在 L3 里和别的东西排队。
fn subject() -> Subject {
    let mut os = SimulatedOs::new();
    os.seed(WATCHED, "资料摘要\n");
    let cluster = DesktopAndFilesCluster::new(WATCHED, Vec::new()).expect("装配能力簇");
    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(os),
        cluster,
        SubjectId::new("user:local").expect("固定主体"),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000b2").expect("固定 boot"),
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
fn the_agent_path_pays_every_toll_on_the_way() {
    // §15.2 的"同一 L3 候选和 Broker 动作循环"。
    //
    // 每一步都要留下三样东西：一条**执行许可**、一条**回执**、一次**核验判定**。
    // 三样缺一，就说明那一步没有真的走那条循环——而"走没走"正是这一条要证的。
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 200, at(0)).expect("让主体走一局");

    assert_eq!(run.path, RunPath::Agent, "走的应当是认知通路");
    assert_eq!(run.contradictions, 0, "地图仍然要自洽");
    assert!(!run.steps.is_empty());
    assert!(
        run.won() || run.outcome == "timeout",
        "意外的相位：{}",
        run.outcome
    );

    for step in &run.steps {
        assert!(
            step.permit_id.is_some(),
            "第 {} 步没有执行许可——它没走许可那道关",
            step.index
        );
        assert!(
            step.verdict.is_some(),
            "第 {} 步没有核验判定——它没走回执之后的那次核对",
            step.index
        );
        assert!(
            step.receipt.contains("Completed"),
            "第 {} 步的回执不对：{}",
            step.index,
            step.receipt
        );
    }

    // 而且它**真的排过队**。至少有一轮的候选不是它——簇自己会提文件观测一类的候选，
    // 而选择规则是"证据多者先、并列时先出现的先"。
    //
    // 这条断言如果把 `rounds_waited` 写成恒 0 会通过，所以它同时也是那个字段的自检。
    assert!(
        run.steps.iter().any(|step| step.rounds_waited > 1),
        "每一步都是一轮就轮上，说明它没有和别的候选竞争过：{:?}",
        run.steps
            .iter()
            .map(|step| step.rounds_waited)
            .collect::<Vec<_>>()
    );
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
