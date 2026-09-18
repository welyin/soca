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

use crate::{
    ActionLevel, CandidateKind, CandidateSet, ContractError, EvidenceRef, Verdict,
};

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
    /// 证据新鲜度：这条候选引用的证据，是不是**已经被同一个对象上更晚的观测取代了**。
    ///
    /// 与 [`VerificationKind::CounterExample`] 分开，是因为两者**该不该跑**的条件完全不同：
    ///
    /// * 反例搜索是**主动找茬**。§6 第 4 步明说"低风险纯格式任务不强行编造反方观点"——
    ///   它是对抗性的，代价是可能逼出编造的反方观点，所以按风险开关。
    /// * 新鲜度不是找茬，是**核对一个已经记在账上的事实**：同一个对象上有一条更晚的观测
    ///   说了另一个值。§15.1 第 5 步那句"**文件已变化则失效草稿并重新核验**"说的就是它，
    ///   而那句话没有任何风险等级限定。
    ///
    /// 与 [`VerificationKind::EvidenceAccess`] 也分开：那一条是"证据没了"，这一条是"证据旧了"。
    /// 处置完全不同——一个去重新授权，一个去重新观测。
    EvidenceFreshness,
}

impl VerificationKind {
    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::CounterExample => "counter_example",
            Self::IndependentSource => "independent_source",
            Self::EvidenceAccess => "evidence_access",
            Self::EvidenceFreshness => "evidence_freshness",
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

/// 一轮里允许进入核验的**正式候选**数（§13.1）。
///
/// §13.1 的原话是"一个主体默认**一个当前焦点**、最**多 8 个待核验正式候选**"。
///
/// 与 [`crate::MAX_CANDIDATES`] 分开，是因为两者管的是**两件事**：
///
/// * [`crate::MAX_CANDIDATES`]` = 32` 是 §4.1 L2 的"黑板有界"——一轮里能**提出**多少条。
/// * 本常量是 §13.1 的"待核验"上限——**送去核验**多少条。
///
/// 差别是实的：核验要花预算（§4.1 L3 的"有预算的……工作流"，见
/// [`SelectionPolicy::max_checks`]）。不设这道闸的话，"提出 32 条"就等于"核验 32 条"，
/// 而那正是 §13.1 要防的拥塞——预算被摊薄到每条只够查一下，于是**没有一条被查清**。
pub const MAX_FORMAL_CANDIDATES: usize = 8;

/// 一条被拒的候选**什么条件下可以重来**（§13.1）。
///
/// §13.1 的原话是"拒绝有原因和**可重试条件**"。两者缺一不可，而且是两个字段：
/// 原因回答"为什么不行"，可重试条件回答"**接下来该做什么**"。
///
/// 少了它，一条被拒的候选在界面上与"系统根本没看见它"长得一样——而操作员能做的事恰恰
/// 取决于它是哪一类：补证据、批一次、等一等，还是换一条路。把这份判断留在拒绝理由的散文里，
/// 界面就只能把整段话原样贴出来让人自己读，而调度器则连"该不该过一会儿再试"都判断不了。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryWhen {
    /// **此路不通。** 等多久都没用——换一条候选，或者重新授权、重新委托。
    Never,
    /// 补证据：还差 `short_by` 条。
    MoreEvidence {
        /// 距离门槛还差多少条。
        short_by: usize,
    },
    /// 需要一次覆盖这次具体动作的人工批准（§12.1 的 A2/A3）。
    WhenApproved,
    /// 等暂停结束。此刻不通，而"此刻"会过去（§12.1）。
    ///
    /// 与 [`RetryWhen::Never`] 分开，正是 [`crate::PermitDecision`] 里
    /// [`Paused`](crate::PermitDecision::Paused) 与 [`Refused`](crate::PermitDecision::Refused)
    /// 分开的那个理由，只是在这里落成了一个可判定的取值。
    WhenUnpaused,
    /// 等名额腾出来（§13.1 的 8 条上限）。
    ///
    /// 与 [`RetryWhen::WhenNextRound`] 分开，尽管两者都是"下一轮可能就好了"：
    /// 这一条说明它**根本没进核验**，而那一条说明它进了核验、只是没赢。前者是拥塞，
    /// 后者是竞争——把它们并成一个取值，界面就只能显示同一句话，而"我没排上队"
    /// 与"我比了但没赢"对提出方是完全不同的反馈。
    WhenCapacityFreed {
        /// 当前占着名额的正式候选数。
        held_by: usize,
    },
    /// 这一轮的机会给了别人。**它不是失败。**
    ///
    /// 进了核验、够格、但另一条更强（证据更多、独立来源更多，或者只是排在前面）。
    /// 下一轮它会被重新提出，而那时可能就轮到它了。
    WhenNextRound,
    /// 等那条把它否掉的证据发生变化：被撤回、被更新的观测取代，或者重新授权之后又可用。
    ///
    /// 归成一档而不是按 [`VerificationKind`] 拆开，是因为**操作员要做的是同一件事**：
    /// 去看那条证据现在是什么样。拆开只会让界面多出三个长得一样的按钮。
    WhenEvidenceChanges,
}

