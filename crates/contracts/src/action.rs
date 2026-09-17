//! 动作意图、执行许可、执行回执与后置验证（§6.3、§7.2、§7.3、§12.1、§12.2）。
//!
//! 本模块的核心是一条可执行的断言：
//!
//! > 七类数据状态里，**只有 [`ExecutionPermit`] 允许触发 OS 副作用**。
//!
//! 其余六类在 [`DataState::may_trigger_side_effect`] 上恒为 `false`。这不是文档里的口号，
//! 而是执行代理唯一需要检查的入口。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ActionId, ActionLevel, ApprovalId, BudgetRef, CapabilityPolicyRef, ContractError, EvidenceRef,
    PermitId, PolicyVersion, PredictionRef, ResourceScope, Sha256Hex, SubjectId, ToolId,
    UnitId, WallClock,
};

/// 首版禁止经由认知循环调用的工具前缀（§12.2）。
///
/// 工具不接收任意拼接的 shell 字符串作为默认接口。宁可让这类工具在契约层就构造不出
/// `ActionIntent`，也不要在执行代理里靠字符串检查去拦。
pub const V1_FORBIDDEN_TOOL_PREFIXES: &[&str] = &["shell", "cmd", "powershell", "exec", "wmic"];

/// 预估资源代价。新任务必须原子预留资源（§10.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCost {
    /// 预估峰值新增 RAM（字节）。
    pub est_ram_bytes: u64,
    /// 预估 token 数。
    pub est_tokens: u32,
    /// 预估耗时（毫秒），用于 deadline 预留。
    pub est_millis: u64,
}

/// 动作意图（§7.2）。
///
/// 不能直接触发副作用。它只描述"想做什么"，必须再经过执行代理换取 [`ExecutionPermit`]。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionIntent {
    /// 动作标识。去重与恢复核对的锚点（§7.3）。
    pub action_id: ActionId,
    /// 工具标识。
    pub tool_id: ToolId,
    /// 对象范围。
    pub object_scope: ResourceScope,
    /// 结构化参数。必须是 JSON 对象。
    pub parameters: Value,
    /// 预置条件。
    pub preconditions: Vec<String>,
    /// 动作前预测的引用。§6.3：动作前必须记录可检查的预测。
    pub prediction_ref: PredictionRef,
    /// 风险等级。
    pub risk: ActionLevel,
    /// 代价。
    pub cost: ResourceCost,
    /// 提出意图的单元。
    pub proposed_by: UnitId,
}

