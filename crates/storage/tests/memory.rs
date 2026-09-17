//! L5 记忆主存、删除与失效传播的回归测试（§4.1 L5、§12.3、§13.2）。

use soca_contracts::{
    ContractError, DataClass, EvidenceRef, MemoryEntry, MemoryId, MemoryKind, MemoryStatus,
    Provenance, SourceId, SubjectId, WallClock,
};
use soca_storage::{StorageError, Store};

fn at(offset_seconds: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset_seconds)
}

const DAY: i64 = 86_400;

fn owner(name: &str) -> SubjectId {
    SubjectId::new(format!("user:{name}")).expect("固定主体")
}

fn evidence(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn in_memory() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储")
}

fn entry(
    id: &str,
    kind: MemoryKind,
    for_owner: &str,
    claim: &str,
    evidence_name: &str,
    recorded_offset: i64,
) -> MemoryEntry {
    MemoryEntry::new(
        MemoryId::new(format!("memory:{id}")).expect("固定记忆标识"),
        kind,
        owner(for_owner),
        None,
        claim,
        vec![evidence(evidence_name)],
        Provenance::Sensor {
            adapter: SourceId::new("device:file-watcher").expect("固定适配器"),
        },
        DataClass::Personal,
        at(recorded_offset),
    )
    .expect("合法记忆")
}

// ---------------------------------------------------------------------------
// 契约层：没有证据的不算记忆
// ---------------------------------------------------------------------------

#[test]
fn a_memory_without_evidence_cannot_be_constructed() {
    // §7.2 的真相来源是原始可核验证据。没有证据的东西可以存在于草稿里，
    // 但不能进入任何被当作事实使用的存储。
    let result = MemoryEntry::new(
        MemoryId::new("memory:1").expect("固定标识"),
        MemoryKind::Fact,
        owner("alice"),
        None,
        "用户偏好深色主题",
        Vec::new(),
        Provenance::Sensor {
            adapter: SourceId::new("device:file-watcher").expect("固定适配器"),
        },
        DataClass::Personal,
        at(0),
    );
    assert!(matches!(
        result,
        Err(ContractError::MissingRefs {
            field: "memory.evidence_refs"
        })
    ));
}

#[test]
fn retention_follows_the_section_12_3_table() {
    // §12.3：对话与转写初值 7 天；长期偏好/语义事实不自动过期，
    // 而是靠"保存来源、范围与可删除入口"来约束。
    let episode = entry("1", MemoryKind::Episode, "alice", "聊过一次摘要任务", "1", 0);
    let fact = entry("2", MemoryKind::Fact, "alice", "用户的摘要目录", "2", 0);

    assert_eq!(episode.valid_until, Some(at(7 * DAY)));
    assert_eq!(fact.valid_until, None);
}

// ---------------------------------------------------------------------------
// 删除：先隐藏，后清理
// ---------------------------------------------------------------------------

#[test]
fn a_tombstoned_memory_is_immediately_invisible_but_still_auditable() {
    let mut store = in_memory();
    let item = entry("1", MemoryKind::Fact, "alice", "摘要目录是 D:\\资料\\摘要", "1", 0);
    let id = item.memory_id.clone();
    store.record_memory(&item).expect("写入");

    assert!(store.memory(&id).expect("读取").is_some());
    assert_eq!(store.memory_count(&owner("alice")).expect("计数"), 1);

    store
        .tombstone(&id, "用户要求删除", at(1))
        .expect("删除");

    // §12.3：立即不可见。这个"立即"由存储层保证，不依赖调用方记得跳过。
    assert!(store.memory(&id).expect("读取").is_none());
    assert_eq!(store.memory_count(&owner("alice")).expect("计数"), 0);
    assert!(
        store
            .recall(&owner("alice"), None, at(1))
            .expect("检索")
            .is_empty()
    );

    // 但审计入口仍能读出来——这是"删除"与"篡改历史"的区别。
    let audited = store
        .memory_including_hidden(&id)
        .expect("读取")
        .expect("审计入口仍可读");
    assert_eq!(audited.status, MemoryStatus::Tombstoned);
    assert_eq!(audited.tombstone_reason.as_deref(), Some("用户要求删除"));
    assert_eq!(audited.tombstoned_at, Some(at(1)));
}