impl RetryWhen {
    /// 稳定名称，用于审计记录与界面。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::MoreEvidence { .. } => "more_evidence",
            Self::WhenApproved => "when_approved",
            Self::WhenUnpaused => "when_unpaused",
            Self::WhenCapacityFreed { .. } => "when_capacity_freed",
            Self::WhenNextRound => "when_next_round",
            Self::WhenEvidenceChanges => "when_evidence_changes",
        }
    }

    /// 等一等有没有可能变好。`false` 表示"此路不通"。
    ///
    /// 调度器要的就是这一个比特：它决定"这个目标该继续挂着还是该结束"。
    /// 让调用方自己 `match` 的话，每加一个变体都会在某个调用点上悄悄变成"再等等"。
    pub fn is_retryable(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// 一条没有被选中的候选，以及为什么（§13.1）。
///
/// 拒绝要**逐条**记下来，而不是只留一句汇总。原因具体：一条候选被拒意味着它背后的那个
/// 子单元白干了一轮，而它下一轮该不该再提这一条，取决于它是"证据差一条"还是"名额满了"。
/// 汇总成"本轮没有合适的候选"的话，两种情形长得一样。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rejection {
    /// 候选在集合里的下标。
    pub candidate_index: usize,
    /// 为什么不行。给人读。
    pub reason: String,
    /// 什么条件下可以重来。
    pub retry_when: RetryWhen,
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
    /// 没有进入核验、或者进过核验但没被选中的候选，各自为什么（§13.1）。
    ///
    /// **每一条候选都在这里或者在被选中的位置上，没有第三种去处。** 此前它们是"没被选中"
    /// 就消失了——而"消失"与"被拒"在界面上看起来一样，在调度器那里也一样。
    pub rejections: Vec<Rejection>,
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

    /// 某条候选被拒的理由。
    pub fn rejection_for(&self, candidate_index: usize) -> Option<&Rejection> {
        self.rejections
            .iter()
            .find(|rejection| rejection.candidate_index == candidate_index)
    }

    /// 这一轮有没有"再等等就会变好"的东西。
    ///
    /// 给调度器用：一条都没有、又没选中任何东西时，这个目标是真的走完了，
    /// 而不是"碰巧这一轮没轮到"。
    pub fn has_retryable_rejection(&self) -> bool {
        self.rejections
            .iter()
            .any(|rejection| rejection.retry_when.is_retryable())
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

    // 一条候选的排序键：证据条数、独立来源次数、出现顺序。
    //
    // **这不是 §13.1 的焦点评分。** §13.1 列了六项（任务相关度、预期收益、证据新颖度、
    // 截止期、风险、资源成本），而这里只有"证据多、独立来源多、先出现的先"——它是一套
    // 确定性的**排序**，不是一套评分。差别是实的：少了相关度与收益这两项，系统在两条都对、
    // 都便宜的候选之间按证据数量挑，而不是按"哪一条更接近目标"挑。写在这里，是为了不让
    // 人以为 §13.1 的评分已经实现了。
    let rank = |index: usize| -> (usize, usize, usize) {
        let review = reviews.iter().find(|review| review.candidate_index == index);
        (
            candidates.candidates[index].evidence_refs().len(),
            review.map_or(0, CandidateReview::independent_supports),
            // 越先出现的越大，于是在降序比较里排在前面。
            usize::MAX - index,
        )
    };

    let mut rejections: Vec<Rejection> = Vec::new();

    // §13.1 的容量闸。**先于一切别的判定**：它管的是"送去核验多少条"，而不是"哪一条通过"。
    // 放到后面就等于先给 32 条各查了一遍再挑 8 条——而核验预算被摊薄正是拥塞的成因。
    let mut order: Vec<usize> = (0..candidates.candidates.len()).collect();
    // `Reverse` 而不是在闭包里把两边换个位置：排序键的三个分量都是"越大越优先"，
    // 写成比较函数的话，读的人得自己把每一维的方向推一遍。
    order.sort_by_key(|index| std::cmp::Reverse(rank(*index)));
    let formal: Vec<usize> = order.iter().copied().take(MAX_FORMAL_CANDIDATES).collect();
    for (position, index) in order.iter().enumerate().skip(MAX_FORMAL_CANDIDATES) {
        rejections.push(Rejection {
            candidate_index: *index,
            reason: format!(
                "本轮待核验名额已满（§13.1 的上限 {MAX_FORMAL_CANDIDATES}），它排在第 {} 位",
                position.saturating_add(1)
            ),
            retry_when: RetryWhen::WhenCapacityFreed {
                held_by: MAX_FORMAL_CANDIDATES,
            },
        });
    }

    let conflicts: Vec<String> = candidates
        .conflicts
        .iter()
        .map(|conflict| format!("冲突未消解：{}", conflict.subject_ref))
        .collect();
    if !conflicts.is_empty() {
        // 冲突挡下的是**全部**候选，所以全部都要有条目。只报冲突不报候选的话，
        // 一次"什么也没选"看起来像"本来就没有候选"。
        for index in &formal {
            rejections.push(Rejection {
                candidate_index: *index,
                reason: format!("存在未消解的冲突：{}", conflicts.join("；")),
                retry_when: RetryWhen::WhenEvidenceChanges,
            });
        }
        rejections.sort_by_key(|rejection| rejection.candidate_index);
        return Ok(Selection {
            outcome: SelectionOutcome::NeedsMoreInformation { missing: conflicts },
            reviews,
            required_evidence: bar,
            rejections,
            rationale: "存在未消解的冲突；按 §4.2 不用决定覆盖矛盾。".to_string(),
        });
    }

    let mut eligible: Vec<(usize, usize, usize)> = Vec::new();
    for index in &formal {
        let index = *index;
        let review = reviews.iter().find(|review| review.candidate_index == index);
        if review.is_some_and(CandidateReview::is_refuted) {
            // 报出**是哪一类检验**否掉的。只说"被否定"的话，操作员得自己去翻档案，
            // 而翻出来之后要做的事恰恰取决于答案：证据失效要去重新授权，
            // 反例成立要去看那条反例还在不在。
            let kinds: Vec<&str> = review
                .map(|review| {
                    review
                        .outcomes
                        .iter()
                        .filter(|outcome| outcome.verdict == Verdict::Refuted)
                        .map(|outcome| outcome.kind.as_str())
                        .collect()
                })
                .unwrap_or_default();
            rejections.push(Rejection {
                candidate_index: index,
                reason: format!("被检验否定：{}", kinds.join("、")),
                retry_when: RetryWhen::WhenEvidenceChanges,
            });
            continue;
        }

        let evidence = candidates.candidates[index].evidence_refs().len();
        if candidates.candidates[index].kind() == CandidateKind::Claim && evidence < bar {
            // 结论类候选必须达标。这里**不**替它补证据，也不降门槛：编一条证据来过关，
            // 正是证据门槛存在的意义所在。
            rejections.push(Rejection {
                candidate_index: index,
                reason: format!("结论类候选未达证据门槛：{evidence} 条，门槛 {bar}"),
                // `bar - evidence` 是**可操作的**：它说的不是"还不够"，而是"还差几条"。
                retry_when: RetryWhen::MoreEvidence {
                    short_by: bar - evidence,
                },
            });
            continue;
        }

        let independent = review.map_or(0, CandidateReview::independent_supports);
        eligible.push((index, evidence, independent));
    }

    if eligible.is_empty() {
        rejections.sort_by_key(|rejection| rejection.candidate_index);
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
            rejections,
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

    // 进了核验、够格、但没被选中的那些，也要有条目。理由有两种，而且调度器要能分开：
    // "它够格，只是这一轮轮不到"（`WhenCapacityFreed`，下一轮很可能就轮到）与
    // "它压根不够格"（`MoreEvidence`，得先去补东西）。
    for (other, other_evidence, other_independent) in eligible.iter().skip(1) {
        rejections.push(Rejection {
            candidate_index: *other,
            reason: format!(
                "并列中落选：{other_evidence} 条证据、{other_independent} 次独立来源支持，\
                 不高于选中那条的 {evidence}／{independent}"
            ),
            // 不是"名额满了"：它**进了核验**，只是没赢。两类反馈对提出方不一样，
            // 所以取值也不一样。
            retry_when: RetryWhen::WhenNextRound,
        });
    }
    rejections.sort_by_key(|rejection| rejection.candidate_index);

    // 未决问题不阻塞，但也不能被吞掉：选中的理由里要如实带上还有哪些问题悬着，
    // 否则一次"选好了"看起来像"什么都清楚了"。
    let outstanding = candidates.unresolved.len();
    let tail = if outstanding == 0 {
        String::new()
    } else {
        format!(" 另有 {outstanding} 个未决问题仍未回答。")
    };
    let dropped = rejections.len();
    let dropped_tail = if dropped == 0 {
        String::new()
    } else {
        format!(" 另有 {dropped} 条候选各有理由地出局（见 rejections）。")
    };

    Ok(Selection {
        outcome: SelectionOutcome::Selected { index },
        reviews,
        required_evidence: bar,
        rejections,
        rationale: format!(
            "候选 #{index}：{evidence} 条证据、{independent} 次独立来源支持；门槛 {bar}。{tail}{dropped_tail}"
        ),
    })
}
