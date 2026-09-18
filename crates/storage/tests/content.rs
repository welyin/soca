//! 分段内容仓的回归测试（§9.3、§10.4、§12.3）。
//!
//! 三组性质，每组都对应一条**失败时不会报错、只会悄悄错**的东西：
//!
//! * **引用拼进路径**。一个 `blob:../../x:...` 能读写仓外的任何文件，而它构造得出来——
//!   `BlobRef` 只校验前缀。
//! * **内容被换掉**。缺失会被发现，被换掉的内容看起来是好的。
//! * **孤儿对象**。崩溃最容易留下的东西：字节写完改名成功了，元数据那一行还没提交。

use std::collections::BTreeSet;

use soca_contracts::{BlobRef, DataClass, Sha256Hex, WallClock};
use soca_storage::{ContentStore, StorageError, Store};

fn at(offset_seconds: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset_seconds)
}

fn in_memory() -> Store {
    Store::open_in_memory(at(0)).expect("内存存储")
}

fn digest_of(bytes: &[u8]) -> String {
    Sha256Hex::of_bytes(bytes).to_string()
}

/// 内容对象在磁盘上的位置。**故意在这里重算一遍**：布局是 §9.3 的一部分，
/// 测试跟着它走，布局改了测试就该红，而不是跟着改。
fn path_of(store: &ContentStore, reference: &BlobRef) -> std::path::PathBuf {
    let rest = reference.as_str().strip_prefix("blob:").expect("前缀");
    let (class, digest) = rest.split_once(':').expect("类别与摘要");
    store
        .root()
        .join(class)
        .join(&digest[..2])
        .join(digest)
}

// ---------------------------------------------------------------------------
// 往返与寻址
// ---------------------------------------------------------------------------

#[test]
fn content_round_trips_and_is_addressed_by_its_digest() {
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content
        .put("一段对话原文".as_bytes(), DataClass::Personal)
        .expect("写入");

    assert_eq!(
        written.blob_ref.as_str(),
        format!("blob:personal:{}", digest_of("一段对话原文".as_bytes()))
    );
    assert_eq!(
        content.get(&written.blob_ref).expect("读取"),
        Some("一段对话原文".as_bytes().to_vec())
    );
    assert!(content.contains(&written.blob_ref).expect("查询"));
}

#[test]
fn putting_the_same_bytes_twice_yields_one_object() {
    // 按内容寻址：同一份内容在同一个类别下永远只有一个对象。两次不同的"同一个文件"不该
    // 在仓里占两份空间，也不该有两个引用指向同一段字节。
    let content = ContentStore::temporary().expect("临时内容仓");
    let first = content.put(b"same", DataClass::Personal).expect("第一次");
    let second = content.put(b"same", DataClass::Personal).expect("第二次");

    assert_eq!(first.blob_ref, second.blob_ref);
    assert_eq!(content.list().expect("列出").len(), 1);
}

#[test]
fn the_same_bytes_in_two_classes_are_two_objects() {
    // §9.3 的"按租户和数据类别隔离"。类别进了引用，也进了目录——所以同一段文字被当成
    // 公开内容和个人内容存，是两条互不相干的记录，而不是共享一份。
    let content = ContentStore::temporary().expect("临时内容仓");
    let public = content.put(b"same", DataClass::Public).expect("公开");
    let personal = content.put(b"same", DataClass::Personal).expect("个人");

    assert_ne!(public.blob_ref, personal.blob_ref);
    assert_eq!(content.list().expect("列出").len(), 2);
}

#[test]
fn reading_something_that_was_never_written_is_none_not_an_error() {
    let content = ContentStore::temporary().expect("临时内容仓");
    let absent = BlobRef::new(format!("blob:personal:{}", "a".repeat(64))).expect("合法引用");
    assert_eq!(content.get(&absent).expect("读取"), None);
}

// ---------------------------------------------------------------------------
// 引用不是路径
// ---------------------------------------------------------------------------