#[test]
fn tombstoning_twice_keeps_the_first_reason() {
    let mut store = in_memory();
    let item = entry("1", MemoryKind::Fact, "alice", "一条结论", "1", 0);
    let id = item.memory_id.clone();
    store.record_memory(&item).expect("写入");

    store.tombstone(&id, "用户要求删除", at(1)).expect("首次");
    store.tombstone(&id, "另一条理由", at(2)).expect("重复删除是幂等的");

    let audited = store
        .memory_including_hidden(&id)
        .expect("读取")
        .expect("存在");
    assert_eq!(
        audited.tombstone_reason.as_deref(),
        Some("用户要求删除"),
        "谁最先主张删除它，是审计需要保留的信息"
    );
    assert_eq!(audited.tombstoned_at, Some(at(1)));
}

#[test]
fn purge_removes_only_tombstoned_entries() {
    let mut store = in_memory();
    let doomed = entry("1", MemoryKind::Fact, "alice", "要删掉的结论", "1", 0);
    let kept = entry("2", MemoryKind::Fact, "alice", "要留着的结论", "2", 0);
    store.record_memory(&doomed).expect("写入");
    store.record_memory(&kept).expect("写入");

    store
        .tombstone(&doomed.memory_id, "用户要求删除", at(1))
        .expect("删除");
    assert_eq!(store.purge_tombstoned().expect("清理"), 1);

    assert!(
        store
            .memory_including_hidden(&doomed.memory_id)
            .expect("读取")
            .is_none(),
        "两段式删除的第二步：彻底清理"
    );
    assert!(
        store
            .memory(&kept.memory_id)
            .expect("读取")
            .is_some(),
        "活着的条目不受清理影响"
    );
    assert_eq!(store.purge_tombstoned().expect("再次清理"), 0);
}

// ---------------------------------------------------------------------------
// 修订：新增，不是改写
// ---------------------------------------------------------------------------

#[test]
fn superseding_adds_an_entry_and_keeps_the_original_evidence() {
    let mut store = in_memory();
    let first = entry("1", MemoryKind::Fact, "alice", "摘要目录是 D:\\资料\\摘要", "1", 0);
    let first_id = first.memory_id.clone();
    store.record_memory(&first).expect("写入");

    let next = first
        .next_revision(
            MemoryId::new("memory:2").expect("固定标识"),
            "摘要目录是 D:\\资料\\新摘要",
            vec![evidence("2")],
            at(10),
        )
        .expect("构造下一版");
    store.supersede(&first_id, next).expect("取代");

    // 旧条目退出检索，但它的证据仍在原处（§13.2：不覆盖原证据）。
    assert!(store.memory(&first_id).expect("读取").is_none());
    let old = store
        .memory_including_hidden(&first_id)
        .expect("读取")
        .expect("仍然存在");
    assert_eq!(old.status, MemoryStatus::Superseded);
    assert_eq!(
        old.evidence_refs,
        vec![evidence("1")],
        "当初判断的依据必须原样保留"
    );
    assert_eq!(
        old.superseded_by.expect("回填继任者").to_string(),
        "memory:2"
    );
    assert_eq!(old.revision, 1);

    let current = store
        .memory(&MemoryId::new("memory:2").expect("固定标识"))
        .expect("读取")
        .expect("当前信念");
    assert_eq!(current.revision, 2);
    assert_eq!(
        current.supersedes.expect("指向前一版").to_string(),
        "memory:1"
    );
    assert_eq!(current.status, MemoryStatus::Active);
}

