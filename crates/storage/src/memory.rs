//! L5 记忆主存、检索与删除传播（§4.1 L5、§12.3、§13.2）。
//!
//! 三条在接口层面就成立的性质：
//!
//! 1. **可见性由存储层过滤，不由调用方自觉跳过。** [`Store::recall`] 与 [`Store::memory`]
//!    永远不会返回 `Tombstoned` 条目——§12.3 要的是"立即不可见"，而不是"读的人记得不看"。
//!    想看被删除的内容必须显式调用 [`Store::memory_including_hidden`]，而那是审计入口。
//! 2. **修订是新增，不是改写。** [`Store::supersede`] 在一次事务里插入新条目、把旧条目标为
//!    `Superseded` 并回填继任者。旧条目的证据仍然躺在原处（§13.2：不覆盖原证据）。
//! 3. **删除两步走。** [`Store::tombstone`] 立即让它从检索中消失；[`Store::purge_tombstoned`]
//!    是另一条独立路径，由调用方在合适的时机发起（§12.3：先隐藏、再异步清理、给用户完成状态）。
//!
//! 失效传播（§12.3 的"传播到快照、摘要、向量索引"）落在 [`Store::tombstone_by_evidence`]：
//! 撤回一份证据，所有引用它的记忆一起失效。这是"授权文件派生索引"那条保留规则能成立的前提。

use rusqlite::{params, Connection, OptionalExtension};
use soca_contracts::{
    EvidenceRef, MemoryEntry, MemoryId, MemoryKind, MemoryStatus, SubjectId, WallClock,
};

use crate::error::StorageError;
use crate::Store;

/// 一行记忆的原始列，顺序与 [`ENTRY_COLUMNS`] 一致：
/// `(entry_json, status, superseded_by, tombstoned_at_utc, tombstone_reason)`。
type RawEntry = (String, String, Option<String>, Option<String>, Option<String>);

/// 读取时统一选出的列。
///
/// **生命周期字段只以列为准。** `entry_json` 保存的是条目内容，写入后不再随生命周期重写；
/// `status` / `superseded_by` / `tombstoned_*` 会变化，而它们只写在列上。两处都存同一份状态
/// 必然分叉——除非让其中一处成为唯一来源。让列当唯一来源还有个好处：状态迁移是一次
/// `UPDATE`，不存在"状态变了但 JSON 忘了跟着改"这种半更新。
const ENTRY_COLUMNS: &str =
    "entry_json, status, superseded_by, tombstoned_at_utc, tombstone_reason";

/// 把一行原始列拼成记忆条目。
fn assemble(raw: RawEntry) -> Result<MemoryEntry, StorageError> {
    let (json, status, superseded_by, tombstoned_at, tombstone_reason) = raw;
    let mut entry: MemoryEntry = serde_json::from_str(&json)?;

    entry.status = match status.as_str() {
        "active" => MemoryStatus::Active,
        "superseded" => MemoryStatus::Superseded,
        "tombstoned" => MemoryStatus::Tombstoned,
        _ => {
            return Err(StorageError::CorruptRow {
                field: "memory_entries.status",
            });
        }
    };
    entry.superseded_by = superseded_by.map(MemoryId::new).transpose()?;
    entry.tombstoned_at = tombstoned_at
        .map(|value| WallClock::from_rfc3339(&value))
        .transpose()?;
    entry.tombstone_reason = tombstone_reason;
    Ok(entry)
}

fn raw_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, Option<String>>(2)?,
        row.get::<_, Option<String>>(3)?,
        row.get::<_, Option<String>>(4)?,
    ))
}

/// 在给定连接（含事务）上读取一条记忆，不过滤状态。
fn load_entry(
    conn: &Connection,
    memory_id: &str,
) -> Result<Option<MemoryEntry>, StorageError> {
    let raw = conn
        .query_row(
            &format!("SELECT {ENTRY_COLUMNS} FROM memory_entries WHERE memory_id = ?1"),
            params![memory_id],
            raw_from_row,
        )
        .optional()?;
    raw.map(assemble).transpose()
}

