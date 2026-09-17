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

    #[error("动作不在公开合法范围内")]
    InvalidAction,

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
