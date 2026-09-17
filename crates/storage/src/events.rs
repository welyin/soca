//! 事件日志（§7.1、§7.3、§9.2）。
//!
//! 事件日志是 append-only 的追加语义：写入只增不改，读取只能按游标向前。
//! §12.3 说明了这不是"永久不可删除"——撤回权限或删除个人数据时，先写 tombstone 使查询
//! 立即不可见，再异步清理。删除路径不在 P0 范围内，但读取接口已经只暴露"游标之后"，
//! 不会因为将来加了墓碑就改变调用方语义。
//!
//! 幂等去重在这里落地：§7.3 要求"至少一次消息 + 幂等处理"。重复投递的同一幂等键既不产生
//! 第二条事件，也不重复触发单元唤醒。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{Envelope, Provenance, WallClock};

use crate::error::StorageError;
use crate::Store;

/// 从日志读回的事件。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    /// 全库单调提交序。冷单元恢复时用它的前一个值作为游标（§9.2）。
    pub sequence: i64,
    /// 完整信封。重放读它，不读任何拼接出来的近似结构。
    pub envelope: Envelope,
    /// 落库时刻。注意它不等于 `envelope.observed_at_utc`。
    pub recorded_at: WallClock,
}

/// 追加密钥的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// 新事件已落库。
    Appended {
        /// 分配到的提交序。
        sequence: i64,
    },
    /// 幂等键或事件 ID 命中既有记录，本次没有写入任何新内容。
    Duplicate {
        /// 首次落库时的提交序。
        sequence: i64,
    },
}

impl AppendOutcome {
    /// 本次调用分配或命中的提交序。
    pub fn sequence(&self) -> i64 {
        match self {
            Self::Appended { sequence } | Self::Duplicate { sequence } => *sequence,
        }
    }

    /// 本次调用是否真的写入了新事件。
    pub fn is_new(&self) -> bool {
        matches!(self, Self::Appended { .. })
    }
}

impl Store {
    /// 追加一条事件。
    ///
    /// 一次事务内完成三件事：事件落库、幂等键登记、流位置占位。任何一步失败都整体回滚，
    /// 不会留下"事件写进去了但幂等键没登记"这种会让重复投递穿透的状态。
    pub fn append_event(
        &mut self,
        envelope: &Envelope,
        recorded_at: WallClock,
    ) -> Result<AppendOutcome, StorageError> {
        // 先校验：非法信封不应该占用幂等键，也不应该拿到提交序。
        envelope
            .validate()
            .map_err(|source| StorageError::EventRejected {
                event_id: envelope.event_id.to_string(),
                source,
            })?;

        let key = envelope.idempotency_key.as_str();
        let event_id = envelope.event_id.to_string();
        let tx = self.connection_mut().transaction()?;

        // 1. 幂等键是否已经用过。
        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT event_id, sequence FROM idempotency WHERE idempotency_key = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((existing_event_id, sequence)) = existing {
            if existing_event_id != event_id {
                return Err(StorageError::IdempotencyKeyConflict {
                    key: key.to_string(),
                    existing_event_id,
                    incoming_event_id: event_id,
                });
            }
            // 完全重复的投递：不写任何新内容。
            return Ok(AppendOutcome::Duplicate { sequence });
        }

        // 2. 事件 ID 是否已经存在（同一事件换了幂等键再投一次）。补登记幂等键后按重复处理。
        let existing_sequence: Option<i64> = tx
            .query_row(
                "SELECT sequence FROM events WHERE event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(sequence) = existing_sequence {
            tx.execute(
                "INSERT OR IGNORE INTO idempotency
                     (idempotency_key, event_id, sequence, first_seen_at_utc)
                 VALUES (?1, ?2, ?3, ?4)",
                params![key, event_id, sequence, recorded_at.to_string()],
            )?;
            tx.commit()?;
            return Ok(AppendOutcome::Duplicate { sequence });
        }

        // 3. 新事件。
        let (inline_payload, blob_ref) = match &envelope.payload_ref {
            soca_contracts::PayloadRef::Inline { body, .. } => (Some(body.as_str()), None),
            soca_contracts::PayloadRef::Blob { blob_ref, .. } => (None, Some(blob_ref.to_string())),
        };

        let insert = tx.execute(
            "INSERT INTO events (
                 event_id, task_id, source_id, source_epoch, boot_id, source_sequence,
                 observed_at_utc, received_monotonic_ns, data_class, provenance_kind,
                 instruction_authority, envelope_json, inline_payload, blob_ref, recorded_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                event_id,
                envelope.task_id.to_string(),
                envelope.source_id.to_string(),
                i64::from(envelope.source_epoch),
                envelope.boot_id.to_string(),
                envelope.source_sequence as i64,
                envelope.observed_at_utc.to_string(),
                envelope.received_monotonic.nanos as i64,
                envelope.data_class.as_str(),
                provenance_kind(&envelope.provenance),
                i64::from(envelope.provenance.is_instruction_authority()),
                serde_json::to_string(envelope)?,
                inline_payload,
                blob_ref,
                recorded_at.to_string(),
            ],
        );

