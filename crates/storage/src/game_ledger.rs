//! 游戏动作账（实施规格 §11.2、§13）。
//!
//! 这张表存在的唯一理由是让"检查—登记—执行—回执"这条链路在崩溃面前仍然只有一次世界步：
//!
//! ```text
//! 查 (episode_id, request_id) → 没有 → 登记 PENDING → 执行 step → 写回执并结算
//!                            → 有且载荷相同 → 返回原回执，不执行 step
//!                            → 有但载荷不同 → IDEMPOTENCY_CONFLICT
//! ```
//!
//! 关键是登记发生在执行**之前**。若登记在之后，崩溃窗口里的请求既查不到记录、又可能已经
//! 推进过世界；恢复时就只能靠猜。登记在前，则崩溃留下的就是一条 `PENDING`，
//! 恢复流程据此判定 `unknown_commit`（§13："不能简单 reset 然后冒充继续"）。
//!
//! 载荷哈希用请求的规范化 JSON 计算。`serde_json` 默认的 map 按 key 排序，因此同一份请求
//! 无论字段书写顺序如何，哈希一致。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{
    GameActionReceipt, GameLedgerStatus, PublicId, Sha256Hex, WallClock,
};

use crate::error::StorageError;
use crate::Store;

/// 账上的一条游戏请求。
#[derive(Debug, Clone, PartialEq)]
pub struct GameLedgerEntry {
    /// 回合。
    pub episode_id: PublicId,
    /// 请求标识。
    pub request_id: PublicId,
    /// 请求载荷哈希。
    pub request_hash: Sha256Hex,
    /// 提交者依据的观测。
    pub expected_observation_id: PublicId,
    /// 提交时的拓扑世代。
    pub topology_epoch: u64,
    /// 状态。
    pub status: GameLedgerStatus,
    /// 结果观测。
    pub result_observation_id: Option<PublicId>,
    /// 已结算的回执。
    pub receipt: Option<GameActionReceipt>,
    /// 登记时刻。
    pub recorded_at: WallClock,
    /// 结算时刻。
    pub settled_at: Option<WallClock>,
}

/// 登记一次游戏请求的判定。
#[derive(Debug, Clone, PartialEq)]
pub enum LedgerDecision {
    /// 新请求，可以执行一次世界步。
    Fresh,
    /// 同一请求已经结算：**返回原回执，不再执行 step**。
    Replay(Box<GameActionReceipt>),
    /// 同一请求标识被用于不同载荷。
    Conflict {
        /// 冲突说明。
        reason: String,
    },
    /// 已登记但尚无结论，说明上一次执行的结果未知。
    Pending,
}

impl LedgerDecision {
    /// 是否允许执行世界步。
    pub fn permits_step(&self) -> bool {
        matches!(self, Self::Fresh)
    }
}

