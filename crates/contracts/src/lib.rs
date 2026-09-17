//! SoCA 契约层（实施路线 P0 的第一个工作包）。
//!
//! 本 crate 只做一件事：把《SoCA架构与实施方案》里的硬约束固化成**可编译、可序列化、
//! 可校验**的类型，让下列错误在编译期或解析期发生，而不是等到运行期指望某个 LLM 角色
//! "自觉遵守"。
//!
//! | 约束 | 出处 | 本 crate 的落点 |
//! |---|---|---|
//! | 只有执行许可可以触发副作用 | §7.2 | [`DataState::may_trigger_side_effect`] |
//! | 动作前必须有可检查的预测 | §6.3 | [`ActionIntent::new`] 强制 `prediction_ref` |
//! | 回执不等于后置条件验证成功 | §7.2 | [`ActionReceipt::is_postcondition_verified`] 恒 `false` |
//! | 验证结果不自动追加权限 | §7.2 | [`OutcomeVerified::grants_new_permission`] 恒 `false` |
//! | 许可绑定具体参数与版本 | §12.2 | [`ExecutionPermit::authorizes`] 比对参数摘要 |
//! | 单一动作单次授权、短期有效 | §12.2 | [`ExecutionPermit::new`] 的 TTL 与 `max_uses` |
//! | A4 不得由认知循环发起 | §12.1 | [`ActionLevel::is_allowed_in_cognitive_loop`] |
//! | 单调时钟不可跨 boot 比较 | §7.1 | [`Monotonic`] 故意不实现 `Ord` |
//! | 模型自评分数不是概率真值 | §3.2 | [`ModelSelfReport`] 无到 [`CalibratedProbability`] 的转换 |
//! | 屏幕/文档/转写文本无指令权限 | §6.1、§11.1 | [`Provenance::is_instruction_authority`] |
//! | 私人数据不因内存不足上云 | §8 | [`DataClass::cloud_egress`] |
//! | 不明副作用不能靠卸载单元解决 | §9.2 | [`UnitSnapshot::transition_to`] 的冷态闸门 |
//!
//! 本 crate 不做 I/O、不碰 OS、不调用模型、不持有密钥或句柄，也没有 `unsafe`。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod action;
pub mod belief;
pub mod data_state;
pub mod envelope;
pub mod error;
pub mod ids;
pub mod policy;
pub mod time;
pub mod unit;
pub mod validate;

pub use crate::action::{
    ActionIntent, ActionReceipt, CommitStatus, ExecutionPermit, OutcomeVerified, ResourceCost,
    Verdict, V1_FORBIDDEN_TOOL_PREFIXES,
};
pub use crate::belief::{
    CalibratedProbability, CalibrationSource, Hypothesis, ModelSelfReport, Observation, Prediction,
    Uncertainty,
};
pub use crate::data_state::{DataState, DataStateKind};
pub use crate::envelope::{
    DerivationKind, Envelope, PayloadRef, Provenance, UserChannel,
    MAX_SMALL_MESSAGE_ENVELOPE_BYTES,
};
pub use crate::error::ContractError;
pub use crate::ids::{
    ActionId, ApprovalId, BlobRef, BootId, BudgetRef, CapabilityPolicyRef, DomainId, EventId,
    EvidenceRef, GoalId, IdempotencyKey, MediaType, ModelProfileRef, ModelVersion, PermitId,
    PolicyVersion, PredictionRef, RelationRef, ResourceScope, Sha256Hex, SourceId, StrategyVersion,
    SubjectId, TaskContractVersion, TaskId, ToolId, UnitId,
};
pub use crate::policy::{
    ActionLevel, ApprovalRequirement, DataClass, EgressVerdict, PermissionScope,
};
pub use crate::time::{Monotonic, TimeWindow, WallClock};
pub use crate::unit::{Scope, UnitKind, UnitSnapshot, UnitState, UNIT_SNAPSHOT_FIELDS};

/// 冻结的契约版本。
///
/// 信封与单元快照的字段集合在此版本上冻结。任何字段增删都必须提升它，并同步更新
/// `tests/golden/` 下的样本。P0 门槛"冻结最小消息版本"指的就是这一条。
pub const SCHEMA_VERSION: u16 = 1;
