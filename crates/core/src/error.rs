//! 运行时错误。
//!
//! 与契约层、存储层一致：变体只携带标识与短原因，可以直接写入审计账（§12.3）。

use soca_contracts::ContractError;
use soca_storage::StorageError;

use crate::broker::BrokerError;

/// 闭环运行时的失败原因。
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error(transparent)]
    Contract(#[from] ContractError),

    #[error(transparent)]
    Broker(#[from] BrokerError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("动作 {action_id} 未通过受理：{reason}")]
    AdmissionDenied { action_id: String, reason: String },

    #[error("动作 {action_id} 处于 {state} 状态，无法继续该步骤：{step}")]
    UnexpectedActionState {
        action_id: String,
        state: String,
        step: &'static str,
    },

    #[error("无法从动作参数解析出作用对象：{0}")]
    UnresolvedSubject(String),

    #[error("动作 {action_id} 没有对应的动作前预测 {prediction_ref}")]
    PredictionMissing {
        action_id: String,
        prediction_ref: String,
    },

    #[error("预测 {prediction_ref} 的期望作用对象与观测对象不是同一个：{observed}")]
    SubjectMismatch {
        prediction_ref: String,
        observed: String,
    },
}