#[test]
fn a_reference_that_is_not_shaped_like_an_object_is_refused() {
    // **这是本模块唯一的安全边界。** 引用从外部字符串解出来，然后被拼进文件路径；
    // 而 `BlobRef` 只校验前缀，所以下面这些全都构造得出来。
    //
    // 处理办法是白名单而不是清理："把 `..` 替换掉"永远会有想不到的写法，而白名单只有
    // 一种结果——不在名单里的，拒绝。
    let content = ContentStore::temporary().expect("临时内容仓");
    let valid = "a".repeat(64);

    let rejected = [
        format!("blob:..:{valid}"),
        format!("blob:../../etc:{valid}"),
        format!("blob:unknown-class:{valid}"),
        format!("blob:personal:{}", "a".repeat(63)),
        format!("blob:personal:{}", "a".repeat(65)),
        format!("blob:personal:{}", "A".repeat(64)),
        format!("blob:personal:{}", "g".repeat(64)),
        "blob:personal".to_string(),
    ];

    for raw in rejected {
        let reference = BlobRef::new(raw.clone()).expect("前缀合法，构造得出来");
        assert!(
            matches!(
                content.get(&reference),
                Err(StorageError::MalformedBlobRef { .. })
            ),
            "{raw} 应当被拒绝"
        );
        assert!(
            matches!(
                content.remove(&reference),
                Err(StorageError::MalformedBlobRef { .. })
            ),
            "{raw} 在删除路径上也应当被拒绝"
        );
    }
}

// ---------------------------------------------------------------------------
// 内容完整性
// ---------------------------------------------------------------------------

#[test]
fn reading_content_whose_bytes_were_replaced_is_refused_not_served() {
    // 缺失会被发现；**被换掉的内容看起来是好的**。所以读的时候要按引用里声明的摘要核一遍。
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"original", DataClass::Personal).expect("写入");

    std::fs::write(path_of(&content, &written.blob_ref), b"tampered").expect("替换字节");

    assert!(matches!(
        content.get(&written.blob_ref),
        Err(StorageError::ContentCorrupted { .. })
    ));
}

#[test]
fn registering_a_reference_with_a_different_digest_is_refused() {
    let mut store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"payload", DataClass::Personal).expect("写入");
    store
        .record_blob(
            &written.blob_ref,
            &written.sha256,
            "text/plain",
            written.bytes,
            at(0),
        )
        .expect("登记");

    assert!(
        store
            .record_blob(
                &written.blob_ref,
                &Sha256Hex::of_bytes(b"something else"),
                "text/plain",
                written.bytes,
                at(1),
            )
            .is_err(),
        "按内容寻址的引用指向的应当永远是同一份字节"
    );
}

// ---------------------------------------------------------------------------
// 元数据与字节的配合
// ---------------------------------------------------------------------------

#[test]
fn reading_something_with_no_metadata_is_a_diagnosable_missing() {
    // §9.3：已有引用指向缺失对象时返回**可诊断缺失**，不能伪造证据。返回 `Ok(None)` 会把
    // "这条引用是坏的"与"这个对象不存在"混成一件事，而前者需要有人去查。
    let store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");
    let absent = BlobRef::new(format!("blob:personal:{}", "b".repeat(64))).expect("合法引用");

    assert!(matches!(
        store.read_content(&content, &absent),
        Err(StorageError::ContentMissing { .. })
    ));
}

#[test]
fn a_reference_with_metadata_but_no_bytes_is_also_a_diagnosable_missing() {
    let mut store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"gone", DataClass::Personal).expect("写入");
    store
        .record_blob(
            &written.blob_ref,
            &written.sha256,
            "text/plain",
            written.bytes,
            at(0),
        )
        .expect("登记");

    content.remove(&written.blob_ref).expect("字节被抹掉");

    assert!(matches!(
        store.read_content(&content, &written.blob_ref),
        Err(StorageError::ContentMissing { .. })
    ));
}

// ---------------------------------------------------------------------------
// 回收
// ---------------------------------------------------------------------------

#[test]
fn gc_removes_objects_that_have_no_metadata_row() {
    // §9.3："崩溃留下的孤儿对象由 GC 回收。" 孤儿的定义就是这一条：磁盘上有、元数据里没有。
    let mut store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");

    let kept = content.put(b"registered", DataClass::Personal).expect("写入");
    store
        .record_blob(
            &kept.blob_ref,
            &kept.sha256,
            "text/plain",
            kept.bytes,
            at(0),
        )
        .expect("登记");
    let orphan = content.put(b"never registered", DataClass::Personal).expect("写入");

    let report = store.gc_content(&content, at(3600)).expect("回收");

    assert_eq!(report.orphans, 1);
    assert!(content.contains(&kept.blob_ref).expect("查询"), "在册的要留着");
    assert!(
        !content.contains(&orphan.blob_ref).expect("查询"),
        "孤儿要被回收"
    );
}

