//! L3 候选竞争：有预算的生成、检验、选择工作流（§4.1 L3、§6 第 4–5 步）。
//!
//! §4.1 给 L3 的一句话是：
//!
//! > L3候选竞争 | 有预算的生成、检验、选择工作流 | **按需启用，不是常驻全部候选**
//!
//! 而 §6 的两步说明了它到底做什么：
//!
//! > 4. L2验证候选结构与证据存在性；**L3对重要结论做工具验证、反例测试或独立来源核验**。
//! >    低风险纯格式任务不强行编造反方观点。
//! > 5. L4比较代价与收益、保留未决冲突，给出明确动作意图或"需要更多信息"；
//! >    **错误代价高时提高证据门槛，不靠置信度投票绕过审批。**
//!
//! 三条本模块负责的性质：
//!
//! 1. **不靠置信度。** 选择时唯一参与比较的量是证据条数与独立检验结果。这不是靠纪律做到的
//!    ——[`crate::Candidate`] 里根本没有置信度字段可以让它参与进来。模型自评的数值留在
//!    [`crate::ModelSelfReport`] 里，它没有任何路径通向这里（§3.2）。
//! 2. **门槛随风险上升。** [`SelectionPolicy::evidence_bar`] 是风险等级的函数。§6 第 5 步
//!    要求"错误代价高时提高证据门槛"，那句话如果只是一个提醒，就等于没有。
//! 3. **未决冲突不被消解。** 只要候选集合里还有冲突或未决问题，就不选出任何一条。§4.2 禁止
//!    "用多数意见覆盖矛盾"，而"选一条了事"正是覆盖的一种形式。
//!
//! 一处刻意的范围限定，写在这里免得被误读：**证据门槛只对结论类候选
//! （[`crate::Candidate::Claim`]）生效。** 申请观测、请求工具、提请动作不是"关于世界的断言"，
//! 而是行动请求；对它们的把关在
//! 别处（L2 的证据存在性、执行许可的参数绑定与审批）。给它们套一个证据门槛，只会逼出
//! "为了过门槛而编一条证据"这种行为。

use serde::{Deserialize, Serialize};

use crate::{ActionLevel, CandidateKind, CandidateSet, ContractError, EvidenceRef, Verdict};

/// 检验的种类（§4.1 L3、§6 第 4 步）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    /// 工具验证：用确定性计算或检查来核验。
    Tool,
    /// 反例测试：主动找一个能让结论不成立的例子。
    CounterExample,
    /// 独立来源核验：用另一条不依赖同一来源的证据交叉核对。
    IndependentSource,
    /// 证据可用性：这条候选引用的证据**还存在且仍可访问吗**（§7.2）。
    ///
    /// 这一条与上面三种**不是同一类东西**，放在同一个枚举里是因为它们共用同一条上报通路。
    /// 上面三种是 §6 第 4 步点名的**检验方法**；这一条是 §7.2 给所有引用定的**有效性前提**：
    ///
    /// > 引用必须能解析为存在**且仍可访问**的证据。缺失来源、过期证据、**权限变化**和数据
    /// > 撤回都可使候选失效。
    ///
    /// 单列成一档，是因为它报出的问题与别的最不一样：结论与证据**完全自洽**，证据也确实
    /// 存在过——只是它现在不能用了。和"值对不上"混在同一个判定里，看审计的人会去查推导
    /// 过程，而真正该看的是"谁在什么时候把权限收回了"。
    EvidenceAccess,
}

impl VerificationKind {
    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::CounterExample => "counter_example",
            Self::IndependentSource => "independent_source",
            Self::EvidenceAccess => "evidence_access",
        }
    }
}

/// 一次检验的结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationOutcome {
    /// 用了哪种检验。
    pub kind: VerificationKind,
    /// 判定。
    pub verdict: Verdict,
    /// 支撑判定的证据。
    pub evidence_refs: Vec<EvidenceRef>,
}

/// 一条候选的检验档案。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateReview {
    /// 候选在集合里的下标。
    pub candidate_index: usize,
    /// 对它做过的检验。
    pub outcomes: Vec<VerificationOutcome>,
}

