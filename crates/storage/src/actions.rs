//! 动作账与 outbox（§7.3、§12.2）。
//!
//! 本模块实现 §7.3 的三句话，其余都是它们的推论：
//!
//! 1. **"ActionIntent、状态更新和待发送记录在一次事务中写入 outbox"** —— 见
//!    [`Store::admit_action`]。事务失败即整体回滚，磁盘满、审计写失败、策略不可读都会
//!    让动作根本没有进入 outbox，符合 §12.2 的失败关闭要求。
//! 2. **"执行代理以动作 ID 去重"** —— 动作 ID 一旦绑定过某个参数摘要就不能改绑
//!    （[`StorageError::ActionIdReused`]）。重复受理同一动作是幂等的，不会产生第二条
//!    outbox 记录。
//! 3. **"若进程在副作用后、回执前崩溃，状态为 `UNKNOWN_COMMIT`；恢复时先查询目标状态，
//!    不能直接重发"** —— 见 [`Store::recover`]。恢复**只产出待核对的清单**，绝不自动重投；
//!    结清要走显式的 [`Store::resolve_unknown_commit`]。
//!
//! 另外两条同样重要的边界：
//!
//! * 许可的 `max_uses` 计数在事务内完成。因为 [`Store`] 是单写者，检查与扣减之间不存在
//!   并发窗口——这正是 §9.3 坚持"单机一个逻辑写入者"的收益之一。
//! * 回执一经写入不可覆盖，且**不改变**后置条件的验证状态（§7.2）。`receipts` 与
//!   `outcomes` 是两张表。

use rusqlite::{params, OptionalExtension};
use soca_contracts::{
    ActionIntent, ActionReceipt, CommitStatus, ExecutionPermit, OutcomeVerified, TaskId, WallClock,
};

use crate::audit::{record_in, AuditCategory};
use crate::error::StorageError;
use crate::predictions::load_prediction;
use crate::Store;

/// 动作在账上的状态（§7.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionState {
    /// 意图已写入 outbox，尚未交给执行代理。可以安全（重新）投递。
    Prepared,
    /// 已交给执行代理。副作用是否发生未知。
    Submitted,
    /// 执行代理报告完成。不等于后置条件已被验证。
    Completed,
    /// 执行代理报告失败。
    Failed,
    /// 恢复核对确认副作用未发生，目标未被改动，调度器可以重新决策。
    Aborted,
    /// 在执行后、回执前崩溃，且恢复时未能确认目标状态。
    UnknownCommit,
    /// 许可校验未通过，从未进入 outbox。
    Denied,
}

impl ActionState {
    /// 写入数据库的标签。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "PREPARED",
            Self::Submitted => "SUBMITTED",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Aborted => "ABORTED",
            Self::UnknownCommit => "UNKNOWN_COMMIT",
            Self::Denied => "DENIED",
        }
    }

    /// 从数据库标签解析。
    pub fn parse(text: &str) -> Result<Self, StorageError> {
        match text {
            "PREPARED" => Ok(Self::Prepared),
            "SUBMITTED" => Ok(Self::Submitted),
            "COMPLETED" => Ok(Self::Completed),
            "FAILED" => Ok(Self::Failed),
            "ABORTED" => Ok(Self::Aborted),
            "UNKNOWN_COMMIT" => Ok(Self::UnknownCommit),
            "DENIED" => Ok(Self::Denied),
            _ => Err(StorageError::CorruptRow {
                field: "actions.state",
            }),
        }
    }

    /// 是否已经终结，不会再自动投递。
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Aborted | Self::UnknownCommit | Self::Denied
        )
    }
}

/// 受理判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 许可校验通过。
    Allowed,
    /// 许可校验未通过。
    Denied,
}

impl Decision {
    /// 写入数据库的标签。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "ALLOWED",
            Self::Denied => "DENIED",
        }
    }

    fn parse(text: &str) -> Result<Self, StorageError> {
        match text {
            "ALLOWED" => Ok(Self::Allowed),
            "DENIED" => Ok(Self::Denied),
            _ => Err(StorageError::CorruptRow {
                field: "actions.decision",
            }),
        }
    }
}

