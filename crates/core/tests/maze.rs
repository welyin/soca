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

use std::collections::BTreeMap;

use soca_core::maze::{play_through_actions, run_episode, RunPath};
use soca_contracts::{
    CapabilityPolicyRef, ModelBackend, ModelBudget, ModelVersion, SubjectId, WallClock,
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

fn owner() -> SubjectId {
    SubjectId::new("user:local").expect("固定主体")
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
    let run = run_episode(&mut store, 7, 300, "door-key", at(0)).expect("跑一局");

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
    let run = run_episode(&mut store, 11, 40, "door-key", at(0)).expect("跑一局");

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
    let run = run_episode(&mut store, 3, 12, "door-key", at(0)).expect("跑一局");

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
    let run = play_through_actions(&mut subject, 7, 200, "door-key", at(0)).expect("让主体走一局");

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

    // **排队那几轮也看得见**，而不是只有一个数字。
    //
    // 这是"所有步骤"里原先缺的那一半：一场 24 步的探索花了 27 轮，多出来的那几轮
    // 也是一轮真实的认知循环，各扣了一次激活。只报"等了 4 轮"，读的人看不到那 4 轮
    // 里发生了什么——而"这一局每一轮在做什么"正是要看的东西。
    for step in &run.steps {
        assert_eq!(
            step.waited_on.len() as u32,
            step.rounds_waited
                .saturating_sub(if step.permit_id.is_some() { 1 } else { 0 }),
            "第 {} 步：等了 {} 轮，却只记下 {} 轮给了谁",
            step.index,
            step.rounds_waited,
            step.waited_on.len()
        );
        for waited in &step.waited_on {
            assert!(
                !waited.advanced.is_empty(),
                "第 {} 步：有一轮没说清给了谁",
                step.index
            );
        }
    }
    assert!(
        run.steps.iter().any(|step| !step.waited_on.is_empty()),
        "整局都没有一轮记录下'给了谁'——多半是它一轮都没排过队"
    );
}

#[test]
fn the_crossing_variant_is_a_plain_maze_with_no_key_or_door() {
    // 那一种传统迷宫：起点、终点、墙体——没有钥匙，没有门。
    //
    // 断言"没有"比断言"有"更要紧：**同一个探索器能跑通两关**这件事，只有在两关真的不同时
    // 才有意义。`crossing` 要是悄悄退回了 DoorKey（比如变体没接上），那么"它也能跑墙与缺口"
    // 就是一句空话，而页面上还会显示得好好的。
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 400, "crossing", at(0)).expect("走一局");
    assert_eq!(run.variant, "crossing", "返回值要说清这是哪一关");
    assert_eq!(run.contradictions, 0);
    assert!(run.won(), "意外的相位：{}", run.outcome);

    let truth = run.truth.clone().expect("真值");
    assert_eq!(truth.width, 9, "这一关是 9×9");
    let objects: Vec<&str> = truth.cells.iter().map(|cell| cell.object.as_str()).collect();
    assert!(objects.contains(&"goal"), "得有终点");
    assert!(!objects.contains(&"door"), "不该有门：{objects:?}");
    assert!(!objects.contains(&"key"), "不该有钥匙：{objects:?}");

    // 而同一 seed 在两关里是**不同的迷宫**。
    let other = play_through_actions(&mut subject, 7, 300, "door-key", at(600)).expect("另一关");
    assert_ne!(
        (run.map.len(), truth.width),
        (other.map.len(), other.truth.as_ref().map(|t| t.width).unwrap_or(0)),
        "两关长得一样，多半是变体没接上"
    );
}

#[test]
fn the_four_rooms_variant_runs() {
    // 四房间是清单里最大的那一关（19×19）。它**未必走得完**——一份目标的额度是 32 次激活，
    // 而这么大的地方很可能不够。所以这里不要求 `won`，只要求它**不崩**、地图自洽、
    // 而且如实说出自己停在哪。
    //
    // 写这条测试是因为它在页面上是**空的 500**：响应体什么都没有，看不出是哪一步出的问题。
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 400, "four-rooms", at(0)).expect("走一局");
    assert_eq!(run.variant, "four-rooms");
    assert_eq!(run.contradictions, 0);
    assert!(!run.steps.is_empty(), "一步都没走");
    let truth = run.truth.clone().expect("真值");
    assert_eq!(truth.width, 19);

    // **走不完不是失败**——额度用尽要说得出为什么，而不是崩掉。
    //
    // 这一条是页面上那个"空的 500"逼出来的：`request_game_step` 在没有可推进目标时
    // 返回的是错误，于是整局以一次内部错误结束，而真因是**这一份预算花完了**。
    // 两者在界面上必须分得开：一个是"我们给的钱不够"，一个是"它坏了"。
    assert!(
        run.won() || run.stopped.is_some(),
        "既没赢也没说清为什么停（outcome={}）",
        run.outcome
    );
}

