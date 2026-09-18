//! 内容仓的元数据侧（§9.3、§12.3）。
//!
//! §9.3 把内容拆成两半："**元数据存内容引用、校验和与保留期**"，字节在内容仓里。
//! 本模块是前半句——`blobs` 表。
//!
//! 与 [`crate::content::ContentStore`] 的分工与顺序：
//!
//! ```text
//!   写：content.put(bytes)          ← 先落盘、先耐久化
//!        store.record_blob(&written) ← 再提交引用
//!
//!   删：store.retire_blob(ref, at)   ← 先标记（保留期从这里开始算）
//!        content.remove(ref)         ← 再删字节
//! ```
//!
//! **两步的顺序都不能反。** 写反了会留下"元数据说它在、磁盘上没有"的引用，而那正是 §9.3
//! 要求"返回可诊断缺失，不能伪造证据"的那种状态；删反了会留下"字节没了、元数据还在"的引用，
//! 情况和前者一样，只是更难归因。这两步的编排属于调用方（`core`），因为"什么时候算写成功"
//! 是运行时的问题，不是存储层能替它决定的。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{BlobRef, Sha256Hex, WallClock};

use crate::content::{ContentGc, ContentStore};
use crate::error::StorageError;
use crate::Store;

/// 一条内容对象的元数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBlob {
    /// 对象引用。
    pub blob_ref: BlobRef,
    /// 内容摘要。
    pub sha256: Sha256Hex,
    /// 媒体类型。
    pub media_type: String,
    /// 字节数。
    pub bytes: u64,
    /// 写入时刻。
    pub created_at: WallClock,
    /// 退休时刻。§12.3 的保留期从这里开始算。
    pub retired_at: Option<WallClock>,
}

impl StoredBlob {
    /// 是否已经退休（不再被新的引用采用，等着清理）。
    pub fn is_retired(&self) -> bool {
        self.retired_at.is_some()
    }
}

const BLOB_COLUMNS: &str = "blob_ref, sha256, media_type, bytes, created_at_utc, retired_at_utc";

fn assemble(raw: (String, String, String, i64, String, Option<String>)) -> Result<StoredBlob, StorageError> {
    let (blob_ref, sha256, media_type, bytes, created_at, retired_at) = raw;
    Ok(StoredBlob {
        blob_ref: BlobRef::new(blob_ref)?,
        sha256: Sha256Hex::parse(sha256)?,
        media_type,
        bytes: u64::try_from(bytes).map_err(|_| StorageError::CorruptRow {
            field: "blobs.bytes",
        })?,
        created_at: WallClock::from_rfc3339(&created_at)?,
        retired_at: retired_at
            .map(|value| WallClock::from_rfc3339(&value))
            .transpose()?,
    })
}

fn raw_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, i64, String, Option<String>)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn load(conn: &rusqlite::Connection, blob_ref: &str) -> Result<Option<StoredBlob>, StorageError> {
    let raw = conn
        .query_row(
            &format!("SELECT {BLOB_COLUMNS} FROM blobs WHERE blob_ref = ?1"),
            params![blob_ref],
            raw_from_row,
        )
        .optional()?;
    raw.map(assemble).transpose()
}

