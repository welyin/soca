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
//! 还有一条**不是检验方法**的判定，但它必须走同一条通路：§7.2 的证据可用性。见
//! [`check_evidence_access`]。
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
    ActionLevel, BlobRef, Candidate, CandidateReview, CandidateSet, EvidenceRef, VerificationKind,
    VerificationOutcome, Verdict,
};
use soca_storage::ContentStore;

use crate::evidence::EvidenceLedger;

/// 核对时按引用取正文的那一头。
///
/// 做成 trait，而不是让验证器直接收一个 `&ContentStore`：验证器要的是**正文**，不关心它存在
/// 文件里、数据库里还是别处。收一个具体的仓，会让"这批验证器只能跑在有磁盘的环境里"变成一条
/// 隐式的编译期约束——一条只想给一份假正文的单元测试，得先建一个临时目录。
pub trait BodySource {
    /// 取一条正文。返回 `None` 表示**这条引用解析不出可用的正文**——不存在、按保留期清理过，
    /// 或存储层判定它与声明的摘要对不上。
    ///
    /// 三种都返回 `None` 而不是各自报错，是因为调用方要回答的问题是同一个："这条引用还能不能
    /// 用来核对"。而"不能"是一个确定的答案，不是一个异常。真要把"内容被悄悄换过"与"内容被
    /// 按约定清理了"分开，读的是主体那条路（`Subject::observed_body` 会为前者报错）。
    fn body_of(&self, blob_ref: &BlobRef) -> Option<String>;
}

