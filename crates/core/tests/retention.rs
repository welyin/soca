//! 保留期与删除的回归测试（§12.3）。
//!
//! 本文件盯的是这条要求里最容易被跳过的那一半：
//!
//! > 先写 tombstone 使查询立即不可见，**再异步清理**，并给用户完成状态。
//!
//! 前半句其实**不需要驱动**——[`soca_storage::Store::recall`] 自己会按 `valid_until` 过滤，
//! 所以一条超期的情景记忆在没人管它的时候也已经检索不到了。看起来"保留期在工作"，
//! 而真相是它永远停在那个状态：条目还是 `active`，不在任何清理的范围内，内容原封不动地
//! 留在库里，直到数据库被删掉为止。
//!
//! 所以下面的测试特意把两件事分开断言：**可见性**与**内容是否还在**。

use soca_contracts::{
    DataClass, EvidenceRef, MemoryEntry, MemoryId, MemoryKind, Provenance, SourceId, SubjectId,
    WallClock,
};
use soca_core::{expire_retained, forget, purge_retained, RetentionPolicy};
use soca_storage::{audit::AuditCategory, StorageError, Store};

const DAY: i64 = 86_400;

fn at(offset_seconds: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset_seconds)
}

fn owner() -> SubjectId {
    SubjectId::new("user:alice").expect("固定主体")
}

fn id(name: &str) -> MemoryId {
    MemoryId::new(format!("memory:{name}")).expect("固定标识")
}

fn entry(name: &str, kind: MemoryKind, recorded_offset: i64) -> MemoryEntry {
    MemoryEntry::new(
        id(name),
        kind,
        owner(),
        None,
        format!("命题 {name}"),
        vec![EvidenceRef::new(format!("obs:{name}")).expect("固定证据")],
        Provenance::Sensor {
            adapter: SourceId::new("device:file-watcher").expect("固定适配器"),
        },
        DataClass::Personal,
        at(recorded_offset),
    )
    .expect("合法记忆")
}

fn in_memory() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储")
}

// ---------------------------------------------------------------------------
// 保留期
// ---------------------------------------------------------------------------

#[test]
fn without_the_driver_an_expired_memory_is_invisible_but_never_cleaned() {
    // 这条测试要说清"驱动到底补了什么"。它补的**不是**可见性——那一步早就有了。
    let mut store = in_memory();
    store
        .record_memory(&entry("ep", MemoryKind::Episode, 0))
        .expect("写入");

    // 第 8 天：超过 7 天的初值。检索已经不再返回它，而这一步不需要任何人做。
    assert_eq!(
        store.recall(&owner(), None, at(8 * DAY)).expect("检索").len(),
        0,
        "超期条目本来就检索不到"
    );

    // 但内容还在，而且**永远没人会清它**：它仍然是 `active`，不在清理的范围内。
    assert_eq!(store.tombstoned_memory_count().expect("计数"), 0);
    assert_eq!(
        store.purge_tombstoned().expect("清理"),
        0,
        "没有驱动的话，清理这一步永远不会碰到它"
    );
    assert!(
        store
            .memory_including_hidden(&id("ep"))
            .expect("读")
            .is_some(),
        "内容原封不动地留在库里"
    );
}

#[test]
fn hiding_and_purging_are_two_states_not_one() {
    let mut store = in_memory();
    store
        .record_memory(&entry("ep", MemoryKind::Episode, 0))
        .expect("写入");

    let hidden = expire_retained(&mut store, at(8 * DAY)).expect("执行保留期");
    assert_eq!(hidden.tombstoned.len(), 1);
    assert_eq!(hidden.tombstoned[0].memory_id, "memory:ep");
    assert_eq!(hidden.tombstoned[0].reason, "retention_expired");
    assert_eq!(hidden.purged, 0, "这一步不做清理");
    assert_eq!(hidden.awaiting_purge, 1, "有一条在等清理");
    assert!(
        store
            .memory_including_hidden(&id("ep"))
            .expect("读")
            .is_some(),
        "隐藏之后内容还在原处——这正是「异步清理」要留出的那个窗口"
    );

    let purged = purge_retained(&mut store, &RetentionPolicy::default(), at(9 * DAY)).expect("清理");
    assert_eq!(purged.purged, 1);
    assert_eq!(purged.awaiting_purge, 0);
    assert!(
        store
            .memory_including_hidden(&id("ep"))
            .expect("读")
            .is_none(),
        "清理之后连审计入口也读不到了；留下的是审计账上的那次记录"
    );
}

#[test]
fn a_fact_does_not_expire_on_its_own() {
    // §12.3："长期偏好/语义事实，只有明确需记忆的内容才提升。" 它不设自动过期，
    // 而"不设过期"必须是真的不设——否则一条被提升的结论会在某天悄悄消失，而没人知道为什么。
    let mut store = in_memory();
    store
        .record_memory(&entry("fact", MemoryKind::Fact, 0))
        .expect("写入");

    let report = expire_retained(&mut store, at(3_650 * DAY)).expect("执行");
    assert!(report.tombstoned.is_empty(), "语义事实不该自己过期");
    assert_eq!(
        store.recall(&owner(), None, at(3_650 * DAY)).expect("检索").len(),
        1
    );
}