impl Store {
    /// 登记一次游戏请求，并给出是否允许执行。
    ///
    /// 登记与查询在同一事务内完成：两个并发的同标识请求不可能都拿到 `Fresh`。
    pub fn record_game_request(
        &mut self,
        request: &soca_contracts::ActionRequest,
        at: WallClock,
    ) -> Result<LedgerDecision, StorageError> {
        let episode_id = request.episode_id.to_string();
        let request_id = request.request_id.to_string();
        let hash = Sha256Hex::of_bytes(&serde_json::to_vec(request)?);

        let tx = self.connection_mut().transaction()?;
        let existing: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT request_hash, status, receipt_json FROM game_action_ledger
                  WHERE episode_id = ?1 AND request_id = ?2",
                params![episode_id, request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        if let Some((existing_hash, status, receipt_json)) = existing {
            if existing_hash != hash.to_string() {
                tx.commit()?;
                return Ok(LedgerDecision::Conflict {
                    reason: format!(
                        "请求 {request_id} 此前登记的载荷哈希为 {existing_hash}，本次为 {hash}"
                    ),
                });
            }
            // 状态本身不参与判定，但必须先证明它是合法取值，否则账上已经有坏数据。
            parse_ledger_status(&status)?;
            if let Some(text) = receipt_json {
                let receipt: GameActionReceipt = serde_json::from_str(&text)?;
                tx.commit()?;
                return Ok(LedgerDecision::Replay(Box::new(receipt)));
            }
            tx.commit()?;
            return Ok(LedgerDecision::Pending);
        }

        tx.execute(
            "INSERT INTO game_action_ledger (
                 episode_id, request_id, request_hash, expected_observation_id,
                 topology_epoch, status, recorded_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
            params![
                episode_id,
                request_id,
                hash.to_string(),
                request.expected_observation_id.to_string(),
                request.topology_epoch as i64,
                at.to_string(),
            ],
        )?;
        tx.commit()?;
        Ok(LedgerDecision::Fresh)
    }

    /// 结算一次游戏请求。
    ///
    /// 只有 `PENDING` 可以被结算，因此同一次世界步不可能被结算两次。
    pub fn settle_game_request(
        &mut self,
        episode_id: &PublicId,
        request_id: &PublicId,
        status: GameLedgerStatus,
        result_observation_id: Option<&PublicId>,
        receipt: Option<&GameActionReceipt>,
        at: WallClock,
    ) -> Result<(), StorageError> {
        if status == GameLedgerStatus::Pending {
            return Err(StorageError::IllegalActionTransition {
                action_id: request_id.to_string(),
                state: GameLedgerStatus::Pending.as_str().to_string(),
                operation: "用 PENDING 结算请求（结算必须是终态）",
            });
        }

        let tx = self.connection_mut().transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM game_action_ledger
                  WHERE episode_id = ?1 AND request_id = ?2",
                params![episode_id.to_string(), request_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StorageError::GameRequestNotFound {
                episode_id: episode_id.to_string(),
                request_id: request_id.to_string(),
            });
        };
        let current = parse_ledger_status(&current)?;
        if current != GameLedgerStatus::Pending {
            return Err(StorageError::IllegalActionTransition {
                action_id: request_id.to_string(),
                state: current.as_str().to_string(),
                operation: "重复结算游戏请求",
            });
        }

        tx.execute(
            "UPDATE game_action_ledger
                SET status = ?3, result_observation_id = ?4, receipt_json = ?5, settled_at_utc = ?6
              WHERE episode_id = ?1 AND request_id = ?2",
            params![
                episode_id.to_string(),
                request_id.to_string(),
                status.as_str(),
                result_observation_id.map(ToString::to_string),
                match receipt {
                    Some(value) => Some(serde_json::to_string(value)?),
                    None => None,
                },
                at.to_string(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 读取一条游戏请求记录。
    pub fn game_request(
        &self,
        episode_id: &PublicId,
        request_id: &PublicId,
    ) -> Result<Option<GameLedgerEntry>, StorageError> {
        let row = self
            .connection()
            .query_row(
                "SELECT request_hash, expected_observation_id, topology_epoch, status,
                        result_observation_id, receipt_json, recorded_at_utc, settled_at_utc
                   FROM game_action_ledger WHERE episode_id = ?1 AND request_id = ?2",
                params![episode_id.to_string(), request_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;

        let Some((
            hash,
            expected,
            epoch,
            status,
            result,
            receipt,
            recorded_at,
            settled_at,
        )) = row
        else {
            return Ok(None);
        };

        Ok(Some(GameLedgerEntry {
            episode_id: episode_id.clone(),
            request_id: request_id.clone(),
            request_hash: Sha256Hex::parse(hash)?,
            expected_observation_id: PublicId::new(expected)?,
            topology_epoch: u64::try_from(epoch).unwrap_or(0),
            status: parse_ledger_status(&status)?,
            result_observation_id: match result {
                Some(text) => Some(PublicId::new(text)?),
                None => None,
            },
            receipt: match receipt {
                Some(text) => Some(serde_json::from_str(&text)?),
                None => None,
            },
            recorded_at: WallClock::from_rfc3339(&recorded_at)?,
            settled_at: match settled_at {
                Some(text) => Some(WallClock::from_rfc3339(&text)?),
                None => None,
            },
        }))
    }

    /// 列出未结算的请求。恢复流程据此判定 `unknown_commit`（§13）。
    pub fn unsettled_game_requests(
        &self,
        episode_id: &PublicId,
        limit: usize,
    ) -> Result<Vec<GameLedgerEntry>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = {
            let mut stmt = self.connection().prepare(
                "SELECT request_id FROM game_action_ledger
                  WHERE episode_id = ?1 AND status = 'pending'
                  ORDER BY recorded_at_utc LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                params![episode_id.to_string(), limit as i64],
                |row| row.get::<_, String>(0),
            )?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };

        let mut entries = Vec::new();
        for id in ids {
            if let Some(entry) = self.game_request(episode_id, &PublicId::new(id)?)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// 记录总数，用于对账。
    pub fn game_ledger_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM game_action_ledger", [], |row| {
                row.get(0)
            })?;
        Ok(count)
    }
}

fn parse_ledger_status(text: &str) -> Result<GameLedgerStatus, StorageError> {
    match text {
        "pending" => Ok(GameLedgerStatus::Pending),
        "applied" => Ok(GameLedgerStatus::Applied),
        "no_change" => Ok(GameLedgerStatus::NoChange),
        "rejected" => Ok(GameLedgerStatus::Rejected),
        "unknown_commit" => Ok(GameLedgerStatus::UnknownCommit),
        _ => Err(StorageError::CorruptRow {
            field: "game_action_ledger.status",
        }),
    }
}
