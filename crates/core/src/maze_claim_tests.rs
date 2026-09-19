//! 核对器**真的会说"不"**吗。
//!
//! 这个文件不是形式主义。一个"押了等于没押"的核对（恒真）跑一局也是绿色的，
//! 而它给的是**假的安全感**：地图会镜像、位置会漂，而每一步都"看着对"。
//! 所以这里拿**手工造的视图**直接测那个纯函数——只有能用手造的反例把它推翻，
//! 才证得了它在看世界。

use super::*;
// 这几个类型 `maze.rs` 里是用全路径写的（它只需要其中三个），这里补上名字。
use soca_contracts::{Carrying, MazeCellState, MazeColor, MazeMode};

/// 造一张 7×7 视图。`#`=墙，`.`=空地，`*`=目标，其它字符=看不见。
/// agent 自己那一格由投影负责，这里照投影的做法填上。
fn view(rows: [&str; 7], direction: u8) -> MazeView {
    let make = |ch: char| MazeCell {
        object: match ch {
            '#' => MazeObject::Wall,
            '.' => MazeObject::Empty,
            '*' => MazeObject::Goal,
            _ => MazeObject::Unseen,
        },
        color: MazeColor::Red,
        state: MazeCellState::None,
    };
    let mut cells: Vec<Vec<MazeCell>> = rows
        .iter()
        .map(|row| row.chars().map(make).collect())
        .collect();
    cells[AGENT_ROW as usize][AGENT_COLUMN as usize] = MazeCell {
        object: MazeObject::Agent,
        color: MazeColor::Red,
        state: MazeCellState::None,
    };
    MazeView {
        mode: MazeMode::Symbolic,
        mission: "测试".to_string(),
        direction,
        view: cells,
        carrying: Carrying::None,
    }
}

/// 一条横向走廊：第 3 行是空地，agent 在最右边。
fn corridor() -> MazeView {
    view(
        [
            "#######", "#######", "#######", "......#", "#######", "#######", "#######",
        ],
        3,
    )
}

#[test]
fn the_forward_claim_holds_when_the_world_really_moved() {
    // 走一步之后，走廊里的内容整体前移一格（列号 +1）：原来第 c 列的在第 c+1 列。
    let before = corridor();
    let after = view(
        [
            "#######", "#######", "#######", "......#", "#######", "#######", "#######",
        ],
        3,
    );
    assert!(claim_holds(&Claim::Forward { front: None }, &before, &after));
}

#[test]
fn the_forward_claim_is_refuted_when_the_world_did_not_shift() {
    // 同样走一步，但世界**没有**按"整体前移一格"响应——墙长在了空地上。
    // 这就是镜像或轴读反之后的样子：每一步都"看着对"，而位移关系不成立。
    let before = corridor();
    let mirrored = view(
        [
            "#######", "#######", "#######", "......#", "#######", "#######", "#######",
        ],
        3,
    );
    let mut broken = mirrored.clone();
    broken.view[3][2] = MazeCell {
        object: MazeObject::Wall,
        color: MazeColor::Red,
        state: MazeCellState::None,
    };
    assert!(
        !claim_holds(&Claim::Forward { front: None }, &before, &broken),
        "押的注错了就必须被推翻——推翻不了，这个核对就是摆设"
    );
}

#[test]
fn a_view_with_nothing_comparable_does_not_count_as_holding() {
    // 一格都比不上（全是 `unseen`）时**不算成立**。否则视野空着的时候这条注恒真，
    // 而那正是"假装在检查"最省事的一种写法。
    let blind = view(
        [
            "???????", "???????", "???????", "???????", "???????", "???????", "???????",
        ],
        3,
    );
    assert!(!claim_holds(&Claim::Forward { front: None }, &blind, &blind));
}

/// 一扇门，状态给定。
fn door(state: MazeCellState) -> MazeCell {
    MazeCell {
        object: MazeObject::Door,
        color: MazeColor::Red,
        state,
    }
}

#[test]
fn a_locked_door_without_a_key_is_predicted_to_stay_locked() {
    // 门规则里最容易被想当然的一条：锁着的门而手里没有钥匙时，按下去**什么也不该发生**。
    // 把这一条押成"会开"，会把每一次正确的"没反应"都记成世界模型出错——
    // 于是真正出错的那一天，没有人能从 noise 里认出来。
    assert_eq!(
        expected_door_state(door(MazeCellState::Locked), Carrying::Key),
        "open"
    );
    assert_eq!(
        expected_door_state(door(MazeCellState::Locked), Carrying::None),
        "locked"
    );
    // 而 `toggle` 是**开关**：开着的按下去应当变关。
    assert_eq!(
        expected_door_state(door(MazeCellState::Open), Carrying::None),
        "closed"
    );
}

#[test]
fn the_door_claim_is_refuted_when_the_door_did_not_do_what_the_rules_say() {
    let mut before = corridor();
    before.view[3][5] = door(MazeCellState::Closed);
    let mut after = before.clone();
    after.view[3][5] = door(MazeCellState::Open);

    let claim = Claim::FrontBecomes {
        at: (0, 0),
        to: "open".to_string(),
    };
    assert!(claim_holds(&claim, &before, &after));
    // 门没动（它本该开）——押的注必须被推翻。
    assert!(!claim_holds(&claim, &before, &before));
}

#[test]
fn a_toggle_that_hits_nothing_is_refuted() {
    // 这一条是**真局逼出来的**：门钥匙那一局里有两次 `toggle` 押着"状态会变成 none"，
    // 且都"押中"了——因为它对着的不是门，而空地按下去本来就不会变。
    // 那两步是白走的，而先前的注把它记成了成功：**恒真**从后门溜了回来。
    //
    // 所以前提写进注里：我面前是**门**。
    let nothing = corridor();
    assert!(
        !claim_holds(
            &Claim::FrontBecomes {
                at: (0, 0),
                to: "none".to_string()
            },
            &nothing,
            &nothing
        ),
        "对着空地按 toggle 不该算成功——那一步什么也没做"
    );
}

#[test]
fn the_pose_claim_is_refuted_when_the_cell_in_front_is_not_what_the_map_said() {
    // 地图说"我面前是空地"，而眼前是墙——**位置漂了**。
    // 这是位移那条注抓不到的东西：视图整体前移一格它一样对得上。
    let claim = Claim::Forward {
        front: Some(("empty".to_string(), "red".to_string(), "none".to_string())),
    };
    let matches = corridor();
    let mut drifted = corridor();
    drifted.view[3][5] = MazeCell {
        object: MazeObject::Wall,
        color: MazeColor::Red,
        state: MazeCellState::None,
    };
    assert!(pose_agrees(&claim, &matches));
    assert!(!pose_agrees(&claim, &drifted));
}

#[test]
fn the_turn_claim_is_about_the_direction_that_actually_came_back() {
    let left = view(
        [
            "#######", "#######", "#######", "......#", "#######", "#######", "#######",
        ],
        2,
    );
    // 从朝 3 转左 → 朝 2。
    assert!(claim_holds(&Claim::Turn { to: 2 }, &corridor(), &left));
    // 而它一旦回来还是 3，押的注就被推翻了。
    assert!(!claim_holds(&Claim::Turn { to: 2 }, &corridor(), &corridor()));
}
