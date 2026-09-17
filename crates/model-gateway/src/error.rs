//! 模型网关错误。
//!
//! 与契约层、存储层一致：变体只携带标识、上限与计数，不携带 prompt 内容、密钥或用户数据，
//! 因此可以直接写入审计账（§6.8）。

use thiserror::Error;

use soca_contracts::ContractError;

/// 传输层失败原因。
///
/// 分类直接决定重试策略，所以它必须比"出错了"更细：
///
/// | 变体 | 是否重试 | 理由 |
/// |---|---|---|
/// | [`TransportError::Timeout`] | 可以，受预算上限约束 | 推理没有副作用，重来一次不会改变世界 |
/// | [`TransportError::Unavailable`] | 可以，受预算上限约束 | 后端暂时不可用，等一次可能就好了 |
/// | [`TransportError::Rejected`] | **不重试** | 鉴权失败、参数非法，再试多少次都是同一个结果 |
/// | [`TransportError::Malformed`] | **不重试** | 返回物解析不了，重试只会再拿到一份解析不了的返回物 |
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[allow(missing_docs)]
pub enum TransportError {
    #[error("模型调用超时：上限 {limit_millis} 毫秒")]
    Timeout { limit_millis: u64 },

    #[error("模型后端暂时不可用：{reason}")]
    Unavailable { reason: String },

    #[error("模型后端拒绝本次调用（不重试）：{reason}")]
    Rejected { reason: String },

    #[error("模型返回物无法解析（不重试）：{reason}")]
    Malformed { reason: String },
}

impl TransportError {
    /// 是否值得再试一次。
    ///
    /// 只有"可能自己好起来"的失败才值得重试。把 `Rejected` 也重试，会让一个鉴权配置错误
    /// 变成一串无意义的请求，而且每一次都要等满超时。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout { .. } | Self::Unavailable { .. })
    }

    /// 稳定名称，用于审计记录。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "timeout",
            Self::Unavailable { .. } => "unavailable",
            Self::Rejected { .. } => "rejected",
            Self::Malformed { .. } => "malformed",
        }
    }
}

/// 模型网关失败原因。
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum GatewayError {
    #[error("契约校验失败：{0}")]
    Contract(#[from] ContractError),

    #[error("传输层失败：{0}")]
    Transport(#[from] TransportError),

    #[error("传输层一直失败，已用尽 {attempts} 次尝试：{last}")]
    AttemptsExhausted { attempts: u8, last: TransportError },

    #[error("总墙钟已超出上限：上限 {limit_millis} 毫秒，已用 {elapsed_millis} 毫秒")]
    WallClockExceeded {
        limit_millis: u64,
        elapsed_millis: u64,
    },

    #[error("序列化失败：{0}")]
    Serde(#[from] serde_json::Error),
}
