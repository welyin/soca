//! §8 的上下文编译与模型返回物。
//!
//! §8 规定了一次模型调用的形状：
//!
//! > 一次模型调用由上下文编译器提供：当前目标、可用证据引用、局部 belief 摘要、过去动作
//! > 结果、能力范围、截止时间与输出 Schema。模型返回候选后先解析和校验，再决定工具动作；
//! > 工具结果作为新观测重新进入单元，因此**大模型就在感知–预测–行动–反馈的循环里面**。
//!
//! 以及两条边界：
//!
//! > 状态、目标、证据、工具句柄和预算在 Core 与存储中，不仅在 LLM 上下文窗口里。
//!
//! > 关键边界：……确定性规则管边界，LLM 可以辅助识别风险但不能放宽规则。
//!
//! 本模块把那七项做成 [`ContextBundle`]，并把两条**可机械检查**的边界做成校验：
//!
//! 1. **模型不能引用它没看到的证据。** [`ContextBundle::validate_proposals`] 要求模型返回的
//!    每一条证据引用都已经在上下文里。没有这道检查，模型可以编一个 `obs:xxx` 作为结论的
//!    依据，而所有结构校验都会通过。这与 L2 的
//!    [`crate::Workspace::validate_candidates`] 是同一道闸的两次安装：一次挡在簇边界，一次
//!    挡在模型边界。
//! 2. **模型不能自己签发预测引用。** 提案里的动作必须引用一条**已经记录在案**的预测
//!    （§6.3：动作前必须有可检查的预测由单元写下，不是由模型现编）。存储层随后还会独立
//!    再查一次预测是否存在，两道闸互不依赖。
//!
//! 第三条边界靠类型而非校验：工具句柄、密钥、模型权重在这份结构里**没有字段可以放**，
//! 正如 §3.2 的快照一样。

use serde::{Deserialize, Serialize};

use crate::validate::assert_unique;
use crate::{
    ActionId, Candidate, ContractError, DataClass, EgressVerdict, EvidenceRef, ModelBackend,
    ModelSelfReport, ModelVersion, PredictionRef, ToolId, Verdict, WallClock, ActionLevel,
};

/// 上下文包的 schema 版本。字段增删必须提升它。
pub const CONTEXT_SCHEMA_VERSION: u16 = 1;

/// 模型输出 schema 的版本。
///
/// 与 [`CONTEXT_SCHEMA_VERSION`] 分开：允许的候选种类变了要提升它，而上下文字段变了要提升
/// 前者。两者混成一个版本号，就没法说清"这次不兼容是谁引起的"。
pub const MODEL_OUTPUT_SCHEMA_VERSION: u16 = 1;

/// 上下文允许携带的最大证据条目数。
pub const MAX_CONTEXT_EVIDENCE: usize = 64;
/// 上下文允许携带的最大信念摘要条目数。
pub const MAX_CONTEXT_BELIEFS: usize = 32;
/// 上下文允许携带的最大历史结果数。
pub const MAX_CONTEXT_OUTCOMES: usize = 32;
/// 上下文允许携带的最大预测引用数。
pub const MAX_CONTEXT_PREDICTIONS: usize = 16;
/// 上下文允许的最大字节数（按序列化长度计）。
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;
/// 一次模型返回允许携带的最大提案数。
pub const MAX_PROPOSALS: usize = 16;
/// 单次调用允许的最大尝试次数。§8 只禁止**无限**重试，所以有界重试是允许的。
pub const MAX_MODEL_ATTEMPTS: u8 = 3;

/// 模型调用预算。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBudget {
    /// 最多生成多少 token。
    pub max_output_tokens: u32,
    /// 墙钟上限（毫秒）。
    pub max_wall_millis: u64,
    /// 最多尝试几次。**必须非零且不超过 [`MAX_MODEL_ATTEMPTS`]**。
    pub max_attempts: u8,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        }
    }
}

impl ModelBudget {
    /// 校验。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.max_attempts == 0 {
            return Err(ContractError::ContextLimitExceeded {
                field: "model_budget.max_attempts",
                limit: MAX_MODEL_ATTEMPTS as usize,
                actual: 0,
            });
        }
        if self.max_attempts > MAX_MODEL_ATTEMPTS {
            return Err(ContractError::ContextLimitExceeded {
                field: "model_budget.max_attempts",
                limit: MAX_MODEL_ATTEMPTS as usize,
                actual: self.max_attempts as usize,
            });
        }
        Ok(())
    }

    /// 已经尝试 `attempts_made` 次之后，是否还允许再试一次。
    pub fn allows_another_attempt(&self, attempts_made: u8) -> bool {
        attempts_made < self.max_attempts
    }
}