#[test]
fn the_walls_it_learned_match_the_truth() {
    // **整条链对地面真值的一次比对。**
    //
    // 地图是靠"视图约定"拼出来的（agent 恒在 (3,6)、前方是列号变小）。那条约定读错的话
    // ——镜像或转置——地图会**整体翻转而每一步都"看起来对"**：`contradictions` 是 0
    // （自洽不等于正确），探索器照样能找到门和钥匙（它只是在一个镜像的世界里找）。
    // 唯一能戳穿它的是真值。
    //
    // 比的是**几何**：墙、目标、岩浆那几类不会因为 agent 做了什么而改变。
    // 钥匙和门被排除在外，不是因为它们不重要，而是因为**它们本来就该变**——
    // 钥匙会被拿走、门会被打开，而真值那张图是**开局**的样子。
    // 拿它去比"现在"，会把一次正确的探索判成错的。
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 300, "door-key", at(0)).expect("走一局");
    let truth = run.truth.clone().expect("真值");
    assert!(
        run.won(),
        "这一局该走完，否则地图本来就缺角：{}",
        run.outcome
    );

    let offset = (truth.start.x, truth.start.y);
    let learned: BTreeMap<(i32, i32), String> = run
        .map
        .iter()
        .map(|cell| (cell.at, cell.object.clone()))
        .collect();

    let mut compared = 0usize;
    for cell in &truth.cells {
        if !matches!(cell.object.as_str(), "wall" | "goal" | "lava") {
            continue;
        }
        let at = (cell.x - offset.0, cell.y - offset.1);
        compared += 1;
        assert_eq!(
            learned.get(&at).map(String::as_str),
            Some(cell.object.as_str()),
            "格子 {at:?}（世界 {:?}）与真值不符——视图约定多半读反了",
            (cell.x, cell.y)
        );
    }
    assert!(
        compared >= 30,
        "只比了 {compared} 格，这条测试没测到东西"
    );
}

#[test]
fn what_it_learned_becomes_memory_and_the_two_counts_converge() {
    // §15.2 那句"同一 L1 记忆"。
    //
    // 判据不是"它记住了一些东西"，而是**两个数收敛**：认得的格子数，与在册的格子记忆条数。
    // 一格一条。不等于就说明有一格没记上（那正是探索器有个私有字典的样子），
    // 或者有一条重复（那是"同一格反复被看见攒出多条"的样子）。
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 200, "door-key", at(0)).expect("走一局");

    assert!(run.memories > 0, "一步都没往记忆里记");
    let active_cells = subject
        .store()
        .recall(&owner(), None, at(0))
        .expect("召回")
        .into_iter()
        .filter(|entry| entry.claim.starts_with("迷宫格"))
        .count();
    assert_eq!(
        active_cells,
        run.map.len(),
        "认得的格子数 {} 与在册的格子记忆条数 {active_cells} 对不上——一格一条",
        run.map.len()
    );

    // 而其中**确实**有被取代过的一条：门被打开了。世界变了，记忆该跟着变，
    // 而不是同时留着"门是关的"和"门是开的"两条等着人去分辨。
    let door_revisions = subject
        .store()
        .recall(&owner(), None, at(0))
        .expect("召回")
        .into_iter()
        .filter(|entry| entry.claim.contains("door"))
        .map(|entry| entry.revision)
        .max()
        .unwrap_or(0);
    assert!(
        door_revisions >= 2,
        "门开过之后，那条记忆该是第 2 版（实际最高 {door_revisions} 版）"
    );
}

#[test]
fn revoking_the_capability_takes_back_what_was_learned_through_it() {
    // §12.1 的"撤回立即生效"，对**知识**也成立。
    //
    // 每一格都是从某一次观测里看出来的，而那次观测属于 `cap:game-step` 这个范围。
    // 撤回它，那些观测失效，于是从它们推出来的格子记忆一起失效——
    // §12.1 要的"立即生效"于是不只是"不能做新动作"，还包括"不能接着用旧知识"。
    //
    // 这条推论只有在格子**真的**是记忆、而且**真的**指回那次观测时才成立。
    // 探索器里那个进程内的字典在这里会安静地什么也不变。
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 200, "door-key", at(0)).expect("走一局");
    let before = subject
        .store()
        .recall(&owner(), None, at(0))
        .expect("召回")
        .into_iter()
        .filter(|entry| entry.claim.starts_with("迷宫格"))
        .count();
    assert_eq!(before, run.map.len());

    let capability = CapabilityPolicyRef::new("cap:game-step").expect("固定能力策略");
    let report = subject
        .revoke_capability(&capability, at(900))
        .expect("撤回");
    assert!(
        report.memories_invalidated > 0,
        "撤回能力应当连带让派生记忆失效：{report:?}"
    );

    let after = subject
        .store()
        .recall(&owner(), None, at(0))
        .expect("召回")
        .into_iter()
        .filter(|entry| entry.claim.starts_with("迷宫格"))
        .count();
    assert!(
        after < before,
        "撤回之后仍然认得 {after} 格（撤回前 {before} 格）——那些知识没有跟着失效"
    );
}