impl BodySource for ContentStore {
    fn body_of(&self, blob_ref: &BlobRef) -> Option<String> {
        let bytes = self.get(blob_ref).ok().flatten()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// 一条正文都没有的来源。给"这些用例不关心正文"的测试用。
///
/// 显式写出来而不是让 `bodies: Option<&dyn BodySource>`：一个可选的参数会让验证器出现
/// "没有正文时少查一项"的分支，而那个分支恰恰是**最不该被测试绕过**的那一项。
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBodies;

impl BodySource for NoBodies {
    fn body_of(&self, _blob_ref: &BlobRef) -> Option<String> {
        None
    }
}

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

/// 证据可用性（§7.2）。
///
/// 查的是这条候选引用的证据**还存在且仍可访问吗**。§7.2 的原话是：
///
/// > 引用必须能解析为存在**且仍可访问**的证据。缺失来源、过期证据、**权限变化**和数据撤回
/// > 都可使候选失效。
///
/// 前半句一直有人管（L2 的存在性校验），后半句此前没有任何人管。后果很具体：撤回一项权限
/// 之后，能力簇手里的证据引用还在，于是下一轮它照样会拿那些证据下结论——**只是那些结论
/// 存不进记忆而已**。而"下出了结论但存不进去"比"下不出结论"难发现得多：界面上看起来
/// 一切正常，环路照常一轮一轮地跑。
///
/// 只在**发现问题时**上报，全部可用时返回 `None`。理由是它没有正面结论可报——"这些证据
/// 都能用"是一条没有信息量的判定，而把它写进每一份档案，只会让每一条候选的审计记录都多
/// 一行不变的话。这条取舍与另外三个检验不同，写在这里免得被当成遗漏。
pub fn check_evidence_access(
    claim: &Candidate,
    ledger: &EvidenceLedger,
    bodies: &dyn BodySource,
) -> Option<VerificationOutcome> {
    let Candidate::Claim { evidence_refs, .. } = claim else {
        return None;
    };

    let mut unusable = ledger.retracted_among(evidence_refs);

    // 引用还在，但它指着的那条正文取不回来了。§7.2 把"引用必须能解析为**存在且仍可访问**的
    // 证据"写成一条硬要求，而"证据"在正文这一层就是**这条内容还读不读得回来**：
    // 一条引用了已经按保留期清理掉的正文的结论，与一条引用了已撤回证据的结论，
    // 对"还该不该算数"这个问题的答案是一样的。
    for reference in evidence_refs {
        if unusable.contains(reference) {
            continue;
        }
        let Some(record) = ledger.get(reference) else {
            continue;
        };
        let Some(body_ref) = &record.body_ref else {
            continue;
        };
        if bodies.body_of(body_ref).is_none() {
            unusable.push(reference.clone());
        }
    }

    if unusable.is_empty() {
        return None;
    }

    Some(VerificationOutcome {
        kind: VerificationKind::EvidenceAccess,
        // 判定是 `Refuted` 而不是 `Inconclusive`：这不是"核不了"，而是一条确定的结论——
        // 这条候选建立在一份已经作废的材料上，它不该被选中。报成"无法判定"会让它继续
        // 参与竞争，而"证据没了"恰恰是最不该靠竞争来解决的那类问题。
        verdict: Verdict::Refuted,
        evidence_refs: unusable,
    })
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
    bodies: &dyn BodySource,
) -> Option<VerificationOutcome> {
    let Candidate::Claim {
        statement,
        evidence_refs,
    } = claim
    else {
        return None;
    };

    // 命题自己引用的那些材料：观测值 + 正文。两道核对都对着这一份说话。
    let mut cited_values: Vec<String> = Vec::new();
    let mut cited_bodies: Vec<String> = Vec::new();
    for record in evidence_refs.iter().filter_map(|reference| ledger.get(reference)) {
        cited_values.push(record.observed_value.clone());
        if let Some(body) = record
            .body_ref
            .as_ref()
            .and_then(|body_ref| bodies.body_of(body_ref))
        {
            cited_bodies.push(body);
        }
    }

    // 第一道：命题断言了台账里**已知**的某个值，而它自己没引用持该值的那条证据。
    let asserted: Vec<String> = ledger
        .known_values()
        .into_iter()
        .filter(|value| statement.contains(value))
        .map(str::to_string)
        .collect();
    let unsupported: Vec<String> = asserted
        .iter()
        .filter(|value| !cited_values.contains(value))
        .cloned()
        .collect();

    // 第二道：命题里的数字与日期，在它引用的材料里有没有出处（§15.1 的"数字来源"）。
    //
    // **只有在引用里确实有正文时才查这一道。** 拿一份版本摘要去核"下一次评审是 2026-10-15"，
    // 只会把所有带数字的命题一律判成否定——那不是发现问题，那是把"没有材料"误报成"材料不对"。
    let tokens = checkable_tokens(statement);
    let body_check_applies = !cited_bodies.is_empty();
    let unsourced: Vec<String> = if body_check_applies {
        tokens
            .iter()
            .filter(|token| {
                !cited_values.iter().any(|value| value.contains(token.as_str()))
                    && !cited_bodies.iter().any(|body| body.contains(token.as_str()))
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    if asserted.is_empty() && (tokens.is_empty() || !body_check_applies) {
        // 命题里既没有已知的值，也没有可核的数字——这个工具对它不适用，
        // **不假装核对过了**（模块文档第 1 条）。
        return None;
    }

    if unsupported.is_empty() && unsourced.is_empty() {
        return Some(VerificationOutcome {
            kind: VerificationKind::Tool,
            verdict: Verdict::Supported,
            evidence_refs: evidence_refs.clone(),
        });
    }

    // 两类失败要指的证据不同。
    //
    // 第一类有明确的对象：台账里**真正持那个值**的那条记录，就是"它本该引用却没引用"的。
    // 第二类没有——数字在任何引用的材料里都不存在，找不到一条"正确的出处"，所以指回命题
    // 自己的引用：它们是**本该提供出处却没有**的那几条。
    let mut culprits: Vec<EvidenceRef> = Vec::new();
    for record in ledger.records() {
        if unsupported.contains(&record.observed_value)
            && !culprits.contains(&record.evidence_ref)
        {
            culprits.push(record.evidence_ref.clone());
        }
    }
    if !unsourced.is_empty() {
        for reference in evidence_refs {
            if !culprits.contains(reference) {
                culprits.push(reference.clone());
            }
        }
    }

    Some(VerificationOutcome {
        kind: VerificationKind::Tool,
        verdict: Verdict::Refuted,
        evidence_refs: culprits,
    })
}

/// 命题里值得核的数字。
///
/// 这是"数字来源"这一条能机械化的部分，而它只认**日期**和**长度不少于四位的数字串**。
/// 两条都写下来，是因为它们的收与放都是刻意的：
///
/// * **日期整体算一个**（`YYYY-MM-DD`）。逐段核会让 `2026-12-01` 里的 `12` 与 `01` 各自
///   去撞运气，而它们撞上的可能性比整串高得多——那是把精确度换成假阳性。
/// * **长度下限**。单个数字在正文里太容易撞上（"第 3 步"、"共 4 项"），把它当断言核，
///   结果是每一份写得正常的草稿都被判成否定。
///
/// 代价是明确的：**`版本 42` 这种短数字不会被查。** 一条只在部分情况下成立的检查最容易
/// 被当成它成立的那个特例，所以这句话写在这里而不是省略。
///
/// 它**不**试图认名字、术语或因果——那需要语义理解，而一个靠启发式的"语义核对"会在审计里
/// 留下"已核对"的标记，却答不出它到底核对了什么。
fn checkable_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens: Vec<String> = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        if let Some(date) = date_at(&chars, index) {
            tokens.push(date);
            index += 10;
            continue;
        }
        if chars[index].is_ascii_digit() {
            let start = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
            }
            if index - start >= 4 {
                tokens.push(chars[start..index].iter().collect());
            }
            continue;
        }
        index += 1;
    }

    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// 从 `index` 起是否是一个 `YYYY-MM-DD` 形状的日期。
fn date_at(chars: &[char], index: usize) -> Option<String> {
    let digit = |offset: usize| chars.get(index + offset).is_some_and(char::is_ascii_digit);
    let dash = |offset: usize| chars.get(index + offset) == Some(&'-');
    let shaped = (0..4).all(digit)
        && dash(4)
        && (5..7).all(digit)
        && dash(7)
        && (8..10).all(digit);
    if !shaped {
        return None;
    }
    Some(chars[index..index + 10].iter().collect())
}

/// 把四类判定在一条候选上跑一遍。
///
/// 可用性放在**最前面**。它是 §7.2 的有效性前提，而不是三种检验之一：对一条引用了已撤回
/// 证据的候选做"结论依据核对"和"来源核对"，等于在一个已经不该存在的问题上花两次预算——
/// 而 §4.1 L3 是有预算的。
pub fn review_candidate(
    index: usize,
    candidate: &Candidate,
    ledger: &EvidenceLedger,
    bodies: &dyn BodySource,
    policy: &ReviewPolicy,
) -> CandidateReview {
    let mut outcomes = Vec::new();
    if let Some(outcome) = check_evidence_access(candidate, ledger, bodies) {
        outcomes.push(outcome);
    }
    if let Some(outcome) = check_claim_grounding(candidate, ledger, bodies) {
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
    bodies: &dyn BodySource,
    policy: &ReviewPolicy,
) -> Vec<CandidateReview> {
    let mut reviews = Vec::new();
    let mut spent = 0usize;
    for (index, candidate) in candidates.candidates.iter().enumerate() {
        if spent >= policy.max_checks {
            break;
        }
        let review = review_candidate(index, candidate, ledger, bodies, policy);
        spent = spent.saturating_add(review.outcomes.len());
        // 即便一条检验也没跑成，也要留下档案：`select` 按下标找档案，缺了它就只能当成
        // "没有检验记录"，而这两者在审计里含义不同。
        reviews.push(review);
    }
    reviews
}
