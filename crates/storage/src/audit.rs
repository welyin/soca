//! 最小审计账（§6.8、§12.3、§14）。
//!
//! §6.8 要求"错误的引用、失败、超时、否决均保留在最小审计账中"。§12.3 给出保留边界：
//! 初值 30 天的最小元数据，**不保存密码、完整 prompt 或无限个人内容**。
//!
//! 因此本模块的写入接口只接受短标签与短说明，并且调用方无法塞进任意大文本：单条说明超过
//! [`MAX_DETAIL_LEN`] 会被拒绝，逼迫调用方想清楚要留什么，而不是把上下文整段倒进审计表。

use rusqlite::params;
use soca_contracts::WallClock;

use crate::error::StorageError;
use crate::Store;

/// 单条审计说明的字节上限。
///
/// 取 512 字节：足够写清"哪个动作被谁拒绝、为什么"，不足以塞进一整段对话或一页 prompt。
pub const MAX_DETAIL_LEN: usize = 512;

/// 审计类别。用枚举而不是自由字符串，避免审计表被"随手新增的标签"稀释成一堆无意义行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditCategory {
    /// 事件被拒绝（契约校验失败等）。
    EventRejected,
    /// 动作被受理。
    ActionAdmitted,
    /// 动作被拒绝。§6.8 明确要求留存。
    ActionDenied,
    /// 动作已投递执行代理。
    ActionDispatched,
    /// 收到执行回执。
    ReceiptRecorded,
    /// 收到后置条件判定。
    OutcomeRecorded,
    /// 恢复流程发现未知提交。
    UnknownCommitDetected,
    /// 恢复流程结清了未知提交。
    UnknownCommitResolved,
    /// 单元生命周期变化。
    UnitTransition,
    /// 签发了执行许可。
    PermitIssued,
    /// 许可被拒绝。§6.8 与 [`AuditCategory::ActionDenied`] 同一要求，但发生在更早的一环：
    /// 动作根本没被受理，因为策略代理判定不该签发许可。
    PermitRefused,
    /// 收到一次人工批准（§12.1）。
    ApprovalGranted,
    /// 策略代理判定需要人工批准，目标转入等待状态（§12.1）。
    ///
    /// 与 [`AuditCategory::PermitRefused`] 分开记录，是因为它们对"接下来该做什么"的含义
    /// 完全不同：一个是等外部输入，另一个是此路不通。
    ApprovalRequired,
    /// 授予了一次能力授权（§12.1）。
    ///
    /// 与 [`AuditCategory::CapabilityRevoked`] 成对：只记撤回不记授予的话，审计只能回答
    /// "什么时候收回的"，答不出"当初是谁、什么时候给的"——而后者才是追溯的起点。
    CapabilityGranted,
    /// 撤回了一次能力授权（§12.1："范围限定授权，**撤回立即生效**"）。
    ///
    /// 与 [`AuditCategory::RetentionEnforced`] 分开记录：撤回是一次**决定**，而保留期是一次
    /// 到期。前者要回答"是谁在什么时候收回的"，后者只需要回答"清掉了多少"。
    CapabilityRevoked,
    /// 一次因为授权已撤回而被拒绝的观测或许可（§12.1）。
    CapabilityDenied,
    /// 执行了一次保留期动作：隐藏超期记忆、清理已隐藏内容、或裁掉超期审计（§12.3）。
    ///
    /// 详情里只写**条数**，不写内容。从保留期里删掉一条记忆，却把它的内容抄进审计账，
    /// 等于绕了一圈又存了一份——而 §12.3 对审计账的要求恰恰是"不保存密码、完整 prompt
    /// 或无限个人内容"。
    RetentionEnforced,
}

