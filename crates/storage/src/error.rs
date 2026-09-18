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

    #[error(
        "预测 {prediction_ref} 已经记录过不同内容；预测是当时写下的判断，事后修改等于伪造证据"
    )]
    PredictionAlreadyRecorded { prediction_ref: String },

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

    #[error("登记进目录的单元必须处于 COLD 状态，实际为 {state}（§9.2）")]
    UnitMustBeColdAtRegistration { state: &'static str },

    #[error("找不到单元 {unit_id}")]
    UnitNotFound { unit_id: String },

    #[error("单元 {unit_id} 尚未登记，不能写入快照；§9.2 的冷态就是注册表本身")]
    UnitNotRegistered { unit_id: String },

    #[error("单元 {unit_id} 的快照与注册信息不一致：{reason}")]
    InstanceInconsistent { unit_id: String, reason: &'static str },

    #[error("主体 {subject_id} 的世代 CAS 失败：期望 {expected}，实际 {actual}")]
    EpochMismatch {
        subject_id: String,
        expected: u64,
        actual: u64,
    },

    #[error("找不到主体路由：{subject_id}")]
    RouteNotFound { subject_id: String },

    #[error("找不到迁移事务：{transaction_id}")]
    TransactionNotFound { transaction_id: String },

    #[error("主体 {subject_id} 的世代 {new_epoch} 已经有一个迁移事务")]
    TransactionAlreadyExists { subject_id: String, new_epoch: u64 },

    #[error("回合 {episode_id} 中找不到游戏请求 {request_id}")]
    GameRequestNotFound {
        episode_id: String,
        request_id: String,
    },

    #[error("迁移事务 {transaction_id} 不能从 {from} 迁移到 {to}")]
    IllegalTransactionTransition {
        transaction_id: String,
        from: &'static str,
        to: &'static str,
    },

    #[error("单写者约束：写操作必须串行，同一时刻只允许一个写入者")]
    WriterBusy,

    #[error("失败关闭：{0}")]
    FailClosed(&'static str),

    #[error("找不到记忆条目 {memory_id}")]
    MemoryNotFound { memory_id: String },

    #[error("找不到可用的人工批准 {approval_id}（不存在，或次数已耗尽）")]
    ApprovalNotFound { approval_id: String },

    #[error(
        "审批标识 {approval_id} 已被另一份绑定占用；审批标识一旦使用不得复用\
         （它是追溯依据的锚点）"
    )]
    ApprovalAlreadyRecorded { approval_id: String },

    #[error(
        "记忆标识 {memory_id} 已被另一份内容占用；记忆标识一旦使用不得复用\
         （它是追溯依据的锚点）"
    )]
    MemoryAlreadyRecorded { memory_id: String },

    #[error("记忆 {memory_id} 已被取代过，不能再次取代")]
    MemoryAlreadySuperseded { memory_id: String },

    #[error("记忆 {memory_id} 已被删除；删除不是标记旧版本，不能被取代复活（§12.3）")]
    MemoryAlreadyDeleted { memory_id: String },
}