impl ActionIntent {
    /// 构造并校验。
    ///
    /// 逐条拒绝：
    /// * `prediction_ref` 是必需参数，没有它就没有"请求≠执行≠验证"里的"预测"环节；
    /// * 参数必须是结构化 JSON 对象，不接受任意字符串；
    /// * 工具不得命中首版禁止前缀；
    /// * A4 不允许由普通认知循环发起（§12.1）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        action_id: ActionId,
        tool_id: ToolId,
        object_scope: ResourceScope,
        parameters: Value,
        preconditions: Vec<String>,
        prediction_ref: PredictionRef,
        risk: ActionLevel,
        cost: ResourceCost,
        proposed_by: UnitId,
    ) -> Result<Self, ContractError> {
        if !parameters.is_object() {
            return Err(ContractError::ParametersNotStructured {
                actual: json_kind(&parameters),
            });
        }
        if V1_FORBIDDEN_TOOL_PREFIXES
            .iter()
            .any(|prefix| tool_id.as_str().starts_with(prefix))
        {
            return Err(ContractError::ForbiddenToolInV1 {
                tool_id: tool_id.to_string(),
            });
        }
        if !risk.is_allowed_in_cognitive_loop() {
            return Err(ContractError::ForbiddenInCognitiveLoop {
                level: risk.as_str(),
            });
        }
        Ok(Self {
            action_id,
            tool_id,
            object_scope,
            parameters,
            preconditions,
            prediction_ref,
            risk,
            cost,
            proposed_by,
        })
    }

    /// 参数摘要。执行许可绑定到它，参数一动摘要就变，旧许可立即失效。
    ///
    /// `serde_json` 默认的 map 是按 key 排序的 `BTreeMap`，因此同一份参数序列化结果稳定，
    /// 摘要可复算。若将来开启 `preserve_order`，此处必须改成显式规范化。
    pub fn parameters_digest(&self) -> Sha256Hex {
        let canonical = serde_json::to_vec(&self.parameters).unwrap_or_default();
        Sha256Hex::of_bytes(&canonical)
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 执行许可。能力令牌绑定到具体动作参数与版本（§12.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPermit {
    /// 许可标识。
    pub permit_id: PermitId,
    /// 被授权的主体（用户或主体）。
    pub subject_id: SubjectId,
    /// 只能用于这个工具。
    pub tool_id: ToolId,
    /// 只能作用于这个资源范围。
    pub object_scope: ResourceScope,
    /// 绑定的参数摘要。
    pub parameters_digest: Sha256Hex,
    /// 签发时刻。
    pub issued_at: WallClock,
    /// 失效时刻。短期授权，不是常设许可。
    pub expires_at: WallClock,
    /// 允许次数。
    pub max_uses: u8,
    /// 绑定的预算账。
    pub budget_ref: BudgetRef,
    /// 签发时的策略版本。策略更新后旧许可不得继续使用。
    pub policy_version: PolicyVersion,
    /// 绑定的能力策略。
    pub capability_policy_ref: CapabilityPolicyRef,
    /// 审批标识。A2/A3 必须有。
    pub approval_id: Option<ApprovalId>,
    /// 该许可覆盖的动作等级。
    pub action_level: ActionLevel,
}

impl ExecutionPermit {
    /// 构造并校验许可自身。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        permit_id: PermitId,
        subject_id: SubjectId,
        tool_id: ToolId,
        object_scope: ResourceScope,
        parameters_digest: Sha256Hex,
        issued_at: WallClock,
        expires_at: WallClock,
        max_uses: u8,
        budget_ref: BudgetRef,
        policy_version: PolicyVersion,
        capability_policy_ref: CapabilityPolicyRef,
        approval_id: Option<ApprovalId>,
        action_level: ActionLevel,
    ) -> Result<Self, ContractError> {
        if expires_at <= issued_at {
            return Err(ContractError::PermitInvalid {
                reason: "expires_at 必须晚于 issued_at",
            });
        }
        if max_uses == 0 {
            return Err(ContractError::PermitInvalid {
                reason: "max_uses 必须至少为 1",
            });
        }
        if action_level == ActionLevel::A4 {
            return Err(ContractError::ForbiddenInCognitiveLoop {
                level: ActionLevel::A4.as_str(),
            });
        }
        if action_level.requires_approval_id() && approval_id.is_none() {
            return Err(ContractError::MissingApproval {
                level: action_level.as_str(),
            });
        }
        Ok(Self {
            permit_id,
            subject_id,
            tool_id,
            object_scope,
            parameters_digest,
            issued_at,
            expires_at,
            max_uses,
            budget_ref,
            policy_version,
            capability_policy_ref,
            approval_id,
            action_level,
        })
    }

    /// 由动作意图签发许可。
    #[allow(clippy::too_many_arguments)]
    pub fn issue_for(
        intent: &ActionIntent,
        permit_id: PermitId,
        subject_id: SubjectId,
        issued_at: WallClock,
        ttl_seconds: i64,
        max_uses: u8,
        budget_ref: BudgetRef,
        policy_version: PolicyVersion,
        capability_policy_ref: CapabilityPolicyRef,
        approval_id: Option<ApprovalId>,
    ) -> Result<Self, ContractError> {
        Self::new(
            permit_id,
            subject_id,
            intent.tool_id.clone(),
            intent.object_scope.clone(),
            intent.parameters_digest(),
            issued_at,
            issued_at.plus_seconds(ttl_seconds),
            max_uses,
            budget_ref,
            policy_version,
            capability_policy_ref,
            approval_id,
            intent.risk,
        )
    }

    /// 判断本许可是否授权这次具体动作。
    ///
    /// 执行代理在真正产生副作用前调用它。任何一项不符即拒绝，且拒绝原因写入审计账
    /// （§6.8）。
    pub fn authorizes(
        &self,
        intent: &ActionIntent,
        now: WallClock,
        uses_so_far: u8,
    ) -> Result<(), ContractError> {
        if now >= self.expires_at {
            return Err(ContractError::PermitExpired {
                expires_at: self.expires_at.to_string(),
            });
        }
        if uses_so_far >= self.max_uses {
            return Err(ContractError::PermitExhausted {
                max_uses: self.max_uses,
            });
        }
        if self.tool_id != intent.tool_id {
            return Err(ContractError::PermitMismatch { field: "tool_id" });
        }
        if self.object_scope != intent.object_scope {
            return Err(ContractError::PermitMismatch {
                field: "object_scope",
            });
        }
        // 先检查参数：参数被改过就说明这次动作不是被批准的那次。
        if self.parameters_digest != intent.parameters_digest() {
            return Err(ContractError::PermitMismatch {
                field: "parameters_digest",
            });
        }
        if self.action_level != intent.risk {
            return Err(ContractError::PermitMismatch {
                field: "action_level",
            });
        }
        Ok(())
    }
}

