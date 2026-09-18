//! 人工批准的持久化回归测试（§12.1、§12.2）。
//!
//! 本文件盯的是两件在别处看不见的事：
//!
//! 1. **消费是原子的。** 先读后写会留下一个窗口，两次签发都读到"还剩一次"，于是同一次批准
//!    被用掉两次——那会把 §12.1 的"每动作人工审批"退化成"每个动作类型一次审批"。
//! 2. **次数只以列为准。** JSON 里存一份计数、列里存另一份，必然分叉，而分叉的方向恰好危险：
//!    读的人会以为还能再用一次。重放一条旧的批准记录正是触发它的最直接方式。

use soca_contracts::{
    ActionLevel, Approval, ApprovalId, ResourceScope, Sha256Hex, SubjectId, ToolId, UserChannel,
    WallClock,
};
use soca_storage::{StorageError, Store};

fn at(offset_seconds: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset_seconds)
}

fn owner(name: &str) -> SubjectId {
    SubjectId::new(format!("user:{name}")).expect("固定主体")
}

fn approval(id: &str, for_owner: &str, level: ActionLevel, max_uses: u8) -> Approval {
    Approval::new(
        ApprovalId::new(format!("approval:{id}")).expect("固定审批"),
        owner(for_owner),
        level,
        UserChannel::ApprovalUi,
        at(0),
        None,
        max_uses,
    )
    .expect("合法批准")
}

fn approval_id(id: &str) -> ApprovalId {
    ApprovalId::new(format!("approval:{id}")).expect("固定审批")
}

fn in_memory() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储")
}

// ---------------------------------------------------------------------------
// 往返
// ---------------------------------------------------------------------------

#[test]
fn an_approval_round_trips_through_storage() {
    let mut store = in_memory();
    let bound = approval("1", "alice", ActionLevel::A2, 2)
        .for_tool(ToolId::new("fs.write").expect("固定工具"))
        .for_scope(ResourceScope::new("file:C:\\out\\summary.md").expect("固定范围"))
        .for_parameters(Sha256Hex::of_bytes(b"content"));

    assert!(store.record_approval(&bound).expect("写入"), "首次写入是新增");
    assert_eq!(store.approval(&approval_id("1")).expect("读取"), Some(bound.clone()));
    assert_eq!(
        store.approvals_for(&owner("alice")).expect("按主体列出"),
        vec![bound]
    );
}

#[test]
fn approvals_are_listed_per_subject() {
    // §4.1 L5 的隔离原则同样适用于批准："跨所有者的检索不是加个过滤条件，而是根本不提供入口。"
    let mut store = in_memory();
    store
        .record_approval(&approval("1", "alice", ActionLevel::A2, 1))
        .expect("写入");
    store
        .record_approval(&approval("2", "bob", ActionLevel::A2, 1))
        .expect("写入");

    assert_eq!(store.approvals_for(&owner("alice")).expect("列").len(), 1);
    assert_eq!(store.approvals_for(&owner("carol")).expect("列").len(), 0);
}

// ---------------------------------------------------------------------------
// 消费
// ---------------------------------------------------------------------------

#[test]
fn consuming_returns_the_new_count_and_stops_at_the_limit() {
    let mut store = in_memory();
    store
        .record_approval(&approval("1", "alice", ActionLevel::A3, 2))
        .expect("写入");

    assert_eq!(store.consume_approval(&approval_id("1")).expect("第一次"), 1);
    assert_eq!(store.consume_approval(&approval_id("1")).expect("第二次"), 2);
    assert!(
        matches!(
            store.consume_approval(&approval_id("1")),
            Err(StorageError::ApprovalNotFound { .. })
        ),
        "用尽之后必须拒绝，而不是继续放行"
    );
}

#[test]
fn consuming_an_unknown_approval_is_an_error_not_a_silent_success() {
    let mut store = in_memory();
    assert!(matches!(
        store.consume_approval(&approval_id("missing")),
        Err(StorageError::ApprovalNotFound { .. })
    ));
}