#[test]
fn a_summary_lives_no_longer_than_what_it_summarizes() {
    // §12.3 的保留期表里没有单列摘要，但理由是通的：它是从有保留期的内容派生的，
    // 自身不该活得比来源更久。这一条如果写反，摘要会变成一条绕开保留期的旁路。
    assert_eq!(
        MemoryKind::Summary.default_retention_days(),
        MemoryKind::Episode.default_retention_days()
    );
    assert!(MemoryKind::Summary.default_retention_days().is_some());
}

// ---------------------------------------------------------------------------
// 用户删除
// ---------------------------------------------------------------------------

#[test]
fn forgetting_hides_immediately_and_leaves_the_content_for_the_purge_step() {
    let mut store = in_memory();
    store
        .record_memory(&entry("fact", MemoryKind::Fact, 0))
        .expect("写入");

    let report = forget(&mut store, &id("fact"), "user_requested", at(1)).expect("删除");
    assert_eq!(report.tombstoned.len(), 1);
    assert_eq!(report.tombstoned[0].reason, "user_requested");
    assert_eq!(report.awaiting_purge, 1);
    assert_eq!(
        store.recall(&owner(), None, at(1)).expect("检索").len(),
        0,
        "用户点完删除，它就该立刻消失——哪怕清理还没跑"
    );
}

#[test]
fn forgetting_twice_is_an_idempotent_success() {
    // 用户看到删除请求失败，会以为没删掉，然后点第二次。第二次必须成功，否则界面会
    // 把他困在一个"删不掉"的提示上——而东西其实早就不见了。
    let mut store = in_memory();
    store
        .record_memory(&entry("fact", MemoryKind::Fact, 0))
        .expect("写入");
    forget(&mut store, &id("fact"), "user_requested", at(1)).expect("第一次");

    let second = forget(&mut store, &id("fact"), "user_requested", at(2)).expect("第二次");
    assert!(second.tombstoned.is_empty(), "不该再报一次删除");
    assert_eq!(second.awaiting_purge, 1, "它仍然在等着清理");
}

#[test]
fn forgetting_something_that_never_existed_is_an_error() {
    // 这一条与上一条方向相反，也同样是给用户的：删除一个不存在的标识，用户可能填错了名字，
    // 静默成功会让他以为删掉了某个东西。
    let mut store = in_memory();
    assert!(matches!(
        forget(&mut store, &id("ghost"), "user_requested", at(1)),
        Err(soca_core::CoreError::Storage(StorageError::MemoryNotFound { .. }))
    ));
}

// ---------------------------------------------------------------------------
// 审计账
// ---------------------------------------------------------------------------

#[test]
fn the_audit_ledger_is_pruned_on_its_own_schedule() {
    let mut store = in_memory();
    store
        .audit(at(0), AuditCategory::UnitTransition, "unit:x", "ready", "旧的")
        .expect("写审计");
    store
        .audit(
            at(40 * DAY),
            AuditCategory::UnitTransition,
            "unit:x",
            "ready",
            "新的",
        )
        .expect("写审计");

    let report = purge_retained(&mut store, &RetentionPolicy::default(), at(40 * DAY)).expect("清理");
    assert_eq!(report.audit_pruned, 1, "30 天以前的那条应当被裁掉");

    let left = store.audit_entries(64).expect("读审计");
    assert!(
        left.iter().all(|entry| entry.at >= at(10 * DAY)),
        "裁掉之后不该还有 30 天前的记录：{left:?}"
    );
    assert!(
        left.iter().any(|entry| entry.category == "retention_enforced"),
        "清理自己也要留痕"
    );
}

#[test]
fn audit_pruning_can_be_turned_off() {
    // 审计是**追责**依据，裁掉它会让"当初为什么这么做"永久无法回答。默认开，
    // 但调用方应当能明确关掉它，而不是只能接受。
    let mut store = in_memory();
    store
        .audit(at(0), AuditCategory::UnitTransition, "unit:x", "ready", "旧的")
        .expect("写审计");

    let policy = RetentionPolicy {
        prune_audit: false,
        ..RetentionPolicy::default()
    };
    let report = purge_retained(&mut store, &policy, at(40 * DAY)).expect("清理");
    assert_eq!(report.audit_pruned, 0);
    assert_eq!(store.audit_entries(64).expect("读").len(), 1);
}

// ---------------------------------------------------------------------------
// 报告里不该有什么
// ---------------------------------------------------------------------------

#[test]
fn the_retention_report_never_carries_the_claim_text() {
    // §12.3 对审计账的要求是"最小元数据；**不保存**密码、完整 prompt 或无限个人内容"。
    // 从保留期里清掉一条记忆，却把它的内容抄进报告或审计，等于绕了一圈又存了一份——
    // 而且是存在一个保留期更长的位置上。
    let mut store = in_memory();
    store
        .record_memory(&entry("secret", MemoryKind::Episode, 0))
        .expect("写入");

    let report = expire_retained(&mut store, at(8 * DAY)).expect("执行");
    let rendered = serde_json::to_string(&report).expect("可序列化");
    assert!(
        !rendered.contains("命题 secret"),
        "报告里不该出现命题原文：{rendered}"
    );
    assert!(
        rendered.contains("memory:secret"),
        "但要能定位到是哪一条：{rendered}"
    );

    let detail: String = store
        .audit_entries(64)
        .expect("读审计")
        .iter()
        .map(|entry| entry.detail.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !detail.contains("命题 secret"),
        "审计说明里也不该有：{detail}"
    );
}
