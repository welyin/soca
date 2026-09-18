//! 人工批准的边界规则（§12.1、§12.2、§14）。
//!
//! 本文件盯的是一件事：**"批准过一次"不能被扩成"以后都行"。**
//!
//! 全部判据落在 [`Approval::covers`] 上，而它之所以值得单独测，是因为它的失败模式不是崩溃，
//! 而是"安静地放行"。一个把 `object_scope` 漏掉的实现，在正常路径上表现完全正确——只有
//! 在有人拿 A 的批准去做 B 的时候才看得出来，而那正是它存在的唯一理由。

use serde_json::json;
use soca_contracts::*;

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn subject() -> SubjectId {
    SubjectId::new("user:alice").expect("固定主体")
}

fn intent(action: &str, path: &str, content: &str, level: ActionLevel) -> ActionIntent {
    ActionIntent::new(
        ActionId::new(action).expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new(format!("file:{path}")).expect("固定范围"),
        json!({ "path": path, "content": content }),
        Vec::new(),
        PredictionRef::new(format!("prediction:{action}")).expect("固定预测"),
        level,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 10,
        },
        UnitId::new("unit:leaf:pending-action").expect("固定单元"),
    )
    .expect("合法意图")
}

fn approval(level: ActionLevel, channel: UserChannel, max_uses: u8) -> Approval {
    Approval::new(
        ApprovalId::new("approval:1").expect("固定审批"),
        subject(),
        level,
        channel,
        at(0),
        None,
        max_uses,
    )
    .expect("合法批准")
}

// ---------------------------------------------------------------------------
// 绑定
// ---------------------------------------------------------------------------

#[test]
fn an_approval_bound_to_one_action_does_not_cover_another() {
    // 这是全部设计的落点。批准绑定了参数摘要，改一个字节就不再被覆盖——
    // 于是"批准过一次"不能被扩成"以后都行"。
    let approved = intent("action:1", "C:\\out\\summary.md", "批准过的内容", ActionLevel::A2);
    let other = intent("action:2", "C:\\out\\summary.md", "没批准的内容", ActionLevel::A2);

    let bound = approval(ActionLevel::A2, UserChannel::ApprovalUi, 1).for_intent(&approved);
    assert!(bound.covers(&approved, at(1)).is_ok(), "同一份参数应当被覆盖");
    assert_eq!(
        bound.covers(&other, at(1)),
        Err(ContractError::ApprovalDoesNotCover {
            approval_id: "approval:1".to_string(),
            field: "parameters_digest",
        }),
        "只差一个字的内容也不该被覆盖"
    );
}

#[test]
fn an_approval_for_one_object_does_not_cover_another_object() {
    let approved = intent("action:1", "C:\\out\\summary.md", "内容", ActionLevel::A2);
    let elsewhere = intent("action:2", "C:\\out\\other.md", "内容", ActionLevel::A2);

    let bound = approval(ActionLevel::A2, UserChannel::ApprovalUi, 1).for_intent(&approved);
    assert_eq!(
        bound.covers(&elsewhere, at(1)),
        Err(ContractError::ApprovalDoesNotCover {
            approval_id: "approval:1".to_string(),
            field: "object_scope",
        }),
        "换一个目录就是换了一个对象，哪怕内容一字不差"
    );
}

#[test]
fn an_approval_for_another_tool_does_not_cover_this_one() {
    let target = intent("action:1", "C:\\out\\summary.md", "内容", ActionLevel::A2);
    let bound_to_something_else = approval(ActionLevel::A2, UserChannel::ApprovalUi, 1)
        .for_tool(ToolId::new("fs.rename").expect("另一个工具"));

    assert_eq!(
        bound_to_something_else.covers(&target, at(1)),
        Err(ContractError::ApprovalDoesNotCover {
            approval_id: "approval:1".to_string(),
            field: "tool_id",
        })
    );
}

#[test]
fn a_broad_approval_covers_actions_within_its_level() {
    // §12.1 对 A2 允许"每任务明确批准"，也就是不逐个绑定到参数。这条测试钉住的是：
    // 不绑定 ≠ 没有边界。等级上限仍然生效，而这是那种批准唯一剩下的边界。
    let broad = approval(ActionLevel::A2, UserChannel::ApprovalUi, 3);
    assert!(broad.tool_id.is_none() && broad.object_scope.is_none());
    assert!(
        broad
            .covers(
                &intent("action:1", "C:\\out\\a.md", "甲", ActionLevel::A2),
                at(1)
            )
            .is_ok()
    );
    assert!(
        broad
            .covers(
                &intent("action:2", "C:\\out\\b.md", "乙", ActionLevel::A1),
                at(1)
            )
            .is_ok()
    );
}

#[test]
fn an_approval_for_a_lower_level_does_not_cover_a_higher_action() {
    let a3 = intent("action:3", "C:\\out\\summary.md", "对外发送", ActionLevel::A3);
    let a2_only = approval(ActionLevel::A2, UserChannel::ApprovalUi, 1);

    assert_eq!(
        a2_only.covers(&a3, at(1)),
        Err(ContractError::ApprovalDoesNotCover {
            approval_id: "approval:1".to_string(),
            field: "max_action_level",
        })
    );
}