impl Store {
    /// 登记一个内容对象。返回 `false` 表示此前已经登记过。
    ///
    /// 同引用重复登记是幂等的，而且**不会清掉 `retired_at`**：一次重放就能让一个已退休的
    /// 内容对象重新变成在册的，那正好是一条绕过保留期的路。摘要不一致时拒绝——按内容寻址的
    /// 引用指向的应当永远是同一份字节，对不上说明上游把两个东西搞混了。
    pub fn record_blob(
        &mut self,
        blob_ref: &BlobRef,
        sha256: &Sha256Hex,
        media_type: &str,
        bytes: u64,
        at: WallClock,
    ) -> Result<bool, StorageError> {
        if let Some(existing) = load(self.connection(), blob_ref.as_str())? {
            if existing.sha256 != *sha256 || existing.bytes != bytes {
                return Err(StorageError::FailClosed(
                    "同一内容引用对应了不同的摘要或长度；按内容寻址的引用不该出现这种情况",
                ));
            }
            return Ok(false);
        }
        self.connection().execute(
            "INSERT INTO blobs (blob_ref, sha256, media_type, bytes, created_at_utc, retired_at_utc)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![
                blob_ref.to_string(),
                sha256.to_string(),
                media_type,
                i64::try_from(bytes).unwrap_or(i64::MAX),
                at.to_string(),
            ],
        )?;
        Ok(true)
    }

    /// 读一个内容对象的元数据。
    pub fn blob(&self, blob_ref: &BlobRef) -> Result<Option<StoredBlob>, StorageError> {
        load(self.connection(), blob_ref.as_str())
    }

    /// 标记一个内容对象退休（§12.3：到期或用户删除）。
    ///
    /// 只标记，不删字节：§12.3 要的是"先隐藏、再异步清理"，而这里的"隐藏"就是让它不再被
    /// 新的引用采用。清理走 [`Store::gc_content`]。
    ///
    /// 重复退休是幂等的，且**保留第一次的时刻**：保留期从"第一次不再使用"开始算，
    /// 每次重放都把时钟往后拨，会让一份内容永远差一点到期。
    pub fn retire_blob(
        &mut self,
        blob_ref: &BlobRef,
        at: WallClock,
    ) -> Result<bool, StorageError> {
        let changed = self.connection().execute(
            "UPDATE blobs SET retired_at_utc = ?2
              WHERE blob_ref = ?1 AND retired_at_utc IS NULL",
            params![blob_ref.to_string(), at.to_string()],
        )?;
        Ok(changed > 0)
    }

    /// 已经退休、且退休时刻早于给定时刻的对象，按退休时刻升序。
    pub fn retired_blobs_before(
        &self,
        cutoff: WallClock,
    ) -> Result<Vec<StoredBlob>, StorageError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {BLOB_COLUMNS} FROM blobs
              WHERE retired_at_utc IS NOT NULL AND retired_at_utc < ?1
              ORDER BY retired_at_utc, blob_ref"
        ))?;
        let rows = statement.query_map(params![cutoff.to_string()], raw_from_row)?;
        let mut found = Vec::new();
        for row in rows {
            found.push(assemble(row?)?);
        }
        Ok(found)
    }

    /// 在册的内容对象数。
    pub fn blob_count(&self) -> Result<usize, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// 在册的全部引用。
    pub fn blob_refs(&self) -> Result<Vec<BlobRef>, StorageError> {
        let mut statement = self
            .connection()
            .prepare("SELECT blob_ref FROM blobs ORDER BY blob_ref")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut found = Vec::new();
        for row in rows {
            found.push(BlobRef::new(row?)?);
        }
        Ok(found)
    }

    /// 回收内容仓（§9.3）。
    ///
    /// 三件事，按顺序：
    ///
    /// 1. 冻结时间之前退休的对象——先删字节，再删元数据行。反过来的话，中间崩溃会留下
    ///    "元数据在、字节没了"的引用，而那种引用读到的是可诊断缺失；先删字节则是"字节没了、
    ///    元数据还在"，情况相同但更难归因，所以顺序选了前一种。
    /// 2. 孤儿对象——磁盘上有、元数据里没有。§9.3 说的"崩溃留下的孤儿对象"就是它们。
    /// 3. 中断残留——写到一半就崩了留下的临时文件。
    pub fn gc_content(
        &mut self,
        content: &ContentStore,
        cutoff: WallClock,
    ) -> Result<ContentGc, StorageError> {
        let retired = self.retired_blobs_before(cutoff)?;
        let mut report = ContentGc::default();

        for blob in &retired {
            if content.remove(&blob.blob_ref)? {
                report.bytes_freed = report.bytes_freed.saturating_add(blob.bytes);
            }
            self.connection().execute(
                "DELETE FROM blobs WHERE blob_ref = ?1",
                params![blob.blob_ref.to_string()],
            )?;
        }

        let known: std::collections::BTreeSet<String> = self
            .blob_refs()?
            .iter()
            .map(ToString::to_string)
            .collect();
        let swept = content.gc(&known)?;

        report.orphans = swept.orphans;
        report.stray_temporaries = swept.stray_temporaries;
        report.bytes_freed = report
            .bytes_freed
            .saturating_add(swept.bytes_freed);
        Ok(report)
    }

    /// 读一个内容对象的字节。
    ///
    /// 元数据里没有它时报 [`StorageError::ContentMissing`]——§9.3 要求"已有引用指向缺失对象时
    /// 返回**可诊断缺失**，不能伪造证据"。返回 `Ok(None)` 会把"这条引用是坏的"和"这个对象
    /// 不存在"混成一件事，而前者需要有人去查。
    pub fn read_content(
        &self,
        content: &ContentStore,
        blob_ref: &BlobRef,
    ) -> Result<Vec<u8>, StorageError> {
        if self.blob(blob_ref)?.is_none() {
            return Err(StorageError::ContentMissing {
                blob_ref: blob_ref.to_string(),
            });
        }
        content
            .get(blob_ref)?
            .ok_or_else(|| StorageError::ContentMissing {
                blob_ref: blob_ref.to_string(),
            })
    }
}