/// 在给定连接上插入一条记忆及其证据索引。
fn insert_entry(conn: &Connection, entry: &MemoryEntry) -> Result<(), StorageError> {
    if let Some(existing) = load_entry(conn, entry.memory_id.as_str())? {
        // 同一标识完全相同的重复写入是幂等的；内容不同则是复用标识，必须拒绝。
        if existing == *entry {
            return Ok(());
        }
        return Err(StorageError::MemoryAlreadyRecorded {
            memory_id: entry.memory_id.to_string(),
        });
    }

    conn.execute(
        "INSERT INTO memory_entries (
             memory_id, kind, owner, task_id, unit_id, claim,
             evidence_refs_json, relation_refs_json, provenance_json, data_class,
             confidence_json, revision, supersedes, superseded_by,
             recorded_at_utc, valid_until_utc, status,
             tombstoned_at_utc, tombstone_reason, entry_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            entry.memory_id.to_string(),
            entry.kind.as_str(),
            entry.owner.to_string(),
            entry.task_id.as_ref().map(ToString::to_string),
            entry.unit_id.as_ref().map(ToString::to_string),
            entry.claim,
            serde_json::to_string(&entry.evidence_refs)?,
            serde_json::to_string(&entry.relation_refs)?,
            serde_json::to_string(&entry.provenance)?,
            entry.data_class.as_str(),
            entry
                .confidence
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            i64::from(entry.revision),
            entry.supersedes.as_ref().map(ToString::to_string),
            entry.superseded_by.as_ref().map(ToString::to_string),
            entry.recorded_at.to_string(),
            entry.valid_until.map(|value| value.to_string()),
            entry.status.as_str(),
            entry.tombstoned_at.map(|value| value.to_string()),
            entry.tombstone_reason,
            serde_json::to_string(entry)?,
        ],
    )?;

    for reference in &entry.evidence_refs {
        conn.execute(
            "INSERT OR IGNORE INTO memory_evidence (evidence_ref, memory_id) VALUES (?1, ?2)",
            params![reference.to_string(), entry.memory_id.to_string()],
        )?;
    }
    Ok(())
}

/// 读取某个所有者下仍然活跃的记忆。
fn load_owned(
    conn: &Connection,
    owner: &str,
    kind: Option<MemoryKind>,
) -> Result<Vec<MemoryEntry>, StorageError> {
    let sql = if kind.is_some() {
        format!(
            "SELECT {ENTRY_COLUMNS} FROM memory_entries
              WHERE owner = ?1 AND kind = ?2 AND status = 'active'
              ORDER BY recorded_at_utc, memory_id"
        )
    } else {
        format!(
            "SELECT {ENTRY_COLUMNS} FROM memory_entries
              WHERE owner = ?1 AND status = 'active'
              ORDER BY recorded_at_utc, memory_id"
        )
    };
    let mut stmt = conn.prepare(&sql)?;

    let mut raw: Vec<RawEntry> = Vec::new();
    match kind {
        Some(kind) => {
            let rows = stmt.query_map(params![owner, kind.as_str()], raw_from_row)?;
            for row in rows {
                raw.push(row?);
            }
        }
        None => {
            let rows = stmt.query_map(params![owner], raw_from_row)?;
            for row in rows {
                raw.push(row?);
            }
        }
    }
    raw.into_iter().map(assemble).collect()
}

impl Store {
    /// 写入一条记忆。返回 `true` 表示新写入，`false` 表示幂等重放。
    pub fn record_memory(&mut self, entry: &MemoryEntry) -> Result<bool, StorageError> {
        entry.validate()?;
        let existed = load_entry(self.connection(), entry.memory_id.as_str())?.is_some();
        insert_entry(self.connection(), entry)?;
        Ok(!existed)
    }