/// 受理结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    /// 动作标识。
    pub action_id: String,
    /// 是否放行。
    pub decision: Decision,
    /// 受理后的状态。
    pub state: ActionState,
    /// 该许可累计已使用次数。
    pub permit_uses_after: u8,
    /// 拒绝原因。放行时为 `None`。
    pub denial_reason: Option<String>,
    /// 本次调用是否命中既有记录、没有写入新内容（§7.3 幂等）。
    pub replayed: bool,
}

impl Admission {
    /// 是否已进入 outbox 等待投递。
    pub fn is_dispatchable(&self) -> bool {
        self.decision == Decision::Allowed && self.state == ActionState::Prepared
    }
}

/// 待投递的 outbox 记录。
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxItem {
    /// 动作标识。
    pub action_id: String,
    /// 完整意图。
    pub intent: ActionIntent,
    /// 写入 outbox 的时刻。
    pub created_at: WallClock,
}

/// 账上的一条动作记录。
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRecord {
    /// 动作标识。
    pub action_id: String,
    /// 所属任务。
    pub task_id: String,
    /// 提出意图的单元。
    pub unit_id: String,
    /// 工具。
    pub tool_id: String,
    /// 对象范围。
    pub object_scope: String,
    /// 参数摘要。
    pub parameters_digest: String,
    /// 风险等级。
    pub action_level: String,
    /// 动作前预测引用。
    pub prediction_ref: String,
    /// 完整意图。
    pub intent: ActionIntent,
    /// 签发并绑定到本动作的许可。
    pub permit_id: Option<String>,
    /// 当前状态。
    pub state: ActionState,
    /// 受理判定。
    pub decision: Decision,
    /// 拒绝原因。
    pub denial_reason: Option<String>,
    /// 该许可累计使用次数。
    pub uses_so_far: u8,
    /// 创建时刻。
    pub created_at: WallClock,
    /// 最近更新时刻。
    pub updated_at: WallClock,
}

/// 一个已投递、但结局未知的动作。
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownCommit {
    /// 动作标识。
    pub action_id: String,
    /// 工具。恢复任务据此决定去哪里核对目标状态。
    pub tool_id: String,
    /// 对象范围。
    pub object_scope: String,
    /// 完整意图，供恢复任务构造查询。
    pub intent: ActionIntent,
    /// 投递时刻。
    pub dispatched_at: WallClock,
}

/// 崩溃恢复结果。
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryReport {
    /// 已投递但无回执的动作。**不得重发**，只能核对目标状态。
    pub unknown_commits: Vec<UnknownCommit>,
    /// 从未投递的动作。可以安全重新投递。
    pub resendable: Vec<OutboxItem>,
    /// 已经终结的动作数。
    pub settled: usize,
}

impl RecoveryReport {
    /// 是否存在需要人工或显式恢复任务介入的未知提交。
    pub fn needs_recovery_task(&self) -> bool {
        !self.unknown_commits.is_empty()
    }
}

/// 未知提交的核对结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// 核对确认副作用已经发生。
    ConfirmedCompleted,
    /// 核对确认副作用没有发生，目标未被改动。
    ConfirmedAbsent,
}