#[test]
fn gc_removes_stray_temporaries() {
    // 写到一半就崩了：临时文件留着，最终路径上什么都没有。它不是孤儿对象（连引用都算不出来），
    // 所以需要单独认一遍。
    let mut store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"x", DataClass::Personal).expect("写入");
    store
        .record_blob(&written.blob_ref, &written.sha256, "text/plain", 1, at(0))
        .expect("登记");

    let shard = path_of(&content, &written.blob_ref)
        .parent()
        .expect("分片目录")
        .to_path_buf();
    std::fs::write(shard.join(".tmp-crashed"), b"half written").expect("留下残留");

    let report = store.gc_content(&content, at(3600)).expect("回收");
    assert_eq!(report.stray_temporaries, 1);
    assert_eq!(report.orphans, 0, "在册的那个不是孤儿");
    assert!(content.contains(&written.blob_ref).expect("查询"));
}

#[test]
fn gc_takes_a_known_set_so_it_can_run_before_metadata_is_written() {
    // 同一个 `gc` 在两处用得上：元数据已经写好时（`Store::gc_content` 从表里取在册集合），
    // 和"写内容与提交元数据之间"那个窗口。直接调它时，在册集合由调用方给出。
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"y", DataClass::Personal).expect("写入");

    let empty = content.gc(&BTreeSet::new()).expect("回收");
    assert_eq!(empty.orphans, 1, "没有在册集合时，什么都是孤儿");

    // 再把同一份内容写回去，然后带着在册集合回收一次：这次它是被认识的。
    content.put(b"y", DataClass::Personal).expect("再写一次");
    let kept: BTreeSet<String> = [written.blob_ref.to_string()].into_iter().collect();
    let report = content.gc(&kept).expect("回收");
    assert_eq!(report.orphans, 0);
    assert_eq!(report.stray_temporaries, 0);
    assert!(content.contains(&written.blob_ref).expect("查询"));
}

// ---------------------------------------------------------------------------
// 保留期
// ---------------------------------------------------------------------------

#[test]
fn retiring_is_idempotent_and_keeps_the_first_moment() {
    // 保留期从"第一次不再使用"开始算。每次重放都把时钟往后拨，会让一份内容永远差一点到期。
    let mut store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"z", DataClass::Personal).expect("写入");
    store
        .record_blob(&written.blob_ref, &written.sha256, "text/plain", 1, at(0))
        .expect("登记");

    assert!(store.retire_blob(&written.blob_ref, at(100)).expect("退休"));
    assert!(
        !store.retire_blob(&written.blob_ref, at(9_999)).expect("再退休"),
        "第二次不该改动任何东西"
    );
    assert_eq!(
        store
            .blob(&written.blob_ref)
            .expect("读")
            .expect("存在")
            .retired_at,
        Some(at(100)),
        "保留第一次的时刻"
    );
}

#[test]
fn retired_content_is_purged_only_after_its_cutoff() {
    let mut store = in_memory();
    let content = ContentStore::temporary().expect("临时内容仓");
    let written = content.put(b"retained", DataClass::Personal).expect("写入");
    store
        .record_blob(
            &written.blob_ref,
            &written.sha256,
            "text/plain",
            written.bytes,
            at(0),
        )
        .expect("登记");
    store.retire_blob(&written.blob_ref, at(100)).expect("退休");

    let early = store.gc_content(&content, at(50)).expect("回收");
    assert!(early.is_empty(), "还没到回收时刻");
    assert!(content.contains(&written.blob_ref).expect("查询"));

    let late = store.gc_content(&content, at(200)).expect("回收");
    assert_eq!(late.bytes_freed, written.bytes);
    assert_eq!(store.blob_count().expect("计数"), 0);
    assert!(!content.contains(&written.blob_ref).expect("查询"));
}