#[test]
fn recording_the_same_approval_again_does_not_reset_its_counter() {
    // 这是整个模块最要紧的一条。计数若能被"重放同一条批准记录"冲回 0，那么一个一次性
    // 批准就变成了可反复使用的东西——而 §12.1 对 A3 要的恰恰是每动作一次。
    let mut store = in_memory();
    let once = approval("1", "alice", ActionLevel::A3, 1);
    assert!(store.record_approval(&once).expect("写入"));
    store.consume_approval(&approval_id("1")).expect("用掉");

    let replayed = store.record_approval(&once).expect("重复写入");
    assert!(!replayed, "同标识同内容应当被判为已存在");
    assert_eq!(
        store.approval(&approval_id("1")).expect("读取").expect("存在").used,
        1,
        "计数不能被冲回去"
    );
    assert!(
        store.consume_approval(&approval_id("1")).is_err(),
        "重放之后它仍然是用尽的"
    );
}

#[test]
fn reusing_an_approval_id_for_a_different_binding_is_rejected() {
    // 审批标识是追溯依据的锚点。复用它会伪造一条不存在的批准历史：
    // 动作账上写着一个标识，而那个标识指向的却是另一次批准。
    let mut store = in_memory();
    store
        .record_approval(&approval("1", "alice", ActionLevel::A2, 1))
        .expect("写入");

    let different = approval("1", "alice", ActionLevel::A3, 1)
        .for_tool(ToolId::new("fs.write").expect("固定工具"));
    assert!(matches!(
        store.record_approval(&different),
        Err(StorageError::ApprovalAlreadyRecorded { .. })
    ));
}

// ---------------------------------------------------------------------------
// 可用性
// ---------------------------------------------------------------------------

#[test]
fn the_usable_count_excludes_expired_and_exhausted_approvals() {
    let mut store = in_memory();
    let expiring = Approval::new(
        ApprovalId::new("approval:exp").expect("固定审批"),
        owner("alice"),
        ActionLevel::A2,
        UserChannel::ApprovalUi,
        at(0),
        Some(at(10)),
        1,
    )
    .expect("合法批准");

    store
        .record_approval(&approval("live", "alice", ActionLevel::A2, 1))
        .expect("写入");
    store.record_approval(&expiring).expect("写入");
    store
        .record_approval(&approval("spent", "alice", ActionLevel::A2, 1))
        .expect("写入");
    store
        .consume_approval(&approval_id("spent"))
        .expect("用掉一条");

    assert_eq!(
        store.usable_approval_count(&owner("alice"), at(5)).expect("计数"),
        2,
        "这一刻：尚未过期的一条 + 未用尽的一条"
    );
    assert_eq!(
        store.usable_approval_count(&owner("alice"), at(10)).expect("计数"),
        1,
        "到点之后过期的那条不再算数"
    );
}

#[test]
fn the_counter_survives_reopening_the_store() {
    // §12.1 要求动作能追溯到一次明确的批准。如果批准本身重启就没了，动作账上留下的
    // `approval_id` 就是一个悬空锚点——追溯链恰好断在最需要它回答的那个问题上。
    let path = std::env::temp_dir().join(format!(
        "soca-approvals-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut store = Store::open(&path, at(0)).expect("首次打开");
        store
            .record_approval(&approval("1", "alice", ActionLevel::A3, 2))
            .expect("写入");
        store.consume_approval(&approval_id("1")).expect("用掉一次");
    }

    {
        let store = Store::open(&path, at(3600)).expect("再次打开");
        assert_eq!(
            store.approval(&approval_id("1")).expect("读取").expect("存在").used,
            1,
            "重启之后已用次数必须还在"
        );
        assert!(
            store.approvals_for(&owner("alice")).expect("列").len() == 1,
            "绑定也必须还在"
        );
    }

    let _ = std::fs::remove_file(&path);
}