/// 候选的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// 申请新观测。
    Observation,
    /// 提请一次动作。
    Action,
    /// 提请一次工具调用。
    Tool,
    /// 提交一条带证据的结论。
    Claim,
}

impl CandidateKind {
    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Action => "action",
            Self::Tool => "tool",
            Self::Claim => "claim",
        }
    }
}

/// 模型必须遵守的输出 schema。
///
/// 这里给的不是一份塞进上下文的 JSON Schema 文本，而是**版本标识 + 本次允许的候选种类**。
/// 理由：让模型"自己读懂 Schema 然后自觉遵守"是把强制点放在它手里；真正的强制在
/// [`ContextBundle::validate_proposals`]，而 Schema 只是提前告诉它边界在哪，省下无效往返。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSchema {
    /// 版本。改变允许的种类必须提升它。
    pub version: u16,
    /// 本次允许返回的候选种类。
    pub allowed: Vec<CandidateKind>,
}

impl OutputSchema {
    /// 只允许提出结论与申请观测。用于只读任务：这类上下文的 `allowed` 里没有 `Action`，
    /// 所以模型即使想提也过不去。
    pub fn read_only() -> Self {
        Self {
            version: MODEL_OUTPUT_SCHEMA_VERSION,
            allowed: vec![CandidateKind::Observation, CandidateKind::Claim],
        }
    }

    /// 允许全部种类。
    pub fn full() -> Self {
        Self {
            version: MODEL_OUTPUT_SCHEMA_VERSION,
            allowed: vec![
                CandidateKind::Observation,
                CandidateKind::Action,
                CandidateKind::Tool,
                CandidateKind::Claim,
            ],
        }
    }

    /// 是否允许某类候选。
    pub fn allows(&self, kind: CandidateKind) -> bool {
        self.allowed.contains(&kind)
    }
}

/// 上下文里的一条证据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSlice {
    /// 证据引用。模型回引时用的就是它。
    pub evidence_ref: EvidenceRef,
    /// 作用对象。
    pub subject_ref: String,
    /// 观测到的值。
    pub observed_value: String,
    /// 数据类别。决定这份上下文能否发往远端（§8）。
    pub data_class: DataClass,
}

/// 局部信念的一条摘要（§8 的"局部 belief 摘要"）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefSummary {
    /// 命题。
    pub statement: String,
    /// 支持它的证据。**每一条都必须在上下文的证据列表里**——否则模型被告知了一个
    /// 它无法核对的结论。
    pub evidence_refs: Vec<EvidenceRef>,
}

/// 过去动作的结果（§8 的"过去动作结果"）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionOutcomeSlice {
    /// 动作标识。
    pub action_id: ActionId,
    /// 所用工具。
    pub tool_id: ToolId,
    /// 后置条件判定。
    pub verdict: Verdict,
    /// 支撑判定的观测。
    pub observation_refs: Vec<EvidenceRef>,
}

/// 能力范围（§8 的"能力范围"）。
///
/// 只读出能力，不读工具句柄。§8 明确"工具句柄在 Core 与存储中，不仅在 LLM 上下文窗口里"，
/// 所以这里给的是**标识**，不是可以拿去调用的东西。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySlice {
    /// 允许调用的工具标识。
    pub tool_ids: Vec<ToolId>,
    /// 允许的最高动作等级。
    pub max_action_level: ActionLevel,
}

/// §8 规定模型调用应当收到的全部内容。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBundle {
    /// 版本。
    pub schema_version: u16,
    /// 当前目标。
    pub goal: String,
    /// 可用证据。
    pub evidence: Vec<EvidenceSlice>,
    /// 局部信念摘要。
    pub belief: Vec<BeliefSummary>,
    /// 过去动作结果。
    pub past_outcomes: Vec<ActionOutcomeSlice>,
    /// 能力范围。
    pub capabilities: CapabilitySlice,
    /// 截止时间。
    pub deadline: WallClock,
    /// 输出 schema。
    pub output_schema: OutputSchema,
    /// 已经记录在案、模型可以引用的预测。
    ///
    /// 模型**不能**自己签发预测引用（§6.3：预测由单元在动作前写下）。它要提动作，就得引用
    /// 这里已有的一条；否则 [`ContextBundle::validate_proposals`] 会拒绝。
    pub recorded_predictions: Vec<PredictionRef>,
}

