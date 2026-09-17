//! §8 的上下文编译器。
//!
//! §8 把编译器要提供的七项列得很清楚。本模块做两件超出"把字段拼起来"的事：
//!
//! 1. **证据挑选不看分数，看引用。** 一份上下文能带的证据有上限，于是总要丢掉一些。取舍规则
//!    是：**被信念摘要或历史结果引用到的证据优先入选**，其余按输入顺序补足。理由不是效率，
//!    是自洽——丢掉了被引用的那一条，整份包就不成立（[`ContextBundle::validate`] 会拒绝
//!    它）。而"按重要性打分再丢"需要一个可信的打分函数，这里没有；没有它却照丢，会让同一次
//!    任务在两次运行中看到不同的世界，而 §13 要求可复现。
//! 2. **被引用的证据挤不进去时，编译失败，而不是丢掉它们。** 丢掉引用等于让模型看到一个
//!    缺少依据的结论——错误的形式变了，错误的量没变。
//!
//! 出站判断（§8 的"私人数据不得出站"）也在这一步完成，见 [`ContextCompiler::compile`]。

use soca_contracts::{
    ActionOutcomeSlice, BeliefSummary, CapabilitySlice, ContextBundle, ContractError, EvidenceRef,
    EvidenceSlice, ModelBackend, OutputSchema, PredictionRef, WallClock, MAX_CONTEXT_EVIDENCE,
};

use crate::error::GatewayError;

/// 编译器的输入：§8 那七项，加上已记录预测。
#[derive(Clone, Debug, PartialEq)]
pub struct ContextInput {
    /// 当前目标。
    pub goal: String,
    /// 候选证据池。编译器按配额从中挑选。
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
    /// 已经记录在案的预测引用。
    pub recorded_predictions: Vec<PredictionRef>,
}

/// 上下文编译器。
#[derive(Clone, Copy, Debug)]
pub struct ContextCompiler {
    backend: ModelBackend,
    remote_authorized: bool,
    max_evidence: usize,
}

impl ContextCompiler {
    /// 按目标后端构造编译器。
    pub fn new(backend: ModelBackend, remote_authorized: bool) -> Self {
        Self {
            backend,
            remote_authorized,
            max_evidence: MAX_CONTEXT_EVIDENCE,
        }
    }

    /// 收紧本次编译允许携带的证据条数。
    ///
    /// 只能收紧，不能放宽：契约层的硬上限是上下文有界性的最后一道，绕过它就等于让"有界"
    /// 变成一个由调用方自己填的数字。
    pub fn with_max_evidence(mut self, max: usize) -> Self {
        self.max_evidence = max.min(MAX_CONTEXT_EVIDENCE);
        self
    }

    /// 本次针对的后端。
    pub fn backend(&self) -> ModelBackend {
        self.backend
    }

    /// 编译一份上下文。
    ///
    /// 顺序是有意的：先挑证据、再校验结构、**最后做出站判断**。出站判断若放到发送前由调用方
    /// 自行执行，它就成了一个"记得调用"的约定；放在编译出口，一份不该出境的材料根本无法被
    /// 编译出来。
    pub fn compile(&self, input: ContextInput) -> Result<ContextBundle, GatewayError> {
        let evidence = self.select_evidence(&input)?;
        let bundle = ContextBundle::new(
            input.goal,
            evidence,
            input.belief,
            input.past_outcomes,
            input.capabilities,
            input.deadline,
            input.output_schema,
            input.recorded_predictions,
        )?;
        bundle.authorize_backend(self.backend, self.remote_authorized)?;
        Ok(bundle)
    }

    /// 挑选进入上下文的证据。
    ///
    /// 被引用的一律优先，且**不会被丢弃**；配额只用来裁剪没被引用的部分。
    fn select_evidence(&self, input: &ContextInput) -> Result<Vec<EvidenceSlice>, GatewayError> {
        let mut required: Vec<&EvidenceRef> = Vec::new();
        for summary in &input.belief {
            required.extend(summary.evidence_refs.iter());
        }
        for outcome in &input.past_outcomes {
            required.extend(outcome.observation_refs.iter());
        }

        let mut selected: Vec<EvidenceSlice> = Vec::new();
        let mut selected_refs: Vec<EvidenceRef> = Vec::new();

        for reference in required {
            if selected_refs.contains(reference) {
                continue;
            }
            let Some(slice) = input
                .evidence
                .iter()
                .find(|slice| &slice.evidence_ref == reference)
            else {
                // 摘要或历史结果引用了一条输入里根本没有的证据。这不是"少了一条"，
                // 而是上游给了互相矛盾的两份材料——编译不该悄悄替它圆场。
                return Err(ContractError::EvidenceNotInContext {
                    evidence_ref: reference.to_string(),
                }
                .into());
            };
            if selected.len() >= self.max_evidence {
                return Err(ContractError::ContextLimitExceeded {
                    field: "context.evidence(被引用的部分)",
                    limit: self.max_evidence,
                    actual: selected.len().saturating_add(1),
                }
                .into());
            }
            selected_refs.push(reference.clone());
            selected.push(slice.clone());
        }

        for slice in &input.evidence {
            if selected.len() >= self.max_evidence {
                break;
            }
            if selected_refs.contains(&slice.evidence_ref) {
                continue;
            }
            selected_refs.push(slice.evidence_ref.clone());
            selected.push(slice.clone());
        }

        Ok(selected)
    }
}
