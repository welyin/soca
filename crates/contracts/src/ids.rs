//! 标识与引用类型。
//!
//! 设计约束（§2、§3.2）：信封和快照里只允许出现**引用**，不允许出现裸内存指针、活连接
//! 句柄、密钥或模型权重。所以这里所有类型要么是"不透明字符串 + 前缀校验"，要么是 UUID，
//! 没有任何可以承载句柄的 `usize` / `*const T` 字段。
//!
//! 前缀校验不是装饰：`obs:193` 和 `tool-result:64` 都能通过 `EvidenceRef`，但
//! `"unit:file-summary:07"` 传进 `EvidenceRef` 会在解析期直接失败，而不是等到审计时
//! 才发现证据根本不存在（§7.2：引用必须能解析为存在且仍可访问的证据）。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::ContractError;

/// 非空、有长度上限、不含首尾空白的不透明字符串。
macro_rules! opaque_string {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $max:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// 允许的最大字节数。
            pub const MAX_LEN: usize = $max;

            /// 构造并校验。空串、超长、首尾空白都会被拒绝。
            pub fn new(raw: impl Into<String>) -> Result<Self, ContractError> {
                let raw = raw.into();
                if raw.is_empty() {
                    return Err(ContractError::EmptyField { field: $kind });
                }
                if raw.len() > Self::MAX_LEN {
                    return Err(ContractError::FieldTooLong {
                        field: $kind,
                        limit: Self::MAX_LEN,
                        actual: raw.len(),
                    });
                }
                if raw.trim() != raw.as_str() {
                    return Err(ContractError::SurroundingWhitespace { field: $kind });
                }
                Ok(Self(raw))
            }

            /// 借用底层字符串。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ContractError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::new(raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

pub(crate) use opaque_string;

/// 带固定前缀的引用。前缀不符即在解析期失败。
macro_rules! prefixed_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, [$($prefix:literal),+ $(,)?]) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// 允许的前缀集合。
            pub const PREFIXES: &'static [&'static str] = &[$($prefix),+];
            /// 允许的最大字节数。
            pub const MAX_LEN: usize = 256;

            /// 构造并校验前缀。
            pub fn new(raw: impl Into<String>) -> Result<Self, ContractError> {
                let raw = raw.into();
                if raw.is_empty() {
                    return Err(ContractError::EmptyField { field: $kind });
                }
                if raw.len() > Self::MAX_LEN {
                    return Err(ContractError::FieldTooLong {
                        field: $kind,
                        limit: Self::MAX_LEN,
                        actual: raw.len(),
                    });
                }
                if raw.trim() != raw.as_str() {
                    return Err(ContractError::SurroundingWhitespace { field: $kind });
                }
                if !Self::PREFIXES.iter().any(|p| raw.starts_with(*p)) {
                    return Err(ContractError::MalformedId {
                        kind: $kind,
                        expected_prefixes: Self::PREFIXES,
                        actual: raw,
                    });
                }
                Ok(Self(raw))
            }

            /// 借用底层字符串。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ContractError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::new(raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

/// UUID 实体标识。
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// 生成一个新的随机标识。
            pub fn generate() -> Self {
                Self(Uuid::new_v4())
            }

            /// 由既有 UUID 构造。
            pub fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// 从字符串解析。
            pub fn parse(text: &str) -> Result<Self, ContractError> {
                Uuid::parse_str(text).map(Self).map_err(|_| ContractError::MalformedUuid {
                    kind: $kind,
                    actual: text.to_string(),
                })
            }

            /// 取出底层 UUID。
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// UUID 类：事件与 boot
// ---------------------------------------------------------------------------

uuid_id!(
    /// 事件标识。跨设备唯一，不承载顺序信息（顺序见 `Envelope` 的序列与单调时钟）。
    EventId,
    "EventId"
);

uuid_id!(
    /// 进程启动标识。§7.1：重启后不能直接比较不同 boot 的单调时间，所以单调时间必须
    /// 携带 `boot_id`，而 `Monotonic` 故意不实现 `Ord`。
    BootId,
    "BootId"
);

// ---------------------------------------------------------------------------
// 自由形态但有约束的字符串
// ---------------------------------------------------------------------------

opaque_string!(
    /// 事件来源标识，例如 `user:desktop`、`device:screen`、`unit:file-summary:07`。
    SourceId,
    "source_id",
    128
);

opaque_string!(
    /// 工具标识，例如 `fs.read`、`fs.write`、`ui.capture.screen`。
    ToolId,
    "tool_id",
    96
);

opaque_string!(
    /// 签名策略版本。
    PolicyVersion,
    "policy_version",
    64
);

opaque_string!(
    /// 模型版本，初值使用权重哈希，例如 `sha256:...`。
    ModelVersion,
    "model_version",
    160
);

