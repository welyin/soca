//! 游戏宿主错误。

use soca_contracts::{ContractError, GameProtocolError};
use soca_storage::StorageError;

/// 宿主层的失败原因。
///
/// 注意区分两类：[`HostError`] 是**宿主自己**的问题（编程错误、存储故障），
/// 而"请求不合法"不是错误——它是一条正常的拒绝回执，必须留痕而不是中断循环（§11.2）。
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error(transparent)]
    Protocol(#[from] GameProtocolError),

    #[error(transparent)]
    Contract(#[from] ContractError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("回合 {episode_id} 不在运行中")]
    UnknownEpisode { episode_id: String },

    #[error("回合 {episode_id} 已经存在")]
    EpisodeAlreadyExists { episode_id: String },

    #[error("引擎不可用：{reason}")]
    EngineUnavailable { reason: String },

    #[error("宿主状态损坏：{0}")]
    CorruptState(&'static str),
}

/// 子进程引擎的失败原因。
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum ProcessEngineError {
    #[error("无法启动游戏进程：{0}")]
    Spawn(#[source] std::io::Error),

    #[error("与游戏进程的输入输出失败：{0}")]
    Io(#[source] std::io::Error),

    #[error("游戏进程已不可用：{reason}")]
    Unusable { reason: String },

    #[error("等待游戏进程响应超时（{seconds} 秒）")]
    Timeout { seconds: f64 },

    #[error("帧声明 {declared} 字节，超过上限 {limit} 字节；先检查长度再分配（§11.4）")]
    FrameTooLarge { declared: u32, limit: usize },

    #[error("游戏进程返回的响应不是合法 JSON：{0}")]
    Malformed(#[source] serde_json::Error),

    #[error("游戏进程报告错误：{kind} - {reason}")]
    Refused { kind: String, reason: String },

    #[error("游戏进程提前结束")]
    Exited,

    #[error("响应缺少字段 {field}")]
    MissingField { field: &'static str },
}

impl From<ProcessEngineError> for EngineError {
    fn from(error: ProcessEngineError) -> Self {
        match error {
            ProcessEngineError::Refused { kind, reason } if kind == "domain" => {
                // 引擎认为动作不在自己的规则内。宿主已经查过动作域，所以这通常说明
                // 两侧对规则版本的理解不一致——按"动作非法"处理，不推进世界。
                EngineError::InvalidAction { reason }
            }
            ProcessEngineError::Malformed(_)
            | ProcessEngineError::FrameTooLarge { .. }
            | ProcessEngineError::Exited => EngineError::Unavailable {
                reason: "游戏进程协议失步".to_string(),
            },
            other => EngineError::Unavailable {
                reason: other.to_string(),
            },
        }
    }
}

/// 规则引擎的失败原因。
///
/// 引擎故障必须与"规则判负"分开：§11.3 要求 `truncated` 与 `terminated` 各自成立，
/// 不得把基础设施故障伪装成游戏结束。
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("回合已结束，拒绝继续执行")]
    EpisodeFinished,

    #[error("动作不属于本游戏的动作域")]
    DomainMismatch,

    #[error("动作不在公开合法范围内：{reason}")]
    InvalidAction {
        /// 引擎给出的具体原因。
        reason: String,
    },

    #[error("引擎不可用：{reason}")]
    Unavailable { reason: String },

    #[error("该引擎不支持确定性快照恢复")]
    SnapshotUnsupported,
}

impl From<EngineError> for HostError {
    fn from(error: EngineError) -> Self {
        HostError::EngineUnavailable {
            reason: error.to_string(),
        }
    }
}