impl AuditCategory {
    /// 写入数据库的短标签。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EventRejected => "event_rejected",
            Self::ActionAdmitted => "action_admitted",
            Self::ActionDenied => "action_denied",
            Self::ActionDispatched => "action_dispatched",
            Self::ReceiptRecorded => "receipt_recorded",
            Self::OutcomeRecorded => "outcome_recorded",
            Self::UnknownCommitDetected => "unknown_commit_detected",
            Self::UnknownCommitResolved => "unknown_commit_resolved",
            Self::UnitTransition => "unit_transition",
            Self::PermitIssued => "permit_issued",
            Self::PermitRefused => "permit_refused",
            Self::ApprovalGranted => "approval_granted",
            Self::ApprovalRequired => "approval_required",
            Self::CapabilityGranted => "capability_granted",
            Self::CapabilityRevoked => "capability_revoked",
            Self::CapabilityDenied => "capability_denied",
            Self::RetentionEnforced => "retention_enforced",
        }
    }
}

/// 读回的一条审计记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// 写入时刻。
    pub at: WallClock,
    /// 类别标签。
    pub category: String,
    /// 被审计对象的引用（动作 ID、事件 ID、单元 ID 等）。
    pub subject_ref: String,
    /// 结果短标签。
    pub outcome: String,
    /// 简短说明。不会包含密钥、完整 prompt 或长段个人内容。
    pub detail: String,
}

/// 在既有事务里追加一条审计记录。
///
/// 之所以要求调用方传入 `tx`：审计必须和它描述的那个事实在同一个事务里落库，否则会出现
/// "动作已受理但审计没写上"的窗口。
pub(crate) fn record_in(
    tx: &rusqlite::Transaction<'_>,
    at: WallClock,
    category: AuditCategory,
    subject_ref: &str,
    outcome: &str,
    detail: &str,
) -> Result<(), StorageError> {
    let detail = truncate_detail(detail);
    tx.execute(
        "INSERT INTO audit_log (at_utc, category, subject_ref, outcome, detail)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            at.to_string(),
            category.as_str(),
            subject_ref,
            outcome,
            detail
        ],
    )?;
    Ok(())
}

/// 按字节边界安全截断，并标记已截断。
fn truncate_detail(detail: &str) -> String {
    if detail.len() <= MAX_DETAIL_LEN {
        return detail.to_string();
    }
    let mut end = MAX_DETAIL_LEN;
    // 不要把多字节字符切一半。
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[已截断]", &detail[..end])
}

impl Store {
    /// 单独追加一条审计记录，自开事务。
    ///
    /// 与 `record_in` 的分工：那条路径服务于"审计必须和它描述的事实同事务落库"的场合
    /// （受理、投递、回执）。本方法服务的是另一类事实——策略判定、许可拒绝、收到批准——
    /// 它们自己没有别的表要写，因此不需要与谁同事务。
    pub fn audit(
        &mut self,
        at: WallClock,
        category: AuditCategory,
        subject_ref: &str,
        outcome: &str,
        detail: &str,
    ) -> Result<(), StorageError> {
        let tx = self.connection_mut().transaction()?;
        record_in(&tx, at, category, subject_ref, outcome, detail)?;
        tx.commit()?;
        Ok(())
    }

    /// 读取审计记录，按时间升序。`limit` 为 0 时返回空列表。
    pub fn audit_entries(&self, limit: usize) -> Result<Vec<AuditEntry>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.connection().prepare(
            "SELECT at_utc, category, subject_ref, outcome, detail
               FROM audit_log
              ORDER BY audit_id
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut entries = Vec::new();
        for row in rows {
            let (at, category, subject_ref, outcome, detail) = row?;
            entries.push(AuditEntry {
                at: WallClock::from_rfc3339(&at)?,
                category,
                subject_ref,
                outcome,
                detail,
            });
        }
        Ok(entries)
    }

    /// 将审计账裁剪到某个时刻之前的内容。
    ///
    /// §12.3 的默认保留期是 30 天。裁剪是显式动作，不由后台悄悄执行；调用方（资源控制器或
    /// 用户的删除请求）决定何时执行。
    pub fn prune_audit_before(&mut self, cutoff: WallClock) -> Result<usize, StorageError> {
        let removed = self.connection().execute(
            "DELETE FROM audit_log WHERE at_utc < ?1",
            params![cutoff.to_string()],
        )?;
        Ok(removed)
    }
}