impl ContextBundle {
    /// 构造并校验。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        goal: impl Into<String>,
        evidence: Vec<EvidenceSlice>,
        belief: Vec<BeliefSummary>,
        past_outcomes: Vec<ActionOutcomeSlice>,
        capabilities: CapabilitySlice,
        deadline: WallClock,
        output_schema: OutputSchema,
        recorded_predictions: Vec<PredictionRef>,
    ) -> Result<Self, ContractError> {
        let bundle = Self {
            schema_version: CONTEXT_SCHEMA_VERSION,
            goal: goal.into(),
            evidence,
            belief,
            past_outcomes,
            capabilities,
            deadline,
            output_schema,
            recorded_predictions,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// 校验上下文自洽性与边界。
    ///
    /// 逐条拒绝：
    /// * 目标为空；
    /// * 各类条目数或总字节数超限；
    /// * 证据引用重复；
    /// * **信念摘要或历史结果引用了不在证据列表里的观测**——那等于告诉模型一个它无法核对
    ///   的结论，而模型的全部输出都必须能回引证据；
    /// * 输出 schema 版本不是本版本。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != CONTEXT_SCHEMA_VERSION {
            return Err(ContractError::SchemaVersionMismatch {
                expected: CONTEXT_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        if self.goal.trim().is_empty() {
            return Err(ContractError::EmptyField {
                field: "context.goal",
            });
        }

        for (field, actual, limit) in [
            (
                "context.evidence",
                self.evidence.len(),
                MAX_CONTEXT_EVIDENCE,
            ),
            ("context.belief", self.belief.len(), MAX_CONTEXT_BELIEFS),
            (
                "context.past_outcomes",
                self.past_outcomes.len(),
                MAX_CONTEXT_OUTCOMES,
            ),
            (
                "context.recorded_predictions",
                self.recorded_predictions.len(),
                MAX_CONTEXT_PREDICTIONS,
            ),
        ] {
            if actual > limit {
                return Err(ContractError::ContextLimitExceeded {
                    field,
                    limit,
                    actual,
                });
            }
        }

        let refs: Vec<EvidenceRef> = self
            .evidence
            .iter()
            .map(|slice| slice.evidence_ref.clone())
            .collect();
        assert_unique(&refs, "context.evidence")?;
        assert_unique(&self.recorded_predictions, "context.recorded_predictions")?;

        for summary in &self.belief {
            for reference in &summary.evidence_refs {
                if !self.knows(reference) {
                    return Err(ContractError::EvidenceNotInContext {
                        evidence_ref: reference.to_string(),
                    });
                }
            }
        }
        for outcome in &self.past_outcomes {
            for reference in &outcome.observation_refs {
                if !self.knows(reference) {
                    return Err(ContractError::EvidenceNotInContext {
                        evidence_ref: reference.to_string(),
                    });
                }
            }
        }

        let encoded =
            serde_json::to_vec(self).map_err(|_| ContractError::EncodingFailed {
                field: "context_bundle",
            })?;
        if encoded.len() > MAX_CONTEXT_BYTES {
            return Err(ContractError::ContextLimitExceeded {
                field: "context.bytes",
                limit: MAX_CONTEXT_BYTES,
                actual: encoded.len(),
            });
        }
        Ok(())
    }

    /// 某条证据是否在本上下文里。
    pub fn knows(&self, reference: &EvidenceRef) -> bool {
        self.evidence
            .iter()
            .any(|slice| &slice.evidence_ref == reference)
    }

    /// 上下文里最高的数据类别。
    pub fn highest_data_class(&self) -> Option<DataClass> {
        self.evidence.iter().map(|slice| slice.data_class).max()
    }

    /// 授权发往某个后端（§8、§4.3）。
    ///
    /// 三条规则：
    /// * 本地 CPU/GPU 后端不做出站判断；
    /// * 远端后端要求**每一条**证据都不是 `Denied`——只要有一条私人内容，整份上下文被拒，
    ///   而不是"把那条滤掉再发"。过滤后再发会让模型看到的结论缺少依据，等于用一个更隐蔽的
    ///   方式产生同一种错误；
    /// * 远端还要求显式授权（§4.3：`remote_authorized`）。内存不足、本地模型不可用都不是
    ///   把私人上下文发到云端的理由。
    pub fn authorize_backend(
        &self,
        backend: ModelBackend,
        remote_authorized: bool,
    ) -> Result<(), ContractError> {
        if backend != ModelBackend::Remote {
            return Ok(());
        }
        for slice in &self.evidence {
            if slice.data_class.cloud_egress() == EgressVerdict::Denied {
                return Err(ContractError::EgressDenied {
                    class: slice.data_class.as_str(),
                });
            }
        }
        if !remote_authorized {
            return Err(ContractError::RemoteNotAuthorized);
        }
        Ok(())
    }

    /// 校验模型返回的提案（§8"模型返回候选后先解析和校验"）。
    ///
    /// 三条检查：
    /// * 提案数不超限；
    /// * 每一条被引用的证据都已经在上下文里；
    /// * 动作提案引用的预测**已经记录在案**——模型不能现编一个预测引用；
    /// * 候选种类在本次允许的范围内。
    pub fn validate_proposals(&self, proposals: &[ModelProposal]) -> Result<(), ContractError> {
        if proposals.len() > MAX_PROPOSALS {
            return Err(ContractError::ProposalLimitExceeded {
                limit: MAX_PROPOSALS,
                actual: proposals.len(),
            });
        }

        for proposal in proposals {
            let kind = proposal.candidate.kind();
            if !self.output_schema.allows(kind) {
                return Err(ContractError::CandidateKindNotAllowed {
                    kind: kind.as_str(),
                });
            }

            for reference in proposal.candidate.evidence_refs() {
                if !self.knows(reference) {
                    return Err(ContractError::EvidenceNotInContext {
                        evidence_ref: reference.to_string(),
                    });
                }
            }

            if let Candidate::RequestAction { intent } = &proposal.candidate
                && !self.recorded_predictions.contains(&intent.prediction_ref)
            {
                return Err(ContractError::PredictionNotRecorded {
                    prediction_ref: intent.prediction_ref.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// 模型返回的一条提案。
///
/// 它**不是**决定。§3.1："LLM 是循环内可替换的推理/解释/生成服务。它提出假设和动作，不独占
/// 信念、记忆、权限、预算或执行权。" 所以这里装的是候选，而候选要经过 L2、L4 与执行许可
/// 三道各自独立的关卡。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProposal {
    /// 候选本身。
    pub candidate: Candidate,
    /// 模型自报的置信度。
    ///
    /// §3.2：这不是概率真值，[`ModelSelfReport`] 没有任何到
    /// [`crate::CalibratedProbability`] 的转换路径。把它当置信度用，就是把一句"我觉得 90%"
    /// 当成了经过校准的分布。
    pub self_report: ModelSelfReport,
    /// 公开简要理由。
    ///
    /// §6 明确："不要求也不存储模型内部隐式思维链。审计记录输入证据、公开简要理由、候选、
    /// 校验结果、动作与观测即可。" 所以这里存的是给人看的理由，不是推理过程。
    pub rationale: String,
}

/// token 用量。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    /// 输入 token。
    pub input_tokens: u32,
    /// 输出 token。
    pub output_tokens: u32,
}

/// 一次模型调用的返回。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOutput {
    /// 输出 schema 版本。
    pub schema_version: u16,
    /// 产出这次结果的模型版本。审计与校准都挂在它上面。
    pub model_version: ModelVersion,
    /// 提案。
    pub proposals: Vec<ModelProposal>,
    /// 用量。
    pub usage: TokenUsage,
    /// 模型是否声称任务已经完成。
    ///
    /// **这个字段不改变任何权限。** §6 第 9 步的"结束"由预算与 L4 决定；一个能自己宣布完工的
    /// 模型，等于把停止条件交给了被监督者。
    pub claims_finished: bool,
}

impl ModelOutput {
    /// 校验。
    pub fn validate(&self, budget: &ModelBudget) -> Result<(), ContractError> {
        if self.schema_version != MODEL_OUTPUT_SCHEMA_VERSION {
            return Err(ContractError::SchemaVersionMismatch {
                expected: MODEL_OUTPUT_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        budget.validate()?;
        if self.usage.output_tokens > budget.max_output_tokens {
            return Err(ContractError::ContextLimitExceeded {
                field: "model_output.usage.output_tokens",
                limit: budget.max_output_tokens as usize,
                actual: self.usage.output_tokens as usize,
            });
        }
        Ok(())
    }

    /// 提案数与总 token 用量，供成本记账使用。
    pub fn total_tokens(&self) -> u32 {
        self.usage.input_tokens.saturating_add(self.usage.output_tokens)
    }
}
