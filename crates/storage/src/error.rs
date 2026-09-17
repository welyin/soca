//! 存储层错误。
//!
//! 与契约层一致：变体只携带标识、状态名与字段名，不携带路径明文、密钥或用户内容，
//! 因此可以直接写入审计账（§6.8、§12.3）。

use thiserror::Error;

/// 存储层失败原因。
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite 错误：{0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("契约校验失败：{0}")]
    Contract(#[from] soca_contracts::ContractError),

    #[error("序列化失败：{0}")]
    Serde(#[from] serde_json::Error),

    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("数据库 schema 版本 {found} 高于本程序支持的 {supported}，拒绝打开")]
    SchemaTooNew { found: u32, supported: u32 },

    #[error("事件已存在：{event_id}")]
    DuplicateEvent { event_id: String },

    #[error(
        "事件流位置已被占用：<{source_id}> epoch {source_epoch} boot {boot_id} 序号 {source_sequence}"
    )]
    StreamPositionConflict {
        source_id: String,
        source_epoch: u32,
        boot_id: String,
        source_sequence: u64,
    },

    #[error("幂等键 {key} 已绑定到事件 {existing_event_id}，不能改绑到 {incoming_event_id}")]
    IdempotencyKeyConflict {
        key: String,
        existing_event_id: String,
        incoming_event_id: String,
    },

    #[error("找不到动作：{action_id}")]
    ActionNotFound { action_id: String },

    #[error("动作 {action_id} 处于 {state} 状态，不允许执行 {operation}")]
    IllegalActionTransition {
        action_id: String,
        state: String,
        operation: &'static str,
    },

    #[error("动作 {action_id} 已被拒绝过，不能再次投递")]
    ActionAlreadyDenied { action_id: String },

    #[error(
        "动作 ID {action_id} 已被参数摘要 {existing_digest} 占用，不能改绑到 {incoming_digest}；\
         动作 ID 一旦使用不得复用（§7.3 去重的依据就是它）"
    )]
    ActionIdReused {
        action_id: String,
        existing_digest: String,
        incoming_digest: String,
    },

    #[error("回执引用的许可与动作 {action_id} 绑定的许可不一致：应为 {expected}，实际 {actual}")]
    ReceiptPermitMismatch {
        action_id: String,
        expected: String,
        actual: String,
    },

    #[error("事件 {event_id} 未通过契约校验：{source}")]
    EventRejected {
        event_id: String,
        #[source]
        source: soca_contracts::ContractError,
    },

    #[error("游标 {cursor} 非法：{reason}")]
    InvalidCursor { cursor: i64, reason: &'static str },

    #[error("数据库内容损坏：{field}")]
    CorruptRow { field: &'static str },

    #[error("单写者约束：写操作必须串行，同一时刻只允许一个写入者")]
    WriterBusy,
}
