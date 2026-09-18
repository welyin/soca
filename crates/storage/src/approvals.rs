//! 人工批准的持久化（§12.1、§12.2）。
//!
//! 两条在接口层面就成立的性质：
//!
//! 1. **消费是原子的。** [`Store::consume_approval`] 是一条带条件的 `UPDATE ... WHERE
//!    used < max_uses`，受影响行数为零就意味着"用完了"。先读后写会留下一个并发窗口：两次
//!    签发都读到 `used = 0`，于是同一次批准被用掉两次。§12.1 的"每动作人工审批"在那种情况下
//!    就退化成"每个动作类型一次审批"。
//! 2. **计数只以列为准。** `approval_json` 保存的是绑定，写入后不再改动；`used` 会被消费
//!    递增，因此只写在列上。两处都存同一份计数必然分叉，而分叉的方向恰好危险：读的人会以为
//!    还能再用一次。

use rusqlite::{params, Connection, OptionalExtension};
use soca_contracts::{Approval, ApprovalId, SubjectId};

use crate::error::StorageError;
use crate::Store;

/// 读取时统一选出的列：`(approval_json, used)`。
const APPROVAL_COLUMNS: &str = "approval_json, used";

/// 两份批准的**绑定**是否相同。已用次数不参与比较。
///
/// 次数是可变状态，"登记一条批准"与"这条批准的绑定是什么"是两件事。把次数算进等价判断，
/// 会让"重放一条已经用过的批准"被判成"标识被复用"而报错——那会逼调用方为了拿到幂等
/// 去伪造 `used`，而伪造 `used` 正是本模块要挡的那件事。
///
/// 反过来说：调用方传 `used = 0` 想重置计数，得到的是 `Ok(false)`（判为已存在）而不是写入，
/// 存量计数原封不动。这正是要的结果。
fn same_binding(left: &Approval, right: &Approval) -> bool {
    let mut binding = left.clone();
    binding.used = right.used;
    binding == *right
}

/// 把一行原始列拼成批准。
fn assemble(raw: (String, i64)) -> Result<Approval, StorageError> {
    let (json, used) = raw;
    let mut approval: Approval = serde_json::from_str(&json)?;
    approval.used = u8::try_from(used).map_err(|_| StorageError::CorruptRow {
        field: "approvals.used",
    })?;
    Ok(approval)
}

fn load(conn: &Connection, approval_id: &str) -> Result<Option<Approval>, StorageError> {
    let raw = conn
        .query_row(
            &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE approval_id = ?1"),
            params![approval_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    raw.map(assemble).transpose()
}

impl Store {
    /// 记下一次批准。返回 `false` 表示这条批准此前已经记过。
    ///
    /// 同标识但**绑定**不同时拒绝，与记忆条目同一条理由：标识是追溯依据的锚点，复用它会
    /// 伪造一条不存在的批准历史。
    ///
    /// 存量记录的 `used` 不因重复写入而归零。反过来的话，一次重放就能把一次已经用掉的
    /// 批准重新充满——那正是一条绕过审批的路。
    pub fn record_approval(&mut self, approval: &Approval) -> Result<bool, StorageError> {
        if let Some(existing) = load(self.connection(), approval.approval_id.as_str())? {
            if same_binding(&existing, approval) {
                return Ok(false);
            }
            return Err(StorageError::ApprovalAlreadyRecorded {
                approval_id: approval.approval_id.to_string(),
            });
        }

        self.connection().execute(
            "INSERT INTO approvals (
                 approval_id, subject_id, max_action_level, tool_id, object_scope,
                 parameters_digest, granted_at_utc, expires_at_utc, channel,
                 max_uses, used, approval_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                approval.approval_id.to_string(),
                approval.subject_id.to_string(),
                approval.max_action_level.as_str(),
                approval.tool_id.as_ref().map(ToString::to_string),
                approval.object_scope.as_ref().map(ToString::to_string),
                approval.parameters_digest.as_ref().map(ToString::to_string),
                approval.granted_at.to_string(),
                approval.expires_at.as_ref().map(ToString::to_string),
                channel_str(approval),
                approval.max_uses,
                approval.used,
                serde_json::to_string(approval)?,
            ],
        )?;
        Ok(true)
    }

    /// 读一次批准。
    pub fn approval(&self, approval_id: &ApprovalId) -> Result<Option<Approval>, StorageError> {
        load(self.connection(), approval_id.as_str())
    }

    /// 某个主体的全部批准，按签发时刻升序。
    ///
    /// 不过滤过期与已耗尽：判据在 [`soca_contracts::Approval::covers`] 里，把它复制一份到
    /// 查询条件上，两处迟早会分叉。这里只负责"有哪些"。
    pub fn approvals_for(&self, subject_id: &SubjectId) -> Result<Vec<Approval>, StorageError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {APPROVAL_COLUMNS} FROM approvals
              WHERE subject_id = ?1 ORDER BY granted_at_utc, approval_id"
        ))?;
        let rows = statement.query_map(params![subject_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut found = Vec::new();
        for row in rows {
            found.push(assemble(row?)?);
        }
        Ok(found)
    }

    /// 消费一次批准，返回消费之后的已用次数。
    ///
    /// 单条带条件的 `UPDATE`，因此"检查余量"与"扣减"之间没有窗口：受影响行数为零就说明
    /// 这条批准不存在或已经用尽，两者都返回错误。分开成先读后写的话，两次签发可能都读到
    /// 余量为一，于是同一次批准被消费两次。
    pub fn consume_approval(&mut self, approval_id: &ApprovalId) -> Result<u8, StorageError> {
        let changed = self.connection().execute(
            "UPDATE approvals SET used = used + 1
              WHERE approval_id = ?1 AND used < max_uses",
            params![approval_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StorageError::ApprovalNotFound {
                approval_id: approval_id.to_string(),
            });
        }
        let used: i64 = self.connection().query_row(
            "SELECT used FROM approvals WHERE approval_id = ?1",
            params![approval_id.to_string()],
            |row| row.get(0),
        )?;
        u8::try_from(used).map_err(|_| StorageError::CorruptRow {
            field: "approvals.used",
        })
    }

    /// 某个主体当前可用的批准数（未过期且有余量）。
    pub fn usable_approval_count(
        &self,
        subject_id: &SubjectId,
        at: soca_contracts::WallClock,
    ) -> Result<usize, StorageError> {
        Ok(self
            .approvals_for(subject_id)?
            .iter()
            .filter(|approval| {
                approval.remaining() > 0
                    && approval.expires_at.is_none_or(|expires_at| at < expires_at)
            })
            .count())
    }
}

/// 通道的稳定名称。
fn channel_str(approval: &Approval) -> &'static str {
    match approval.channel {
        soca_contracts::UserChannel::Chat => "chat",
        soca_contracts::UserChannel::PushToTalk => "push_to_talk",
        soca_contracts::UserChannel::ApprovalUi => "approval_ui",
        soca_contracts::UserChannel::DeviceControl => "device_control",
    }
}