#[test]
fn a_tombstoned_memory_cannot_be_revived_by_superseding_it() {
    // 删除就是删除，不是"标记旧版本"。允许取代一个已删除的条目，等于给了一条绕过用户
    // 删除请求的路径。
    let mut store = in_memory();
    let first = entry("1", MemoryKind::Fact, "alice", "原结论", "1", 0);
    let first_id = first.memory_id.clone();
    store.record_memory(&first).expect("写入");
    store.tombstone(&first_id, "用户要求删除", at(1)).expect("删除");

    let next = first
        .next_revision(
            MemoryId::new("memory:2").expect("固定标识"),
            "换个说法再写一遍",
            vec![evidence("2")],
            at(2),
        )
        .expect("构造下一版");
    let result = store.supersede(&first_id, next);
    assert!(matches!(
        result,
        Err(StorageError::MemoryAlreadyDeleted { .. })
    ));
}

#[test]
fn superseding_cannot_cross_owners() {
    // §4.1 L5 要求记忆按所有者隔离。允许跨越所有者的取代，就是允许一个主体的事后修订
    // 悄悄改变另一个主体的信念。
    let mut store = in_memory();
    let alice = entry("1", MemoryKind::Fact, "alice", "alice 的结论", "1", 0);
    store.record_memory(&alice).expect("写入");

    let mut bob = entry("2", MemoryKind::Fact, "bob", "bob 的说法", "2", 1);
    bob.supersedes = Some(alice.memory_id.clone());
    let result = store.supersede(&alice.memory_id, bob);
    assert!(matches!(result, Err(StorageError::FailClosed(_))));
}

#[test]
fn a_memory_id_cannot_be_reused_for_different_content() {
    let mut store = in_memory();
    let first = entry("1", MemoryKind::Fact, "alice", "原结论", "1", 0);
    store.record_memory(&first).expect("写入");
    assert!(
        !store.record_memory(&first).expect("重复写入"),
        "完全相同的重复写入是幂等的"
    );

    let tampered = entry("1", MemoryKind::Fact, "alice", "事后改过的结论", "2", 1);
    let result = store.record_memory(&tampered);
    assert!(matches!(
        result,
        Err(StorageError::MemoryAlreadyRecorded { .. })
    ));
    assert_eq!(
        store
            .memory(&first.memory_id)
            .expect("读取")
            .expect("存在")
            .claim,
        "原结论"
    );
}

// ---------------------------------------------------------------------------
// §12.3 的失效传播
// ---------------------------------------------------------------------------

#[test]
fn revoking_evidence_invalidates_every_memory_that_cited_it() {
    // §12.3：「撤回权限或删除个人数据时，传播到快照、摘要、向量索引、模型会话缓存和备份保留
    // 计划。」表中那条"授权文件派生索引：撤销目录权限后立即不可检索，随后清理"就是本测试。
    let mut store = in_memory();
    let first = entry("1", MemoryKind::Fact, "alice", "摘要 1 的内容", "shared", 0);
    let second = entry("2", MemoryKind::Summary, "alice", "摘要 1 的压缩", "shared", 1);
    let unrelated = entry("3", MemoryKind::Fact, "alice", "另一份证据得出的结论", "other", 2);
    for item in [&first, &second, &unrelated] {
        store.record_memory(item).expect("写入");
    }
    assert_eq!(store.memory_count(&owner("alice")).expect("计数"), 3);

    let invalidated = store
        .tombstone_by_evidence(&evidence("shared"), "目录权限已撤回", at(3))
        .expect("传播");

    assert_eq!(invalidated, 2, "引用该证据的条目一起失效");
    assert!(store.memory(&first.memory_id).expect("读取").is_none());
    assert!(
        store.memory(&second.memory_id).expect("读取").is_none(),
        "由它派生的摘要也要一起失效"
    );
    assert!(
        store.memory(&unrelated.memory_id).expect("读取").is_some(),
        "不依赖该证据的结论不受牵连"
    );
    assert_eq!(store.memory_count(&owner("alice")).expect("计数"), 1);

    // 失效后的清理仍然走两段式。
    assert_eq!(store.purge_tombstoned().expect("清理"), 2);
    assert_eq!(store.memory_count(&owner("alice")).expect("计数"), 1);
}