impl CandidateReview {
    /// 构造。
    pub fn new(candidate_index: usize, outcomes: Vec<VerificationOutcome>) -> Self {
        Self {
            candidate_index,
            outcomes,
        }
    }

    /// 是否被某次检验否定过。
    ///
    /// 被否定过的候选直接出局，**不论它有多少证据**。一条被反例推翻的结论不会因为支持者多
    /// 就重新成立——这正是"不靠置信度投票"最直白的样子。
    pub fn is_refuted(&self) -> bool {
        self.outcomes
            .iter()
            .any(|outcome| outcome.verdict == Verdict::Refuted)
    }

    /// 独立来源核验支持了几次。
    ///
    /// 只有 [`VerificationKind::IndependentSource`] 计入：同一条证据被工具检查两次不构成
    /// 两次独立支持（§7.2 的证据去重原则）。`Inconclusive` 也不计入——无法判定不是支持。
    pub fn independent_supports(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| {
                outcome.kind == VerificationKind::IndependentSource
                    && outcome.verdict == Verdict::Supported
            })
            .count()
    }

    /// 本次档案里所有的检验证据。
    pub fn evidence_refs(&self) -> Vec<&EvidenceRef> {
        self.outcomes
            .iter()
            .flat_map(|outcome| outcome.evidence_refs.iter())
            .collect()
    }
}

/// 选择的走向（§6 第 5 步）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionOutcome {
    /// 选定一条候选推进。
    Selected {
        /// 候选在集合里的下标。
        index: usize,
    },
    /// 需要更多信息（§6 第 5 步的"请求澄清"）。
    NeedsMoreInformation {
        /// 缺什么。
        missing: Vec<String>,
    },
    /// 集合里没有可推进的候选。
    NothingToPursue,
}

/// 一次选择的完整记录。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// 走向。
    pub outcome: SelectionOutcome,
    /// 本次用到的检验档案。
    pub reviews: Vec<CandidateReview>,
    /// 本次采用的证据门槛。
    pub required_evidence: usize,
    /// 为什么这样选。给人读，不参与判定。
    pub rationale: String,
}

impl Selection {
    /// 被选中的候选下标。
    pub fn selected_index(&self) -> Option<usize> {
        match self.outcome {
            SelectionOutcome::Selected { index } => Some(index),
            _ => None,
        }
    }
}

/// 选择策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionPolicy {
    /// 低风险任务要求的证据条数。
    pub base_evidence: usize,
    /// 高风险任务在此基础上额外要求的条数。
    pub high_risk_extra: usize,
    /// 从哪个风险等级起算高风险。
    pub high_risk_from: ActionLevel,
    /// 一次选择允许消耗的最大检验次数（"有预算的……工作流"）。
    pub max_checks: usize,
}

impl Default for SelectionPolicy {
    fn default() -> Self {
        Self {
            base_evidence: 1,
            high_risk_extra: 2,
            // A2 起算高风险：它会改动目标对象。A0/A1 只读或本地计算。
            high_risk_from: ActionLevel::A2,
            max_checks: 8,
        }
    }
}

impl SelectionPolicy {
    /// 给定风险等级下的证据门槛（§6 第 5 步）。
    ///
    /// 逐级上升，不是"高风险就一律很高"：门槛是风险的函数这一点要能被测试，
    /// 否则它只是一个写死的数字，而写死的数字在需要它的场合不会有任何作用。
    pub fn evidence_bar(&self, risk: ActionLevel) -> usize {
        if risk >= self.high_risk_from {
            self.base_evidence.saturating_add(self.high_risk_extra)
        } else {
            self.base_evidence
        }
    }
}

