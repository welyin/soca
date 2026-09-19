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
    assert!(claim_holds(&Claim::Forward, &before, &after));
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
        !claim_holds(&Claim::Forward, &before, &broken),
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
    assert!(!claim_holds(&Claim::Forward, &blind, &blind));
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