#[test]
fn propagation_does_not_touch_already_superseded_entries() {
    // 已经被取代的历史条目不该被失效传播再改一次状态：它的 superseded 状态本身就是
    // "不再参与检索"的证据，覆盖它会丢掉"这条是被取代的，不是被撤回的"这个区别。
    let mut store = in_memory();
    let first = entry("1", MemoryKind::Fact, "alice", "旧结论", "shared", 0);
    store.record_memory(&first).expect("写入");

    let next = first
        .next_revision(
            MemoryId::new("memory:2").expect("固定标识"),
            "新结论",
            vec![evidence("shared")],
            at(1),
        )
        .expect("构造下一版");
    store.supersede(&first.memory_id, next).expect("取代");

    let invalidated = store
        .tombstone_by_evidence(&evidence("shared"), "目录权限已撤回", at(2))
        .expect("传播");
    assert_eq!(invalidated, 1, "只有仍然活跃的那条被牵连");

    let old = store
        .memory_including_hidden(&first.memory_id)
        .expect("读取")
        .expect("存在");
    assert_eq!(old.status, MemoryStatus::Superseded);
    // 仍然活跃的那条被牵连删除；这里必须走审计入口读它，因为 `memory()` 按设计
    // 不返回任何已删除条目——那正是 §12.3"立即不可见"在接口上的样子。
    let current = store
        .memory_including_hidden(&MemoryId::new("memory:2").expect("固定标识"))
        .expect("读取")
        .expect("存在");
    assert_eq!(current.status, MemoryStatus::Tombstoned);
    assert!(
        store
            .memory(&MemoryId::new("memory:2").expect("固定标识"))
            .expect("读取")
            .is_none(),
        "被传播删除之后，常规检索读不到它"
    );
}

// ---------------------------------------------------------------------------
// 保留期与隔离
// ---------------------------------------------------------------------------

#[test]
fn recall_filters_out_expired_entries() {
    let mut store = in_memory();
    let episode = entry("1", MemoryKind::Episode, "alice", "聊过一次摘要任务", "1", 0);
    let fact = entry("2", MemoryKind::Fact, "alice", "用户的摘要目录", "2", 0);
    store.record_memory(&episode).expect("写入");
    store.record_memory(&fact).expect("写入");

    let within = store.recall(&owner("alice"), None, at(3 * DAY)).expect("检索");
    assert_eq!(within.len(), 2, "保留期内两条都在");

    let afterwards = store.recall(&owner("alice"), None, at(8 * DAY)).expect("检索");
    assert_eq!(afterwards.len(), 1, "情景到期后不再参与检索");
    assert_eq!(afterwards[0].kind, MemoryKind::Fact);

    // 到期不等于已删除：§12.3 要求清理走显式流程并给用户完成状态。
    assert_eq!(
        store.expired_memories(at(8 * DAY)).expect("到期清单").len(),
        1
    );
    assert!(
        store
            .memory_including_hidden(&episode.memory_id)
            .expect("读取")
            .is_some(),
        "到期只是「该清理了」，不是「已经清理了」"
    );
}

#[test]
fn recall_is_scoped_to_the_owner() {
    // §4.1 L5："横切；按所有者和任务隔离"。跨所有者检索不是一个"多加一个过滤条件"的功能，
    // 而是根本不提供入口。
    let mut store = in_memory();
    store
        .record_memory(&entry("1", MemoryKind::Fact, "alice", "alice 的结论", "1", 0))
        .expect("写入");
    store
        .record_memory(&entry("2", MemoryKind::Fact, "bob", "bob 的结论", "2", 0))
        .expect("写入");

    let alice = store.recall(&owner("alice"), None, at(0)).expect("检索");
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].claim, "alice 的结论");
    assert_eq!(store.memory_count(&owner("bob")).expect("计数"), 1);
}

#[test]
fn recall_can_be_narrowed_by_kind() {
    let mut store = in_memory();
    store
        .record_memory(&entry("1", MemoryKind::Fact, "alice", "一条事实", "1", 0))
        .expect("写入");
    store
        .record_memory(&entry("2", MemoryKind::Skill, "alice", "一个技能", "2", 1))
        .expect("写入");

    let facts = store
        .recall(&owner("alice"), Some(MemoryKind::Fact), at(0))
        .expect("检索");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind, MemoryKind::Fact);
}