impl Store {
    /// 受理一个动作意图。
    ///
    /// `task_id` 由调用方提供而不是从意图里取：§7.2 列举的 `ActionIntent` 字段里没有任务，
    /// 任务归属来自当前工作上下文。这里不替调用方猜。
    pub fn admit_action(
        &mut self,
        task_id: &TaskId,
        intent: &ActionIntent,
        permit: &ExecutionPermit,
        at: WallClock,
    ) -> Result<Admission, StorageError> {
        let action_id = intent.action_id.to_string();
        let incoming_digest = intent.parameters_digest().to_string();
        let tx = self.connection_mut().transaction()?;

        // 幂等：同一动作 ID 重复受理不再写入。
        if let Some(existing) = load_action(&tx, &action_id)? {
            if existing.parameters_digest != incoming_digest {
                return Err(StorageError::ActionIdReused {
                    action_id,
                    existing_digest: existing.parameters_digest,
                    incoming_digest,
                });
            }
            let admission = Admission {
                action_id: existing.action_id,
                decision: existing.decision,
                state: existing.state,
                permit_uses_after: existing.uses_so_far,
                denial_reason: existing.denial_reason,
                replayed: true,
            };
            tx.commit()?;
            return Ok(admission);
        }

        let uses_so_far = permit_uses(&tx, permit.permit_id.as_str())?;

        // 放行的两个必要条件，缺一不可：
        //   1. 许可确实授权这次具体动作（§12.2）；
        //   2. 动作前预测已经落库，且属于同一任务（§6.3，以及 §17 验收的"预测先于动作"）。
        // 任一不满足都走同一条拒绝路径，都留痕（§6.8：否决保留在最小审计账中）。
        let denial = match permit.authorizes(intent, at, uses_so_far) {
            Err(reason) => Some(reason.to_string()),
            Ok(()) => {
                let reference = intent.prediction_ref.to_string();
                match load_prediction(&tx, &reference)? {
                    None => Some(format!("动作前预测 {reference} 未落库，拒绝受理")),
                    Some(record) if record.task_id != task_id.to_string() => Some(format!(
                        "预测 {reference} 属于任务 {}，与本次受理的任务 {task_id} 不一致",
                        record.task_id
                    )),
                    Some(_) => None,
                }
            }
        };

        if let Some(detail) = denial {
            tx.execute(
                "INSERT INTO actions (
                     action_id, task_id, unit_id, tool_id, object_scope, parameters_digest,
                     action_level, prediction_ref, intent_json, permit_id, state, decision,
                     denial_reason, uses_so_far, created_at_utc, updated_at_utc
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'DENIED', 'DENIED',
                           ?11, ?12, ?13, ?13)",
                params![
                    action_id,
                    task_id.to_string(),
                    intent.proposed_by.to_string(),
                    intent.tool_id.to_string(),
                    intent.object_scope.to_string(),
                    incoming_digest,
                    intent.risk.as_str(),
                    intent.prediction_ref.to_string(),
                    serde_json::to_string(intent)?,
                    permit.permit_id.to_string(),
                    detail,
                    i64::from(uses_so_far),
                    at.to_string(),
                ],
            )?;
            record_in(
                &tx,
                at,
                AuditCategory::ActionDenied,
                &action_id,
                "denied",
                &format!("tool={} 原因={}", intent.tool_id, detail),
            )?;
            tx.commit()?;
            return Ok(Admission {
                action_id,
                decision: Decision::Denied,
                state: ActionState::Denied,
                permit_uses_after: uses_so_far,
                denial_reason: Some(detail),
                replayed: false,
            });
        }