opaque_string!(
    /// 单元策略版本，例如 `summary-policy-v1`。
    StrategyVersion,
    "strategy_version",
    64
);

opaque_string!(
    /// 幂等键。§7.3：执行代理以动作 ID 去重，重放不得重复执行外部动作。
    IdempotencyKey,
    "idempotency_key",
    128
);

opaque_string!(
    /// 资源范围表达式，例如某个已授权目录或某个窗口。
    ResourceScope,
    "resource_scope",
    512
);

opaque_string!(
    /// 媒体类型，例如 `application/json`、`image/png`。
    MediaType,
    "media_type",
    128
);

opaque_string!(
    /// 单元 scope 的领域名，例如 `document-summary`。
    DomainId,
    "scope.domain",
    96
);

opaque_string!(
    /// 任务合同版本，例如 `summary-v1`。§3.2：状态必须声明其有效任务域。
    TaskContractVersion,
    "scope.task_contract",
    64
);

// ---------------------------------------------------------------------------
// 前缀引用
// ---------------------------------------------------------------------------

prefixed_id!(
    /// 任务标识。授权、预算与审计都挂在任务上，而不是挂在"某条 prompt"上。
    TaskId,
    "task_id",
    ["task:"]
);

prefixed_id!(
    /// 认知单元标识，例如 `unit:file-summary:07`。
    UnitId,
    "unit_id",
    ["unit:"]
);

prefixed_id!(
    /// 受托目标标识。
    GoalId,
    "goal_id",
    ["goal:"]
);

prefixed_id!(
    /// 证据引用。必须是可解析、可追溯的观测或工具结果。
    EvidenceRef,
    "evidence_ref",
    ["obs:", "tool-result:", "receipt:"]
);

prefixed_id!(
    /// 关系边引用（来源谱系、依赖、冲突等）。
    RelationRef,
    "relation_ref",
    ["relation:"]
);

prefixed_id!(
    /// 内容仓对象引用。大载荷只传引用（§10.4）。
    BlobRef,
    "blob_ref",
    ["blob:"]
);

prefixed_id!(
    /// 动作前预测的引用。§6.3：动作前必须记录可检查的预测。
    PredictionRef,
    "prediction_ref",
    ["prediction:"]
);

prefixed_id!(
    /// 能力策略引用，例如 `cap:read-selected-folder`。
    CapabilityPolicyRef,
    "capability_policy_ref",
    ["cap:"]
);

prefixed_id!(
    /// 预算账引用。
    BudgetRef,
    "budget_ref",
    ["budget:"]
);

prefixed_id!(
    /// 模型画像引用，例如 `profile:reasoning-local`。
    ModelProfileRef,
    "model_profile_ref",
    ["profile:"]
);

prefixed_id!(
    /// 一次动作的标识。§7.3：动作 ID 是去重与恢复核对的锚点。
    ActionId,
    "action_id",
    ["action:"]
);

prefixed_id!(
    /// 执行许可标识。
    PermitId,
    "permit_id",
    ["permit:"]
);

prefixed_id!(
    /// 审批标识。A2/A3 动作必须能追溯到一次明确的用户批准（§12.1）。
    ApprovalId,
    "approval_id",
    ["approval:"]
);

prefixed_id!(
    /// 用户或主体标识。能力令牌绑定到它（§12.2）。
    SubjectId,
    "subject_id",
    ["user:", "subject:"]
);

// ---------------------------------------------------------------------------
// SHA-256 摘要
// ---------------------------------------------------------------------------

/// 小写十六进制 SHA-256 摘要。
///
/// §12.2 要求执行许可"绑定具体动作参数与版本"。绑定方式就是参数摘要：参数一动，摘要就变，
/// 旧许可立即失效，而不是靠执行代理"再看一眼参数对不对"。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// 十六进制摘要长度。
    pub const LEN: usize = 64;

    /// 计算字节序列的摘要。
    pub fn of_bytes(bytes: &[u8]) -> Self {
        use std::fmt::Write as _;

        let digest = Sha256::digest(bytes);
        let mut hex = String::with_capacity(Self::LEN);
        for byte in digest.iter() {
            // 写入 String 不会失败。
            let _ = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// 从既有十六进制字符串解析，必须是 64 位小写十六进制。
    pub fn parse(raw: impl Into<String>) -> Result<Self, ContractError> {
        let raw = raw.into();
        let is_lower_hex = raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if raw.len() != Self::LEN || !is_lower_hex {
            return Err(ContractError::MalformedDigest {
                kind: "Sha256Hex",
                actual: raw,
            });
        }
        Ok(Self(raw))
    }

    /// 借用底层十六进制字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = ContractError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(raw)
    }
}

impl From<Sha256Hex> for String {
    fn from(value: Sha256Hex) -> Self {
        value.0
    }
}