/// 从候选集合里选出一条推进（§6 第 5 步）。
///
/// 判定顺序是有意的：
///
/// 1. 结构先过一遍（§4.1 L3 的工作对象必须是合法集合）；
/// 2. 检验次数不超预算；
/// 3. **未决冲突先于一切**——只要还有，就不选，返回缺什么；
/// 4. 逐条淘汰：被检验否定的出局；结论类未达证据门槛的出局；
/// 5. 在剩下的里面挑证据最多的；并列时取先出现的（确定性，§13 要求可复现）；
/// 6. 一条都没剩下时，未决问题才成为结论（"需要更多信息"）。
///
/// 第 3 步与第 6 步的区别不是措辞上的。**冲突**是"同一个对象上有两个不能同时成立的结论"，
/// 此时选一条就是用决定覆盖矛盾（§4.2 明令禁止）。**未决问题**往往只是"某件事还没查"——
/// 让后者阻塞前者，会让一个簇只要有一个槽位在等证据，就再也选不出任何东西；而在一个真实
/// 的簇里，"某个槽位在等证据"是常态，不是异常。这个区别是本模块与检验器接起来之后才暴露的：
/// 之前两者各自都通过测试，接上之后 `ActionPrecondition` 的未决问题把每一条结论都挡下了。
pub fn select(
    candidates: &CandidateSet,
    reviews: Vec<CandidateReview>,
    policy: &SelectionPolicy,
    risk: ActionLevel,
) -> Result<Selection, ContractError> {
    candidates.validate()?;

    let checks: usize = reviews.iter().map(|review| review.outcomes.len()).sum();
    if checks > policy.max_checks {
        return Err(ContractError::ContextLimitExceeded {
            field: "selection.checks",
            limit: policy.max_checks,
            actual: checks,
        });
    }

    let bar = policy.evidence_bar(risk);

    let conflicts: Vec<String> = candidates
        .conflicts
        .iter()
        .map(|conflict| format!("冲突未消解：{}", conflict.subject_ref))
        .collect();
    if !conflicts.is_empty() {
        return Ok(Selection {
            outcome: SelectionOutcome::NeedsMoreInformation { missing: conflicts },
            reviews,
            required_evidence: bar,
            rationale: "存在未消解的冲突；按 §4.2 不用决定覆盖矛盾。".to_string(),
        });
    }

    let mut eligible: Vec<(usize, usize, usize)> = Vec::new();
    for (index, candidate) in candidates.candidates.iter().enumerate() {
        let review = reviews.iter().find(|review| review.candidate_index == index);
        if review.is_some_and(CandidateReview::is_refuted) {
            continue;
        }

        let evidence = candidate.evidence_refs().len();
        if candidate.kind() == CandidateKind::Claim && evidence < bar {
            // 结论类候选必须达标。这里**不**替它补证据，也不降门槛：编一条证据来过关，
            // 正是证据门槛存在的意义所在。
            continue;
        }

        let independent = review.map_or(0, CandidateReview::independent_supports);
        eligible.push((index, evidence, independent));
    }

    if eligible.is_empty() {
        // 一条都没剩下，这时未决问题才有资格成为结论。
        let missing: Vec<String> = candidates
            .unresolved
            .iter()
            .map(|unresolved| {
                format!(
                    "{}（缺：{}）",
                    unresolved.question,
                    unresolved.missing.join("、")
                )
            })
            .collect();

        return Ok(Selection {
            outcome: if missing.is_empty() {
                SelectionOutcome::NothingToPursue
            } else {
                SelectionOutcome::NeedsMoreInformation { missing }
            },
            reviews,
            required_evidence: bar,
            rationale: format!("没有候选达到要求：证据门槛 {bar}，或全部被检验否定。"),
        });
    }

    // 证据多者优先；并列时独立来源多者优先；再并列取先出现的。
    // **没有任何一步用到模型的置信度**——候选里也没有那个字段。
    eligible.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then(right.2.cmp(&left.2))
            .then(left.0.cmp(&right.0))
    });
    let (index, evidence, independent) = eligible[0];

    // 未决问题不阻塞，但也不能被吞掉：选中的理由里要如实带上还有哪些问题悬着，
    // 否则一次"选好了"看起来像"什么都清楚了"。
    let outstanding = candidates.unresolved.len();
    let tail = if outstanding == 0 {
        String::new()
    } else {
        format!(" 另有 {outstanding} 个未决问题仍未回答。")
    };

    Ok(Selection {
        outcome: SelectionOutcome::Selected { index },
        reviews,
        required_evidence: bar,
        rationale: format!(
            "候选 #{index}：{evidence} 条证据、{independent} 次独立来源支持；门槛 {bar}。{tail}"
        ),
    })
}