#[test]
fn running_twice_on_the_same_subject_works_the_second_time_too() {
    // 页面上那个"跑一局"按钮会被点第二次。而第二次曾经是**坏的**：
    //
    //     Err(GameAlreadyAttached) → "这一局已经接上一个游戏回合了"
    //
    // 两个原因叠在一起，而修一个不够：
    //
    // 1. 旧回合没摘，`start_game` 于是拒绝接第二个；
    // 2. 就算摘了，旧目标还挂在栈上、而它的 32 次激活**已经花光**——`next_open_goal`
    //    先轮到它，于是新一局的第一步绑在旧目标上、在额度耗尽处安静地停住。
    //
    // 第二条尤其值得钉住：它的表现是"第二局一步没走"，而单看那句话像是探索器坏了。
    let mut subject = subject();
    let first = play_through_actions(&mut subject, 7, 200, "door-key", at(0)).expect("第一局");
    assert!(!first.steps.is_empty());

    let second = play_through_actions(&mut subject, 7, 200, "door-key", at(600)).expect("第二局");
    assert!(
        !second.steps.is_empty(),
        "第二局一步没走——多半是它绑到了一个额度已尽的旧目标上"
    );
    assert_eq!(second.contradictions, 0);
    assert_eq!(
        first.steps.len(),
        second.steps.len(),
        "同一 seed 的两局应当一样长（它是确定性的）"
    );
}

#[test]
fn the_map_only_grows_and_every_cell_was_new_exactly_once() {
    // 逐步存下来的那些地图要满足两条**可算**的性质，否则"往回拖会缩回去"就只是画得好：
    //
    // 1. **它只增不减。** 看见过的格子不会被忘掉——`map` 只有插入与覆盖，没有删除。
    //    如果哪一天有人加了删除（比如"过期忘掉"），这条会红，而那是**该**红的：
    //    地图缩回去与"回到当时"是两件事，前者会让回放说谎。
    // 2. **开局那一眼 + 每一步"新认得"的加总，正好等于最终的格子数。** 每格恰好新过一次。
    //    这条把 `initial_cells`、`learned` 与 `known_cells` 三者绑在一起——只算错一处就会红，
    //    而算错的表现是页面上那个"（+N）"随步数漂移，看不出是哪里错了。
    //
    //    开局那一笔**必须**单列：它发生在任何一步之前，并进哪一步都会让那一步看起来
    //    "什么也没做却学到了 23 格"。（这条断言第一版就没有它，于是 29 ≠ 64。）
    let mut subject = subject();
    let run = play_through_actions(&mut subject, 7, 200, "door-key", at(0)).expect("走一局");

    assert!(!run.steps.is_empty());
    let mut previous = run.initial_cells;
    let mut total_learned = run.initial_cells;
    for step in &run.steps {
        assert!(
            step.known_cells >= previous,
            "第 {} 步的地图缩了：{} → {}",
            step.index,
            previous,
            step.known_cells
        );
        assert_eq!(
            step.map.len(),
            step.known_cells,
            "第 {} 步的逐步地图与计数对不上",
            step.index
        );
        total_learned = total_learned.saturating_add(step.learned);
        previous = step.known_cells;
    }
    assert_eq!(
        total_learned,
        run.map.len(),
        "每一步新认得的加总应当正好等于最终的格子数"
    );
}

#[test]
fn the_same_seed_replays_the_same_walk() {
    // 确定性是"能看着它一步步走"的前提：换一次跑出来的不一样，就没有"刚才那一步"可谈。
    let mut first = store();
    let mut second = store();
    let a = run_episode(&mut first, 5, 30, "door-key", at(0)).expect("第一局");
    let b = run_episode(&mut second, 5, 30, "door-key", at(0)).expect("第二局");

    assert_eq!(a.steps.len(), b.steps.len());
    for (left, right) in a.steps.iter().zip(b.steps.iter()) {
        assert_eq!(left.action, right.action, "第 {} 步不一样", left.index);
        assert_eq!(left.position, right.position);
        assert_eq!(left.view, right.view);
    }
}
