//! 游戏公开协议回归测试（工单 ENG-01）。
//!
//! 向量放在 `tests/vectors/game/vectors_v1.json`，刻意不写成 Rust 内联字符串：同一个文件
//! 应当能被 Rust、TypeScript 与 JSON Schema 校验器共同消费，否则三套实现迟早各自漂移。
//!
//! 正向向量除了"能解析、能通过语义校验"，还必须**逐字段回到同一份 JSON**。只测"能解析"
//! 会漏掉枚举表示写错这类问题——例如把 `0` 写成 `"0"`，或者把 `set_flag` 序列化成
//! `toggle`。

use std::path::{Path, PathBuf};

use serde_json::Value;
use soca_contracts::*;

fn vectors_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/game/vectors_v1.json")
}

fn vectors() -> Value {
    let text = std::fs::read_to_string(vectors_path()).expect("向量文件必须存在");
    serde_json::from_str(&text).expect("向量文件必须是合法 JSON")
}

/// 按 `type` 派发到对应的严格类型，返回归一化后的 JSON 或错误信息。
fn decode(message: &Value) -> Result<Value, String> {
    let text = message.to_string();
    match message.get("type").and_then(Value::as_str) {
        Some("observation") => {
            let parsed: GameObservation =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            parsed
                .validate_semantics()
                .map_err(|error| error.to_string())?;
            serde_json::to_value(&parsed).map_err(|error| error.to_string())
        }
        Some("action_request") => {
            let parsed: ActionRequest =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            parsed
                .validate_semantics()
                .map_err(|error| error.to_string())?;
            serde_json::to_value(&parsed).map_err(|error| error.to_string())
        }
        Some("action_receipt") => {
            let parsed: GameActionReceipt =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            parsed
                .validate_semantics()
                .map_err(|error| error.to_string())?;
            serde_json::to_value(&parsed).map_err(|error| error.to_string())
        }
        other => Err(format!("未知的公开消息类型 {other:?}")),
    }
}

#[test]
fn positive_vectors_parse_validate_and_round_trip() {
    let file = vectors();
    let positives = file["positive"]
        .as_array()
        .expect("positive 必须是数组");
    assert!(positives.len() >= 10, "正向向量太少");

    for vector in positives {
        let name = vector["name"].as_str().expect("向量必须有名字");
        let message = &vector["message"];
        let normalized = decode(message)
            .unwrap_or_else(|error| panic!("正向向量 {name} 不应失败：{error}"));
        assert_eq!(
            &normalized, message,
            "正向向量 {name} 必须逐字段回到同一份 JSON；枚举表示或字段名已经漂移"
        );
    }
}

#[test]
fn negative_vectors_fail_for_the_documented_reason() {
    let file = vectors();
    let negatives = file["negative"]
        .as_array()
        .expect("negative 必须是数组");
    assert!(negatives.len() >= 15, "负向向量太少");

    for vector in negatives {
        let name = vector["name"].as_str().expect("向量必须有名字");
        let reason = vector["reason"].as_str().expect("向量必须写明拒绝理由");
        let expected = vector["expect_error_contains"]
            .as_str()
            .expect("向量必须写明期望的错误片段");
        let message = &vector["message"];

        let error = decode(message)
            .err()
            .unwrap_or_else(|| panic!("负向向量 {name}（{reason}）必须被拒绝，却通过了"));

        assert!(
            error.contains(expected),
            "负向向量 {name} 的拒绝原因不符：期望包含 {expected:?}，实际为 {error:?}"
        );
    }
}

#[test]
fn hidden_world_fields_have_no_representation_in_the_public_protocol() {
    // §11.1 / §13：这些字段在公开协议里没有对应的类型，因此不可能被序列化。
    let file = vectors();
    let forbidden = [
        "info",
        "seed",
        "rng_state",
        "mine_positions",
        "safe_cells",
        "full_map",
        "solution",
        "unwrapped",
        "grid",
    ];
    for message in file["positive"]
        .as_array()
        .expect("positive 必须是数组")
        .iter()
        .chain(file["negative"].as_array().expect("negative 必须是数组"))
    {
        let text = message["message"].to_string();
        for field in forbidden {
            // 负向向量里正是要出现这些夹带字段，所以只检查正向向量。
            if message["expect_error_contains"].is_null() {
                assert!(
                    !text.contains(&format!("\"{field}\"")),
                    "正向向量 {} 里出现了隐藏字段 {field}",
                    message["name"]
                );
            }
        }
    }
}