        let sequence = match insert {
            Ok(_) => tx.last_insert_rowid(),
            Err(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // 区分两种约束：事件 ID 重复，还是同一个流位置被两个事件占用。
                return Err(classify_unique_violation(&tx, &event_id, envelope));
            }
            Err(other) => return Err(other.into()),
        };

        tx.execute(
            "INSERT INTO idempotency (idempotency_key, event_id, sequence, first_seen_at_utc)
             VALUES (?1, ?2, ?3, ?4)",
            params![key, event_id, sequence, recorded_at.to_string()],
        )?;

        tx.commit()?;
        Ok(AppendOutcome::Appended { sequence })
    }

    /// 读取游标之后的事件，按提交序升序。
    ///
    /// `cursor` 是"已经处理到的最后一个序列号"，初值 `0` 表示从头读。负数会被拒绝，避免
    /// 调用方用 `-1` 之类的手势表达"全部"而绕过游标语义。
    pub fn read_events_after(
        &self,
        cursor: i64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        if cursor < 0 {
            return Err(StorageError::InvalidCursor {
                cursor,
                reason: "游标不能为负；从头读请显式传 0",
            });
        }
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut stmt = self.connection().prepare(
            "SELECT sequence, envelope_json, recorded_at_utc
               FROM events
              WHERE sequence > ?1
              ORDER BY sequence
              LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![cursor, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut events = Vec::new();
        for row in rows {
            let (sequence, envelope_json, recorded_at) = row?;
            let envelope: Envelope = serde_json::from_str(&envelope_json)?;
            let recorded_at = WallClock::from_rfc3339(&recorded_at)?;
            events.push(StoredEvent {
                sequence,
                envelope,
                recorded_at,
            });
        }
        Ok(events)
    }

    /// 最新提交序。用作游标的高水位；没有任何事件时返回 `0`。
    pub fn latest_sequence(&self) -> Result<i64, StorageError> {
        let sequence: i64 = self.connection().query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        Ok(sequence)
    }

    /// 事件总数。用于对账与测试断言。
    pub fn event_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count)
    }
}

/// 把 `Provenance` 映射成审计查询用的短标签。
fn provenance_kind(provenance: &Provenance) -> &'static str {
    match provenance {
        Provenance::User { .. } => "user",
        Provenance::Sensor { .. } => "sensor",
        Provenance::Derived { .. } => "derived",
        Provenance::Tool { .. } => "tool",
    }
}

/// 唯一约束被触发后，判断具体是哪一条被占用。
fn classify_unique_violation(
    tx: &rusqlite::Transaction<'_>,
    event_id: &str,
    envelope: &Envelope,
) -> StorageError {
    let known: Option<i64> = tx
        .query_row(
            "SELECT sequence FROM events WHERE event_id = ?1",
            params![event_id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();

    if let Some(sequence) = known {
        return StorageError::DuplicateEvent {
            event_id: format!("{event_id}（已占据提交序 {sequence}）"),
        };
    }

    StorageError::StreamPositionConflict {
        source_id: envelope.source_id.to_string(),
        source_epoch: envelope.source_epoch,
        boot_id: envelope.boot_id.to_string(),
        source_sequence: envelope.source_sequence,
    }
}