/// 提交状态（§7.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitStatus {
    /// 已提交，结果未知。
    Submitted,
    /// 执行代理报告完成。
    Completed,
    /// 执行失败。
    Failed,
    /// 在执行之后、回执之前崩溃，且恢复时未能确认目标状态（§7.3）。
    UnknownCommit,
}

/// 执行回执。
///
/// §7.2：回执**不等于**后置条件验证成功。因此本结构没有"已验证"字段，
/// [`ActionReceipt::is_postcondition_verified`] 恒为 `false`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionReceipt {
    /// 对应动作。
    pub action_id: ActionId,
    /// 消耗的许可。
    pub permit_id: PermitId,
    /// 提交状态。
    pub status: CommitStatus,
    /// 回执记录时刻。
    pub recorded_at: WallClock,
    /// 执行代理提供的简短说明。不得包含密钥或完整个人内容。
    pub detail: String,
    /// 恢复时查到的目标状态版本（§7.3：恢复先查询目标状态，不能直接重发）。
    pub observed_target_version: Option<String>,
}

impl ActionReceipt {
    /// 回执是否构成后置条件验证。
    ///
    /// 恒为 `false`。要判定后置条件，必须由新的观测产生 [`OutcomeVerified`]。
    /// 保留这个方法是为了让"回执=完成"这种写法在代码评审时立刻暴露。
    pub fn is_postcondition_verified(&self) -> bool {
        false
    }
}

/// 后置条件判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// 新观测支持预期结果。
    Supported,
    /// 新观测否定预期结果。
    Refuted,
    /// 受外部变化影响，无法判定（§6.7）。
    Inconclusive,
}

/// 新观测对预期结果的判定（§7.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeVerified {
    /// 对应动作。
    pub action_id: ActionId,
    /// 被检验的预测。
    pub prediction_ref: PredictionRef,
    /// 判定结果。
    pub verdict: Verdict,
    /// 支撑判定的观测。
    pub observation_refs: Vec<EvidenceRef>,
}

impl OutcomeVerified {
    /// 构造并校验。
    ///
    /// 必须有至少一条观测：没有观测的"验证"只是把回执复述了一遍。
    pub fn new(
        action_id: ActionId,
        prediction_ref: PredictionRef,
        verdict: Verdict,
        observation_refs: Vec<EvidenceRef>,
    ) -> Result<Self, ContractError> {
        if observation_refs.is_empty() {
            return Err(ContractError::MissingRefs {
                field: "outcome_verified.observation_refs",
            });
        }
        crate::validate::assert_unique(&observation_refs, "outcome_verified.observation_refs")?;
        Ok(Self {
            action_id,
            prediction_ref,
            verdict,
            observation_refs,
        })
    }

    /// 本判定是否自动追加新权限。
    ///
    /// 恒为 `false`。§7.2：验证结果驱动下一轮，不自动追加新权限。
    pub fn grants_new_permission(&self) -> bool {
        false
    }
}