    /// 按标识读取一条**可见**记忆。已删除或已被取代的返回 `None`。
    pub fn memory(&self, memory_id: &MemoryId) -> Result<Option<MemoryEntry>, StorageError> {
        Ok(load_entry(self.connection(), memory_id.as_str())?
            .filter(|entry| entry.status == MemoryStatus::Active))
    }

    /// 按标识读取一条记忆，**不过滤状态**。
    ///
    /// 只用于审计与"这条为什么不见了"的追责。正常检索走 [`Store::recall`]：让调用方有机会
    /// 读到被删除的内容，就等于把 §12.3 的"立即不可见"降级成了一句建议。
    pub fn memory_including_hidden(
        &self,
        memory_id: &MemoryId,
    ) -> Result<Option<MemoryEntry>, StorageError> {
        load_entry(self.connection(), memory_id.as_str())
    }

    /// 检索某个所有者下、未超过保留期的可见记忆。
    ///
    /// 过期过滤在 Rust 侧做，而不是拿 RFC 3339 字符串在 SQL 里比大小：字符串比较只在格式
    /// 完全一致时才等价于时间比较，而"格式恰好一致"不是一条能被编译器保证的性质。
    pub fn recall(
        &self,
        owner: &SubjectId,
        kind: Option<MemoryKind>,
        at: WallClock,
    ) -> Result<Vec<MemoryEntry>, StorageError> {
        Ok(load_owned(self.connection(), owner.as_str(), kind)?
            .into_iter()
            .filter(|entry| !entry.is_expired_at(at))
            .collect())
    }

