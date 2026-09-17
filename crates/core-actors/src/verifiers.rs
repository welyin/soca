//! L3 的检验器（§4.3「证据与风险评估」、§6 第 4 步）。
//!
//! §6 第 4 步点名了三种检验：
//!
//! > L3 对重要结论做**工具验证、反例测试或独立来源核验**。低风险纯格式任务不强行编造
//! > 反方观点。
//!
//! 本模块实现其中三种可机械判定的形式，对应 §4.3「证据与风险评估」那张表里的三个槽：
//! 「来源核对」、「反方假设」、「代码/工具验证」。表里其余五个槽（概率校准、语义风险、
//! 隐私分类、失败复盘、冲突检测）**没有**在这里假装实现——冲突检测由
//! [`soca_contracts::CandidateSet::conflicts`] 那一层承担，其余四个需要各自的判据，
//! 而一个没有判据的检验器只会往审计账里塞"已检验"的标记。
//!
//! 三条贯穿本模块的原则：
//!
//! 1. **不适用就说"不适用"，不假装核对过了。** 每个检验返回 `Option`，`None` 表示"这条
//!    候选上没有它能核的东西"。把"不适用"写成 `Inconclusive`，会让每次审查都看起来做了
//!    三件事，而实际上可能一件也没做。
//! 2. **"没找到反例"不等于"结论成立"。** 反例搜索空手而归时报 `Inconclusive`，不报
//!    `Supported`。两者混同，等于把"没查出来"当成"没问题"。
//! 3. **检验只看证据，不看模型自评。** 与 L3 的选择同一原则（§3.2）。

use soca_contracts::{
    ActionLevel, Candidate, CandidateReview, CandidateSet, EvidenceRef, VerificationKind,
    VerificationOutcome, Verdict,
};

use crate::evidence::EvidenceLedger;

/// 一次审查的策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReviewPolicy {
    /// 是否搜索反例。§6 第 4 步："低风险纯格式任务不强行编造反方观点。"
    pub counter_example: bool,
    /// 本次审查允许产生的最大检验条数。
    ///
    /// 必须与 [`soca_contracts::SelectionPolicy::max_checks`] 对齐：那个上限是选择阶段的
    /// 硬约束，超了会直接报错。两个数字对不上时，表现是"审查跑完了，选择却报预算超支"。
    pub max_checks: usize,
}

impl ReviewPolicy {
    /// 按风险等级决定要不要找反例。
    ///
    /// 那句"不强行编造"里的**编造**是关键：对一份纯格式化的工作，反方观点不是找不到，
    /// 而是根本不存在。硬凑一个只会让审计账里塞满"已检验"的标记，而实际上什么也没检验。
    ///
    /// `max_checks` 由调用方传入而不是在这里取默认值：它与
    /// [`soca_contracts::SelectionPolicy::max_checks`] 必须是同一个数字，而两个各自取默认值的
    /// 字段迟早会分叉——表现是"审查跑完了，选择却报预算超支"。
    pub fn for_risk(risk: ActionLevel, high_risk_from: ActionLevel, max_checks: usize) -> Self {
        Self {
            counter_example: risk >= high_risk_from,
            max_checks,
        }
    }
}

/// 独立来源核验（§4.3「来源核对」）。
///
/// 数的是命题引用的证据里**互不相同**的来源有几个。两条证据互相独立当且仅当来源链没有交集、
/// 且观测者不是同一个单元——理由见 [`crate::evidence::EvidenceRecord::is_independent_of`]。
///
/// 判定：
/// * 两个及以上独立来源 → `Supported`；
/// * 只有一个 → `Inconclusive`。这不是失败，是事实：结论没有得到交叉核对，
///   而说成"支持"会让一个孤证看起来像两个。
pub fn check_source_independence(
    claim: &Candidate,
    ledger: &EvidenceLedger,
) -> Option<VerificationOutcome> {
    let Candidate::Claim { evidence_refs, .. } = claim else {
        return None;
    };

    let independent = ledger.independent_sources(evidence_refs);
    if independent.is_empty() {
        // 台账里一条都查不到。L2 本该挡住这种引用，走到这里说明核对无从下手——
        // 报"不适用"比报一个凭空的判定诚实。
        return None;
    }

    Some(VerificationOutcome {
        kind: VerificationKind::IndependentSource,
        verdict: if independent.len() >= 2 {
            Verdict::Supported
        } else {
            Verdict::Inconclusive
        },
        evidence_refs: independent
            .iter()
            .map(|record| record.evidence_ref.clone())
            .collect(),
    })
}