// ---------------------------------------------------------------------------
// 时间与次数
// ---------------------------------------------------------------------------

#[test]
fn an_expired_approval_covers_nothing() {
    // §12.2 末段："Broker故障、策略不可读、审计写失败、磁盘满或**审批过期**时，
    // 默认拒绝新副作用。"
    let target = intent("action:1", "C:\\out\\summary.md", "内容", ActionLevel::A2);
    let expiring = Approval::new(
        ApprovalId::new("approval:2").expect("固定审批"),
        subject(),
        ActionLevel::A2,
        UserChannel::ApprovalUi,
        at(0),
        Some(at(10)),
        1,
    )
    .expect("合法批准");

    assert!(expiring.covers(&target, at(9)).is_ok(), "到期前应当有效");
    assert_eq!(
        expiring.covers(&target, at(10)),
        Err(ContractError::ApprovalExpired {
            approval_id: "approval:2".to_string(),
            expires_at: at(10).to_string(),
        }),
        "到期的那一刻起就不覆盖"
    );
}

#[test]
fn consuming_reduces_the_remaining_count_and_stops_at_zero() {
    // §12.1 对 A3 要的是"每动作人工审批"。那意味着次数是会耗尽的，
    // 而耗尽之后必须**拒绝**，不是"继续放行但记一笔"。
    let mut single = approval(ActionLevel::A3, UserChannel::ApprovalUi, 2);
    assert_eq!(single.remaining(), 2);

    single.consume().expect("第一次");
    assert_eq!(single.remaining(), 1);
    single.consume().expect("第二次");
    assert_eq!(single.remaining(), 0);
    assert_eq!(
        single.consume(),
        Err(ContractError::ApprovalExhausted {
            approval_id: "approval:1".to_string(),
            max_uses: 2,
        })
    );
}

#[test]
fn an_exhausted_approval_covers_nothing() {
    let target = intent("action:1", "C:\\out\\summary.md", "内容", ActionLevel::A2);
    let mut one_shot = approval(ActionLevel::A2, UserChannel::ApprovalUi, 1);
    one_shot.consume().expect("用掉");

    assert_eq!(
        one_shot.covers(&target, at(1)),
        Err(ContractError::ApprovalExhausted {
            approval_id: "approval:1".to_string(),
            max_uses: 1,
        })
    );
}

// ---------------------------------------------------------------------------
// 通道
// ---------------------------------------------------------------------------

#[test]
fn a3_does_not_accept_a_voice_approval() {
    // §14："不以可能误识别的语音自动批准高风险操作。"
    //
    // A3 是"每动作人工审批"那一档，且没有别的兜底——A2 至少还有预览与目标版本核对。
    // 把这条只写在文档里是不够的：语音批准的表现和界面批准完全一样，只有这条判定能区分。
    let a3 = intent("action:3", "C:\\out\\summary.md", "对外发送", ActionLevel::A3);
    let by_voice = approval(ActionLevel::A3, UserChannel::PushToTalk, 1).for_intent(&a3);

    match by_voice.covers(&a3, at(1)) {
        Err(ContractError::ApprovalDoesNotCover { field, .. }) => {
            assert!(field.contains("A3"), "理由要说明是 A3 不收语音批准：{field}");
        }
        other => panic!("语音批准必须不覆盖 A3，实际：{other:?}"),
    }

    let by_ui = approval(ActionLevel::A3, UserChannel::ApprovalUi, 1).for_intent(&a3);
    assert!(by_ui.covers(&a3, at(1)).is_ok(), "同一份动作换个通道就该通过");
}

#[test]
fn a2_still_accepts_a_voice_approval() {
    // 把 §14 那条规则写成"A2 也不收语音"，会是一次**过度**收紧：它把 A2 的放行要求里
    // "预览、目标版本核对、备份/撤销"这几条确定性兜底一起否掉了，而这些正是 A2 与 A3
    // 的分界。边界写宽和写窄都是错，这一条防的是写窄。
    let a2 = intent("action:2", "C:\\out\\summary.md", "内容", ActionLevel::A2);
    let by_voice = approval(ActionLevel::A2, UserChannel::PushToTalk, 1).for_intent(&a2);
    assert!(by_voice.covers(&a2, at(1)).is_ok());
}

// ---------------------------------------------------------------------------
// 构造
// ---------------------------------------------------------------------------

#[test]
fn an_approval_that_permits_nothing_or_is_born_expired_is_rejected() {
    assert_eq!(
        Approval::new(
            ApprovalId::new("approval:9").expect("固定审批"),
            subject(),
            ActionLevel::A2,
            UserChannel::ApprovalUi,
            at(10),
            None,
            0,
        ),
        Err(ContractError::ApprovalInvalid {
            reason: "max_uses 必须至少为 1"
        }),
        "可用次数为 0 等于批准了什么也不许做"
    );

    assert_eq!(
        Approval::new(
            ApprovalId::new("approval:9").expect("固定审批"),
            subject(),
            ActionLevel::A2,
            UserChannel::ApprovalUi,
            at(10),
            Some(at(10)),
            1,
        ),
        Err(ContractError::ApprovalInvalid {
            reason: "expires_at 必须晚于 granted_at"
        }),
        "生下来就已经过期"
    );
}