        // 放行：意图、许可消耗与 outbox 记录在同一事务内落库（§7.3）。
        tx.execute(
            "INSERT INTO actions (
                 action_id, task_id, unit_id, tool_id, object_scope, parameters_digest,
                 action_level, prediction_ref, intent_json, permit_id, state, decision,
                 denial_reason, uses_so_far, created_at_utc, updated_at_utc
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'PREPARED', 'ALLOWED',
                       NULL, ?11, ?12, ?12)",
            params![
                action_id,
                task_id.to_string(),
                intent.proposed_by.to_string(),
                intent.tool_id.to_string(),
                intent.object_scope.to_string(),
                incoming_digest,
                intent.risk.as_str(),
                intent.prediction_ref.to_string(),
                serde_json::to_string(intent)?,
                permit.permit_id.to_string(),
                i64::from(uses_so_far + 1),
                at.to_string(),
            ],
        )?;
        tx.execute(
            "INSERT INTO permit_uses (permit_id, action_id, used_at_utc) VALUES (?1, ?2, ?3)",
            params![permit.permit_id.to_string(), action_id, at.to_string()],
        )?;
        tx.execute(
            "INSERT INTO outbox (action_id, payload_json, created_at_utc)
             VALUES (?1, ?2, ?3)",
            params![action_id, serde_json::to_string(intent)?, at.to_string()],
        )?;
        record_in(
            &tx,
            at,
            AuditCategory::ActionAdmitted,
            &action_id,
            "allowed",
            &format!(
                "tool={} level={} permit={}",
                intent.tool_id,
                intent.risk.as_str(),
                permit.permit_id
            ),
        )?;
        tx.commit()?;

        Ok(Admission {
            action_id,
            decision: Decision::Allowed,
            state: ActionState::Prepared,
            permit_uses_after: uses_so_far + 1,
            denial_reason: None,
            replayed: false,
        })
    }

    /// 标记动作已交给执行代理。
    ///
    /// 这一步是**分水岭**：在此之前崩溃，`outbox.dispatched_at_utc` 为空，重投是安全的；
    /// 在此之后崩溃，副作用可能已经发生，恢复只能核对。
    pub fn mark_dispatched(&mut self, action_id: &str, at: WallClock) -> Result<(), StorageError> {
        let tx = self.connection_mut().transaction()?;
        let action = require_action(&tx, action_id)?;
        if action.state == ActionState::Submitted {
            // 幂等重放。
            tx.commit()?;
            return Ok(());
        }
        if action.state != ActionState::Prepared {
            return Err(StorageError::IllegalActionTransition {
                action_id: action_id.to_string(),
                state: action.state.as_str().to_string(),
                operation: "标记已投递",
            });
        }
        tx.execute(
            "UPDATE actions SET state = 'SUBMITTED', updated_at_utc = ?2 WHERE action_id = ?1",
            params![action_id, at.to_string()],
        )?;
        tx.execute(
            "UPDATE outbox SET dispatched_at_utc = ?2
              WHERE action_id = ?1 AND dispatched_at_utc IS NULL",
            params![action_id, at.to_string()],
        )?;
        record_in(
            &tx,
            at,
            AuditCategory::ActionDispatched,
            action_id,
            "dispatched",
            &format!("tool={}", action.tool_id),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 记录执行回执。
    ///
    /// 回执不可覆盖：同一动作的第二份不同状态回执会被拒绝。这与"回执不等于验证"是两件事，
    /// 后者由 [`Store::record_outcome`] 单独承担。
    pub fn settle_receipt(
        &mut self,
        receipt: &ActionReceipt,
        at: WallClock,
    ) -> Result<(), StorageError> {
        let action_id = receipt.action_id.to_string();
        let tx = self.connection_mut().transaction()?;
        let action = require_action(&tx, &action_id)?;

        if let Some(existing_permit) = &action.permit_id
            && existing_permit.as_str() != receipt.permit_id.as_str()
        {
            return Err(StorageError::ReceiptPermitMismatch {
                action_id,
                expected: existing_permit.clone(),
                actual: receipt.permit_id.to_string(),
            });
        }

        if let Some(existing) = load_receipt(&tx, &action_id)? {
            if existing.status == receipt.status {
                // 重复投递的同一回执：幂等。
                tx.commit()?;
                return Ok(());
            }
            return Err(StorageError::IllegalActionTransition {
                action_id,
                state: action.state.as_str().to_string(),
                operation: "用不同状态的回执覆盖既有回执",
            });
        }

        if !matches!(action.state, ActionState::Submitted | ActionState::UnknownCommit) {
            return Err(StorageError::IllegalActionTransition {
                action_id,
                state: action.state.as_str().to_string(),
                operation: "在未投递的动作上记录执行回执",
            });
        }

        tx.execute(
            "INSERT INTO receipts (
                 action_id, permit_id, status, recorded_at_utc, detail,
                 observed_target_version, receipt_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                action_id,
                receipt.permit_id.to_string(),
                commit_status_str(receipt.status),
                receipt.recorded_at.to_string(),
                receipt.detail,
                receipt.observed_target_version,
                serde_json::to_string(receipt)?,
            ],
        )?;

        // 回执说"已提交"并不终结动作；只有完成/失败/结果未知才改变状态。
        let next_state = match receipt.status {
            CommitStatus::Submitted => None,
            CommitStatus::Completed => Some(ActionState::Completed),
            CommitStatus::Failed => Some(ActionState::Failed),
            CommitStatus::UnknownCommit => Some(ActionState::UnknownCommit),
        };

        if let Some(state) = next_state {
            tx.execute(
                "UPDATE actions SET state = ?2, updated_at_utc = ?3 WHERE action_id = ?1",
                params![action_id, state.as_str(), at.to_string()],
            )?;
            tx.execute(
                "UPDATE outbox SET settled_at_utc = ?2 WHERE action_id = ?1",
                params![action_id, at.to_string()],
            )?;
        }

        record_in(
            &tx,
            at,
            AuditCategory::ReceiptRecorded,
            &action_id,
            commit_status_str(receipt.status),
            &format!("动作状态={}", next_state.map_or("不变", ActionState::as_str)),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 记录后置条件判定（§7.2）。
    ///
    /// 本操作**不改变动作状态、不追加权限**。它需要一条既有的回执：没有执行就没有可验证的
    /// 后置条件。
    pub fn record_outcome(
        &mut self,
        outcome: &OutcomeVerified,
        at: WallClock,
    ) -> Result<(), StorageError> {
        let action_id = outcome.action_id.to_string();
        let tx = self.connection_mut().transaction()?;
        let action = require_action(&tx, &action_id)?;

        if load_receipt(&tx, &action_id)?.is_none() {
            return Err(StorageError::IllegalActionTransition {
                action_id,
                state: action.state.as_str().to_string(),
                operation: "在缺少执行回执的动作上记录后置条件判定",
            });
        }

        tx.execute(
            "INSERT INTO outcomes (action_id, prediction_ref, verdict, verified_at_utc, outcome_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(action_id) DO UPDATE SET
                 prediction_ref = excluded.prediction_ref,
                 verdict        = excluded.verdict,
                 verified_at_utc = excluded.verified_at_utc,
                 outcome_json   = excluded.outcome_json",
            params![
                action_id,
                outcome.prediction_ref.to_string(),
                verdict_str(outcome.verdict),
                at.to_string(),
                serde_json::to_string(outcome)?,
            ],
        )?;
        record_in(
            &tx,
            at,
            AuditCategory::OutcomeRecorded,
            &action_id,
            verdict_str(outcome.verdict),
            &format!("观测数={}", outcome.observation_refs.len()),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 列出尚未投递、且仍处于 `PREPARED` 的 outbox 记录。
    ///
    /// 返回即代表"可以投递"，不代表"已经投递过"。投递者必须在真正交接后调用
    /// [`Store::mark_dispatched`]。
    pub fn pending_outbox(&self, limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.connection().prepare(
            "SELECT o.action_id, o.payload_json, o.created_at_utc
               FROM outbox o
               JOIN actions a ON a.action_id = o.action_id
              WHERE o.dispatched_at_utc IS NULL
                AND a.state = 'PREPARED'
              ORDER BY o.outbox_id
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut items = Vec::new();
        for row in rows {
            let (action_id, payload_json, created_at) = row?;
            items.push(OutboxItem {
                action_id,
                intent: serde_json::from_str(&payload_json)?,
                created_at: WallClock::from_rfc3339(&created_at)?,
            });
        }
        Ok(items)
    }

    /// 崩溃恢复。
    ///
    /// §7.3 原文："恢复时先查询目标状态，不能直接重发邮件、重复付款或再次覆盖。"
    /// 因此本方法只做两件事：把"已投递但无回执"的动作标成 `UNKNOWN_COMMIT` 并加入待核对清单，
    /// 以及把"从未投递"的动作标成可重投。**它不会调用任何执行代理。**
    pub fn recover(&mut self, at: WallClock) -> Result<RecoveryReport, StorageError> {
        let tx = self.connection_mut().transaction()?;

        let unknown_commits = {
            let mut stmt = tx.prepare(
                "SELECT a.action_id, a.tool_id, a.object_scope, a.intent_json, o.dispatched_at_utc
                   FROM actions a
                   JOIN outbox o ON o.action_id = a.action_id
                  WHERE a.state = 'SUBMITTED'
                  ORDER BY a.action_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut found = Vec::new();
            for row in rows {
                let (action_id, tool_id, object_scope, intent_json, dispatched_at) = row?;
                found.push(UnknownCommit {
                    action_id,
                    tool_id,
                    object_scope,
                    intent: serde_json::from_str(&intent_json)?,
                    dispatched_at: WallClock::from_rfc3339(&dispatched_at)?,
                });
            }
            found
        };

        for item in &unknown_commits {
            tx.execute(
                "UPDATE actions SET state = 'UNKNOWN_COMMIT', updated_at_utc = ?2
                  WHERE action_id = ?1",
                params![item.action_id, at.to_string()],
            )?;
            record_in(
                &tx,
                at,
                AuditCategory::UnknownCommitDetected,
                &item.action_id,
                "unknown_commit",
                &format!("tool={} 已投递未回执，需核对目标状态", item.tool_id),
            )?;
        }

        let resendable = {
            let mut stmt = tx.prepare(
                "SELECT o.action_id, o.payload_json, o.created_at_utc
                   FROM outbox o
                   JOIN actions a ON a.action_id = o.action_id
                  WHERE o.dispatched_at_utc IS NULL
                    AND a.state = 'PREPARED'
                  ORDER BY o.outbox_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut items = Vec::new();
            for row in rows {
                let (action_id, payload_json, created_at) = row?;
                items.push(OutboxItem {
                    action_id,
                    intent: serde_json::from_str(&payload_json)?,
                    created_at: WallClock::from_rfc3339(&created_at)?,
                });
            }
            items
        };

        let settled: i64 = tx.query_row(
            "SELECT COUNT(*) FROM actions
              WHERE state IN ('COMPLETED', 'FAILED', 'ABORTED', 'DENIED')",
            [],
            |row| row.get(0),
        )?;

        tx.commit()?;
        Ok(RecoveryReport {
            unknown_commits,
            resendable,
            settled: settled as usize,
        })
    }

    /// 结清一个未知提交。用于显式恢复任务核对目标状态之后。
    pub fn resolve_unknown_commit(
        &mut self,
        action_id: &str,
        resolution: Resolution,
        at: WallClock,
    ) -> Result<ActionState, StorageError> {
        let tx = self.connection_mut().transaction()?;
        let action = require_action(&tx, action_id)?;
        if action.state != ActionState::UnknownCommit {
            return Err(StorageError::IllegalActionTransition {
                action_id: action_id.to_string(),
                state: action.state.as_str().to_string(),
                operation: "结清未知提交",
            });
        }

        let next = match resolution {
            Resolution::ConfirmedCompleted => ActionState::Completed,
            Resolution::ConfirmedAbsent => ActionState::Aborted,
        };
        tx.execute(
            "UPDATE actions SET state = ?2, updated_at_utc = ?3 WHERE action_id = ?1",
            params![action_id, next.as_str(), at.to_string()],
        )?;
        tx.execute(
            "UPDATE outbox SET settled_at_utc = ?2 WHERE action_id = ?1",
            params![action_id, at.to_string()],
        )?;
        record_in(
            &tx,
            at,
            AuditCategory::UnknownCommitResolved,
            action_id,
            next.as_str(),
            "恢复核对结论",
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// 读取一条动作记录。
    pub fn action(&self, action_id: &str) -> Result<Option<ActionRecord>, StorageError> {
        load_action(self.connection(), action_id)
    }

    /// 读取一条执行回执。
    pub fn receipt(&self, action_id: &str) -> Result<Option<ActionReceipt>, StorageError> {
        load_receipt(self.connection(), action_id)
    }

    /// 读取一条后置条件判定。
    pub fn outcome(&self, action_id: &str) -> Result<Option<OutcomeVerified>, StorageError> {
        let json: Option<String> = self
            .connection()
            .query_row(
                "SELECT outcome_json FROM outcomes WHERE action_id = ?1",
                params![action_id],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => Ok(Some(serde_json::from_str(&text)?)),
            None => Ok(None),
        }
    }

    /// 动作总数。
    pub fn action_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM actions", [], |row| row.get(0))?;
        Ok(count)
    }

    /// outbox 记录总数。
    pub fn outbox_count(&self) -> Result<i64, StorageError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?;
        Ok(count)
    }

    /// 某个许可累计已使用次数。
    pub fn permit_uses(&self, permit_id: &str) -> Result<u8, StorageError> {
        permit_uses(self.connection(), permit_id)
    }
}

// ---------------------------------------------------------------------------
// 内部读取辅助
// ---------------------------------------------------------------------------

fn permit_uses(conn: &rusqlite::Connection, permit_id: &str) -> Result<u8, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM permit_uses WHERE permit_id = ?1",
        params![permit_id],
        |row| row.get(0),
    )?;
    Ok(u8::try_from(count).unwrap_or(u8::MAX))
}

fn require_action(
    conn: &rusqlite::Connection,
    action_id: &str,
) -> Result<ActionRecord, StorageError> {
    load_action(conn, action_id)?.ok_or_else(|| StorageError::ActionNotFound {
        action_id: action_id.to_string(),
    })
}

fn load_action(
    conn: &rusqlite::Connection,
    action_id: &str,
) -> Result<Option<ActionRecord>, StorageError> {
    let row = conn
        .query_row(
            "SELECT action_id, task_id, unit_id, tool_id, object_scope, parameters_digest,
                    action_level, prediction_ref, intent_json, permit_id, state, decision,
                    denial_reason, uses_so_far, created_at_utc, updated_at_utc
               FROM actions WHERE action_id = ?1",
            params![action_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, String>(15)?,
                ))
            },
        )
        .optional()?;

    let Some((
        action_id,
        task_id,
        unit_id,
        tool_id,
        object_scope,
        parameters_digest,
        action_level,
        prediction_ref,
        intent_json,
        permit_id,
        state,
        decision,
        denial_reason,
        uses_so_far,
        created_at,
        updated_at,
    )) = row
    else {
        return Ok(None);
    };

    Ok(Some(ActionRecord {
        action_id,
        task_id,
        unit_id,
        tool_id,
        object_scope,
        parameters_digest,
        action_level,
        prediction_ref,
        intent: serde_json::from_str(&intent_json)?,
        permit_id,
        state: ActionState::parse(&state)?,
        decision: Decision::parse(&decision)?,
        denial_reason,
        uses_so_far: u8::try_from(uses_so_far).unwrap_or(u8::MAX),
        created_at: WallClock::from_rfc3339(&created_at)?,
        updated_at: WallClock::from_rfc3339(&updated_at)?,
    }))
}

fn load_receipt(
    conn: &rusqlite::Connection,
    action_id: &str,
) -> Result<Option<ActionReceipt>, StorageError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT receipt_json FROM receipts WHERE action_id = ?1",
            params![action_id],
            |row| row.get(0),
        )
        .optional()?;
    match json {
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
        None => Ok(None),
    }
}

fn commit_status_str(status: CommitStatus) -> &'static str {
    match status {
        CommitStatus::Submitted => "SUBMITTED",
        CommitStatus::Completed => "COMPLETED",
        CommitStatus::Failed => "FAILED",
        CommitStatus::UnknownCommit => "UNKNOWN_COMMIT",
    }
}

fn verdict_str(verdict: soca_contracts::Verdict) -> &'static str {
    match verdict {
        soca_contracts::Verdict::Supported => "SUPPORTED",
        soca_contracts::Verdict::Refuted => "REFUTED",
        soca_contracts::Verdict::Inconclusive => "INCONCLUSIVE",
    }
}