    /// 已经超过保留期、但尚未删除的可见记忆。
    ///
    /// §12.3 要求删除走 tombstone 流程并给用户完成状态，所以到期**不等于**已经删除：本方法
    /// 只回答"哪些该清理了"，由调用方显式发起 [`Store::tombstone`]。
    pub fn expired_memories(&self, at: WallClock) -> Result<Vec<MemoryEntry>, StorageError> {
        let mut stmt = self.connection().prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM memory_entries
              WHERE status = 'active'
              ORDER BY recorded_at_utc, memory_id"
        ))?;
        let rows = stmt.query_map([], raw_from_row)?;

        let mut expired = Vec::new();
        for row in rows {
            let entry = assemble(row?)?;
            if entry.is_expired_at(at) {
                expired.push(entry);
            }
        }
        Ok(expired)
    }

    /// 当前已经隐藏、等着清理的条目数。
    ///
    /// 界面需要它来区分"删除已完成"与"删除还在排队"（§12.3 要求给用户完成状态）。两者都
    /// 显示成一个勾，用户就无从知道内容是不是真的走了。
    pub fn tombstoned_memory_count(&self) -> Result<usize, StorageError> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM memory_entries WHERE status = 'tombstoned'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// 用一条新修订取代旧条目（§13.2：不覆盖原证据）。
    ///
    /// 一次事务里做三件事：插入新条目、把旧条目标为 `Superseded`、回填旧条目的继任者。
    /// 任何一步单独失败都会让"哪条是当前信念"变得无法回答。
    pub fn supersede(
        &mut self,
        previous: &MemoryId,
        mut next: MemoryEntry,
    ) -> Result<(), StorageError> {
        if next.supersedes.as_ref() != Some(previous) {
            return Err(StorageError::FailClosed(
                "取代关系必须显式指向被取代的条目，否则旧条目会永远停留在 ACTIVE",
            ));
        }

        let tx = self.connection_mut().transaction()?;
        let Some(existing) = load_entry(&tx, previous.as_str())? else {
            return Err(StorageError::MemoryNotFound {
                memory_id: previous.to_string(),
            });
        };
        match existing.status {
            MemoryStatus::Active => {}
            MemoryStatus::Superseded => {
                return Err(StorageError::MemoryAlreadySuperseded {
                    memory_id: previous.to_string(),
                });
            }
            MemoryStatus::Tombstoned => {
                // 已删除的条目不能被"取代"复活。§12.3 的语义是删除，不是标记旧版本。
                return Err(StorageError::MemoryAlreadyDeleted {
                    memory_id: previous.to_string(),
                });
            }
        }
        if existing.owner != next.owner {
            return Err(StorageError::FailClosed(
                "取代不能跨越所有者：§4.1 L5 要求记忆按所有者隔离",
            ));
        }

        next.status = MemoryStatus::Active;
        next.superseded_by = None;
        insert_entry(&tx, &next)?;
        tx.execute(
            "UPDATE memory_entries
                SET status = 'superseded', superseded_by = ?1
              WHERE memory_id = ?2",
            params![next.memory_id.to_string(), previous.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 让一条记忆立即不可见（§12.3 的第一步）。
    ///
    /// 幂等：重复删除不报错，也不覆盖最先写下的原因——"谁最先主张删除它"是审计需要的信息。
    pub fn tombstone(
        &mut self,
        memory_id: &MemoryId,
        reason: &str,
        at: WallClock,
    ) -> Result<(), StorageError> {
        let Some(existing) = load_entry(self.connection(), memory_id.as_str())? else {
            return Err(StorageError::MemoryNotFound {
                memory_id: memory_id.to_string(),
            });
        };
        if existing.status == MemoryStatus::Tombstoned {
            return Ok(());
        }
        self.connection().execute(
            "UPDATE memory_entries
                SET status = 'tombstoned', tombstoned_at_utc = ?1, tombstone_reason = ?2
              WHERE memory_id = ?3",
            params![at.to_string(), reason, memory_id.to_string()],
        )?;
        Ok(())
    }

    /// 撤回一份证据，让所有引用它的记忆一起失效（§12.3 的失效传播）。
    ///
    /// §12.3 那条"授权文件派生索引：撤销目录权限后立即不可检索，随后清理"就是本方法。
    /// 返回被牵连的条目数，供调用方给出完成状态。
    pub fn tombstone_by_evidence(
        &mut self,
        evidence_ref: &EvidenceRef,
        reason: &str,
        at: WallClock,
    ) -> Result<usize, StorageError> {
        let tx = self.connection_mut().transaction()?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT e.memory_id
                   FROM memory_entries e
                   JOIN memory_evidence v ON v.memory_id = e.memory_id
                  WHERE v.evidence_ref = ?1 AND e.status = 'active'",
            )?;
            let rows = stmt.query_map(params![evidence_ref.to_string()], |row| {
                row.get::<_, String>(0)
            })?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };

        for id in &ids {
            tx.execute(
                "UPDATE memory_entries
                    SET status = 'tombstoned', tombstoned_at_utc = ?1, tombstone_reason = ?2
                  WHERE memory_id = ?3",
                params![at.to_string(), reason, id],
            )?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// 清理已删除的记忆（§12.3 的第二步，由调用方显式发起）。
    ///
    /// 先删索引表再删主表：反过来的话，一次中途崩溃会留下一批指向不存在条目的索引项，
    /// 而它们会让后续的失效传播报出幽灵条目。
    pub fn purge_tombstoned(&mut self) -> Result<usize, StorageError> {
        let tx = self.connection_mut().transaction()?;
        tx.execute(
            "DELETE FROM memory_evidence
              WHERE memory_id IN (SELECT memory_id FROM memory_entries WHERE status = 'tombstoned')",
            [],
        )?;
        let removed = tx.execute(
            "DELETE FROM memory_entries WHERE status = 'tombstoned'",
            [],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// 某个所有者下的可见记忆条数。
    pub fn memory_count(&self, owner: &SubjectId) -> Result<usize, StorageError> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM memory_entries WHERE owner = ?1 AND status = 'active'",
            params![owner.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }
}