#[test]
fn the_public_action_set_excludes_debug_channels() {
    // §10.1 / §10.2：这些通道被禁用，因此协议层不提供构造路径。
    let file = vectors();
    let mut seen = std::collections::BTreeSet::new();
    for vector in file["positive"].as_array().expect("positive 必须是数组") {
        let message = &vector["message"];
        if message.get("type").and_then(Value::as_str) != Some("action_request") {
            continue;
        }
        let action: ActionRequest = serde_json::from_value(message.clone()).expect("合法动作请求");
        seen.insert(action.action.op_name());
    }
    assert!(seen.contains("turn_left"));
    assert!(seen.contains("reveal"));
    assert!(seen.contains("set_flag"));
    for forbidden in ["done", "drop", "undo", "solve", "reset"] {
        assert!(
            !seen.contains(forbidden),
            "{forbidden} 不得成为公开动作"
        );
    }
}

#[test]
fn maze_and_mines_actions_belong_to_disjoint_domains() {
    let maze = GameAction::Move(MoveAction {
        op: MoveOp::Forward,
    });
    let reveal = GameAction::Targeted(TargetedAction {
        op: RevealOp::Reveal,
        row: 0,
        column: 0,
    });
    let flag = GameAction::Flag(FlagAction {
        op: SetFlagOp::SetFlag,
        row: 0,
        column: 0,
        flagged: true,
    });

    assert_eq!(maze.domain(), ActionDomain::Maze);
    assert_eq!(reveal.domain(), ActionDomain::Mines);
    assert_eq!(flag.domain(), ActionDomain::Mines);

    assert!(GameKind::Maze.accepts(maze));
    assert!(!GameKind::Maze.accepts(reveal));
    assert!(GameKind::Minesweeper.accepts(flag));
    assert!(!GameKind::Minesweeper.accepts(maze));
}

#[test]
fn set_flag_carries_an_intent_not_a_toggle() {
    // §11.2：set_flag 按意图幂等设置，重试不能变成引擎的二次 toggle。
    let set = GameAction::Flag(FlagAction {
        op: SetFlagOp::SetFlag,
        row: 1,
        column: 2,
        flagged: true,
    });
    let clear = GameAction::Flag(FlagAction {
        op: SetFlagOp::SetFlag,
        row: 1,
        column: 2,
        flagged: false,
    });
    assert_ne!(set, clear);

    let json = serde_json::to_value(set).expect("可序列化");
    assert_eq!(json["flagged"], Value::Bool(true));
    assert_eq!(json["op"], Value::String("set_flag".to_string()));
}

#[test]
fn mines_flags_are_not_a_win_condition() {
    // §10.2：全安全格揭开即胜；标旗正确不是胜利的替代标准。
    let view: MinesView = serde_json::from_value(serde_json::json!({
        "mode": "symbolic_mines",
        "width": 3,
        "height": 3,
        "total_mines": 1,
        "board": [[0, 1, "flagged"], [0, 1, "flagged"], [0, 1, "flagged"]]
    }))
    .expect("合法棋盘");

    assert_eq!(view.flagged_count(), 3);
    assert_eq!(view.revealed_count(), 6, "旗标不算揭示");
}

#[test]
fn vectors_file_is_self_consistent() {
    let file = vectors();
    assert_eq!(file["protocol_version"], Value::from(GAME_PROTOCOL_VERSION));
    for vector in file["positive"].as_array().expect("positive 必须是数组") {
        assert!(vector["expect_error_contains"].is_null(), "正向向量不该有期望错误");
        assert!(!vector["name"].as_str().expect("名字").is_empty());
    }
    for vector in file["negative"].as_array().expect("negative 必须是数组") {
        assert!(
            vector["expect_error_contains"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "负向向量必须写明期望的错误片段"
        );
        assert!(
            vector["reason"].as_str().is_some_and(|text| !text.is_empty()),
            "负向向量必须写明拒绝理由"
        );
    }
}