/// 反例搜索（§4.3「反方假设」）。
///
/// 找的是**同一个对象的另一个观测值**：命题依据观测到 X，而台账里还有一条对同一对象的观测
/// 说它现在是 Y。这是最具体的一类反例——它不是"我觉得你不对"，而是一条同样可核验的观测。
///
/// 找不到时报 `Inconclusive`（详见模块文档第 2 条）。
pub fn search_counter_example(
    claim: &Candidate,
    ledger: &EvidenceLedger,
) -> Option<VerificationOutcome> {
    let Candidate::Claim { evidence_refs, .. } = claim else {
        return None;
    };

    // 命题自己认下的（对象，值）对。
    let mut endorsed: Vec<(String, String)> = Vec::new();
    for reference in evidence_refs {
        if let Some(record) = ledger.get(reference) {
            endorsed.push((record.subject_ref.clone(), record.observed_value.clone()));
        }
    }
    if endorsed.is_empty() {
        return None;
    }

    let mut contradicting: Vec<EvidenceRef> = Vec::new();
    for (subject, value) in &endorsed {
        for record in ledger.about(subject) {
            let same_subject_different_value = record.observed_value != *value;
            let not_self_cited = !evidence_refs.contains(&record.evidence_ref);
            if same_subject_different_value
                && not_self_cited
                && !contradicting.contains(&record.evidence_ref)
            {
                contradicting.push(record.evidence_ref.clone());
            }
        }
    }

    Some(VerificationOutcome {
        kind: VerificationKind::CounterExample,
        verdict: if contradicting.is_empty() {
            Verdict::Inconclusive
        } else {
            Verdict::Refuted
        },
        evidence_refs: contradicting,
    })
}

/// 结论依据核对（§4.3「代码/工具验证」里可确定的那一类）。
///
/// 查的是**命题与它自己引用的证据是否自洽**：命题里出现的每个可核对的值，都必须是它自己
/// 引用的证据里的某一个值。
///
/// 挡的是一类很具体的错误——模型引用下标 0（一条真实存在的证据），却断言别的东西。比如
/// 证据说"版本是 `sha256:aaa`"，命题写"版本是 `sha256:bbb`"。结构校验看不出来：证据引用
/// 合法、下标存在、证据确实在上下文里。只有把命题文本和证据值对一遍才发现得了。
///
/// 命题没有断言任何可核对的值时（例如"摘要文件已经更新"这种散文式结论）返回 `None`：
/// 这个工具对它不适用，**不假装核对过了**。
pub fn check_claim_grounding(
    claim: &Candidate,
    ledger: &EvidenceLedger,
) -> Option<VerificationOutcome> {
    let Candidate::Claim {
        statement,
        evidence_refs,
    } = claim
    else {
        return None;
    };

    let asserted: Vec<String> = ledger
        .known_values()
        .into_iter()
        .filter(|value| statement.contains(value))
        .map(str::to_string)
        .collect();
    if asserted.is_empty() {
        return None;
    }

    let cited: Vec<String> = evidence_refs
        .iter()
        .filter_map(|reference| ledger.get(reference))
        .map(|record| record.observed_value.clone())
        .collect();

    let unsupported: Vec<&String> = asserted
        .iter()
        .filter(|value| !cited.contains(value))
        .collect();
    if unsupported.is_empty() {
        return Some(VerificationOutcome {
            kind: VerificationKind::Tool,
            verdict: Verdict::Supported,
            evidence_refs: evidence_refs.clone(),
        });
    }

    // 命题断言了一个它自己没引用的值。台账里真正持这个值的那些记录，就是"它本该引用却
    // 没引用"的证据——把它们的引用写进判定，审计时才看得出它错在哪一条上。
    let mut culprits: Vec<EvidenceRef> = Vec::new();
    for record in ledger.records() {
        if unsupported.iter().any(|value| **value == record.observed_value)
            && !culprits.contains(&record.evidence_ref)
        {
            culprits.push(record.evidence_ref.clone());
        }
    }

    Some(VerificationOutcome {
        kind: VerificationKind::Tool,
        verdict: Verdict::Refuted,
        evidence_refs: culprits,
    })
}

/// 把三类检验在一条候选上跑一遍。
pub fn review_candidate(
    index: usize,
    candidate: &Candidate,
    ledger: &EvidenceLedger,
    policy: &ReviewPolicy,
) -> CandidateReview {
    let mut outcomes = Vec::new();
    if let Some(outcome) = check_claim_grounding(candidate, ledger) {
        outcomes.push(outcome);
    }
    if let Some(outcome) = check_source_independence(candidate, ledger) {
        outcomes.push(outcome);
    }
    if policy.counter_example
        && let Some(outcome) = search_counter_example(candidate, ledger)
    {
        outcomes.push(outcome);
    }
    CandidateReview::new(index, outcomes)
}

/// 审查整个候选集合。
///
/// 按候选顺序推进，**用满预算即停**。停在原处而不是"每个候选轮一点"，是因为预算耗尽在这里
/// 意味着"这次审查只看完了前几条"，而那是一个比"每条都只看了一点点"更容易解释的状态——
/// 后者的结果是没有任何一条候选得到了完整核对。停止的位置是确定的，因此可复现（§13）。
pub fn review_all(
    candidates: &CandidateSet,
    ledger: &EvidenceLedger,
    policy: &ReviewPolicy,
) -> Vec<CandidateReview> {
    let mut reviews = Vec::new();
    let mut spent = 0usize;
    for (index, candidate) in candidates.candidates.iter().enumerate() {
        if spent >= policy.max_checks {
            break;
        }
        let review = review_candidate(index, candidate, ledger, policy);
        spent = spent.saturating_add(review.outcomes.len());
        // 即便一条检验也没跑成，也要留下档案：`select` 按下标找档案，缺了它就只能当成
        // "没有检验记录"，而这两者在审计里含义不同。
        reviews.push(review);
    }
    reviews
}
