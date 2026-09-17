//! SoCA 存储层（实施路线 P0 的第二个工作包）。
//!
//! 职责范围严格限定在架构文档 §9.3 的第一行：
//! **单元目录、目标、权限引用、动作状态、游标、事件元数据 → SQLite WAL，单机一个逻辑
//! 写入者，事务 outbox**。内容仓、模型仓与凭据存储不在本 crate 内。
//!
//! 三条本 crate 负责保证、且有回归测试覆盖的语义：
//!
//! 1. **单写者**：[`Store`] 持有唯一一个 [`rusqlite::Connection`]。`Connection` 是 `Send`
//!    但不是 `Sync`，所以 `Store` 在类型层面就不可能被多线程同时改写。§9.3 明确不把运行中的
//!    SQLite 数据库放到网络共享供多机共同写。
//! 2. **原子出账**：动作意图、状态更新与 outbox 记录写在同一个事务里（§7.3）。
//! 3. **恢复只核对、不重发**：恢复流程把"已投递但无回执"的动作标为 `UNKNOWN_COMMIT`，
//!    另开显式恢复任务；历史重放只读已记录内容，不重新执行外部动作（§7.3）。
//!
//! 本 crate 不做网络、不碰 OS 副作用、不调用模型。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actions;
pub mod audit;
pub mod error;
pub mod events;
pub mod game_ledger;
pub mod goals;
pub mod instances;
pub mod memory;
pub mod predictions;
pub mod schema;
pub mod units;

pub use crate::actions::{
    ActionRecord, ActionState, Admission, Decision, OutboxItem, RecoveryReport, Resolution,
    UnknownCommit,
};
pub use crate::audit::{AuditCategory, AuditEntry, MAX_DETAIL_LEN};
pub use crate::error::StorageError;
pub use crate::events::{AppendOutcome, StoredEvent};
pub use crate::game_ledger::{GameLedgerEntry, LedgerDecision};
pub use crate::predictions::PredictionRecord;
pub use crate::schema::LATEST_SCHEMA_VERSION;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use soca_contracts::WallClock;

/// 存储句柄。
///
/// 一个 [`Store`] 就是一个逻辑写入者。需要并发读取时，调用方自行开只读连接；本 crate 不为
/// 此提供便利方法，因为多写者不值得便利。
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Store {
    /// 打开（或创建）磁盘上的存储。
    ///
    /// `opened_at` 由调用方提供，而不是在内部取 `now()`：审计与迁移时间必须可控、可复现，
    /// 这样断电与恢复测试才能得到确定结果。
    pub fn open(path: impl AsRef<Path>, opened_at: WallClock) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&path)?;
        schema::apply_pragmas(&conn)?;
        schema::migrate(&mut conn, opened_at)?;
        Ok(Self {
            conn,
            path: Some(path),
        })
    }

    /// 打开内存存储。用于测试与模拟，重开后内容全部消失。
    pub fn open_in_memory(opened_at: WallClock) -> Result<Self, StorageError> {
        let mut conn = Connection::open_in_memory()?;
        schema::apply_pragmas(&conn)?;
        schema::migrate(&mut conn, opened_at)?;
        Ok(Self { conn, path: None })
    }

    /// 当前 schema 版本。
    pub fn schema_version(&self) -> Result<u32, StorageError> {
        schema::current_version(&self.conn)
    }

    /// 执行一次 WAL 检查点。
    pub fn checkpoint(&self) -> Result<(), StorageError> {
        schema::checkpoint(&self.conn)
    }

    /// 存储位置。内存存储返回 `None`。
    pub fn location(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 仅供本 crate 内部使用的连接访问。
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// 仅供本 crate 内部使用的可变连接访问。
    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}
