//! L3 候选竞争的回归测试（§4.1 L3、§6 第 4–5 步）。
//!
//! 最要紧的三条：
//!
//! * **被反例推翻的结论不因为支持者多而重新成立**；
//! * **有未决冲突时不选**——"选一条了事"就是用决定覆盖矛盾；
//! * **证据门槛随风险上升**，而不是一个写死的数字。

use soca_contracts::*;

fn evidence_ref(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn claim(statement: &str, evidence: &[&str]) -> Candidate {
    Candidate::Claim {
        statement: statement.to_string(),
        evidence_refs: evidence.iter().map(|name| evidence_ref(name)).collect(),
    }
}

fn observation(subject: &str) -> Candidate {
    Candidate::RequestObservation {
        subject_ref: subject.to_string(),
        reason: "证据不足".to_string(),
    }
}

fn set(candidates: Vec<Candidate>) -> CandidateSet {
    CandidateSet {
        candidates,
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    }
}

fn support(kind: VerificationKind, evidence: &str) -> VerificationOutcome {
    VerificationOutcome {
        kind,
        verdict: Verdict::Supported,
        evidence_refs: vec![evidence_ref(evidence)],
    }
}

fn policy() -> SelectionPolicy {
    SelectionPolicy {
        base_evidence: 1,
        high_risk_extra: 2,
        high_risk_from: ActionLevel::A2,
        max_checks: 8,
    }
}

// ---------------------------------------------------------------------------
// 淘汰规则
// ---------------------------------------------------------------------------

#[test]
fn a_refuted_candidate_is_out_even_though_it_has_more_evidence() {
    // 一条被反例推翻的结论不会因为支持者多就重新成立——这正是"不靠置信度投票"最直白的
    // 样子。这里候选 #0 有三条证据、候选 #1 只有一条，但 #0 被否定了。
    let candidates = set(vec![
        claim("文件是 sha256:aaa", &["1", "2", "3"]),
        claim("文件是 sha256:bbb", &["4"]),
    ]);
    let reviews = vec![
        CandidateReview::new(
            0,
            vec![VerificationOutcome {
                kind: VerificationKind::CounterExample,
                verdict: Verdict::Refuted,
                evidence_refs: vec![evidence_ref("counter")],
            }],
        ),
        CandidateReview::new(1, Vec::new()),
    ];

    let selection = select(&candidates, reviews, &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(selection.selected_index(), Some(1));
}

#[test]
fn a_conflict_blocks_selection_entirely() {
    // §4.2 禁止用多数意见覆盖矛盾。"选一条了事"是覆盖的一种形式，所以这里不选。
    let mut candidates = set(vec![claim("文件是 sha256:aaa", &["1"])]);
    candidates.conflicts.push(Conflict {
        subject_ref: "file:summary.md".to_string(),
        positions: vec![
            ConflictPosition {
                statement: "版本是 aaa".to_string(),
                by: UnitId::new("unit:a").expect("固定单元"),
                evidence_refs: vec![evidence_ref("1")],
            },
            ConflictPosition {
                statement: "版本是 bbb".to_string(),
                by: UnitId::new("unit:b").expect("固定单元"),
                evidence_refs: vec![evidence_ref("2")],
            },
        ],
    });

    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    match selection.outcome {
        SelectionOutcome::NeedsMoreInformation { missing } => {
            assert_eq!(missing.len(), 1);
            assert!(missing[0].contains("file:summary.md"), "实际：{missing:?}");
        }
        other => panic!("有冲突时不该选出任何一条，实际：{other:?}"),
    }
}

#[test]
fn an_unresolved_question_is_what_you_report_when_nothing_is_eligible() {
    // 未决问题在"确实没有可推进的候选"时才成为结论。
    let mut candidates = set(Vec::new());
    candidates.unresolved.push(Unresolved {
        question: "哪一个版本才是当前版本".to_string(),
        missing: vec!["需要一次同刻观测".to_string()],
    });

    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    match selection.outcome {
        SelectionOutcome::NeedsMoreInformation { missing } => {
            assert!(missing[0].contains("同刻观测"), "实际：{missing:?}");
        }
        other => panic!("应当报告缺什么，实际：{other:?}"),
    }
}

#[test]
fn an_unresolved_question_does_not_block_a_well_supported_candidate() {
    // 冲突与未决问题的区别不是措辞上的。冲突是"同一个对象上有两个不能同时成立的结论"；
    // 未决问题往往只是"某件事还没查"。让后者阻塞前者，会让一个簇只要有一个槽位在等证据，
    // 就再也选不出任何东西——而在一个真实的簇里，那是常态。
    //
    // 这个缺陷是主体把检验器接上之后才暴露的：`ActionPrecondition` 的未决问题把
    // 每一条结论都挡下了，而它自己与 `select` 在那个改动之前各自都通过测试。
    let mut candidates = set(vec![claim("文件是 sha256:aaa", &["1"])]);
    candidates.unresolved.push(Unresolved {
        question: "动作前提是否成立".to_string(),
        missing: vec!["尚未观测到该前提".to_string()],
    });

    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(selection.selected_index(), Some(0));
    assert!(
        selection.rationale.contains("未决问题"),
        "未决问题不能被吞掉：选好了不等于什么都清楚了。实际：{}",
        selection.rationale
    );
}

#[test]
fn an_observation_request_wins_over_a_claim_that_missed_the_bar() {
    // 结论没过门槛时，正确的下一步是去观测，而不是硬选一个结论。
    let candidates = set(vec![
        claim("文件是 sha256:aaa", &["1"]),
        Candidate::RequestObservation {
            subject_ref: "file:summary.md".to_string(),
            reason: "证据不足".to_string(),
        },
    ]);
    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A3).expect("选择");
    assert_eq!(selection.selected_index(), Some(1));
}

// ---------------------------------------------------------------------------
// 证据门槛
// ---------------------------------------------------------------------------

#[test]
fn the_evidence_bar_rises_with_risk() {
    // §6 第 5 步："错误代价高时提高证据门槛"。门槛是风险的函数这一点要能被测试，
    // 否则它只是一个写死的数字。
    let policy = policy();
    assert_eq!(policy.evidence_bar(ActionLevel::A0), 1);
    assert_eq!(policy.evidence_bar(ActionLevel::A1), 1);
    assert_eq!(policy.evidence_bar(ActionLevel::A2), 3);
    assert_eq!(policy.evidence_bar(ActionLevel::A3), 3);
    assert_eq!(policy.evidence_bar(ActionLevel::A4), 3);
}

#[test]
fn a_low_risk_task_accepts_one_piece_of_evidence_and_a_high_risk_one_does_not() {
    // §6 第 4 步："低风险纯格式任务不强行编造反方观点。"同一份候选，A1 下选得出来，
    // A3 下选不出来——它需要更多证据，而不是需要一个反方观点。
    let candidates = set(vec![claim("文件是 sha256:aaa", &["1"])]);

    let low = select(&candidates, Vec::new(), &policy(), ActionLevel::A1).expect("选择");
    assert_eq!(low.selected_index(), Some(0));
    assert_eq!(low.required_evidence, 1);

    let high = select(&candidates, Vec::new(), &policy(), ActionLevel::A3).expect("选择");
    assert_eq!(high.selected_index(), None);
    assert_eq!(high.required_evidence, 3);
    assert!(matches!(
        high.outcome,
        SelectionOutcome::NothingToPursue
    ));
}

#[test]
fn an_observation_request_is_not_gated_by_the_evidence_bar() {
    // 申请观测不是"关于世界的断言"，而是行动请求——它本身就是去补证据的。
    // 给它套证据门槛只会逼出"为了过门槛而编一条证据"这种行为。
    let candidates = set(vec![
        claim("文件是 sha256:aaa", &["1"]),
        observation("file:summary.md"),
    ]);

    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A3).expect("选择");
    assert_eq!(
        selection.selected_index(),
        Some(1),
        "结论没过门槛时，正确的下一步是去观测，而不是硬选一个结论"
    );
}

// ---------------------------------------------------------------------------
// 排序
// ---------------------------------------------------------------------------

#[test]
fn more_evidence_wins() {
    let candidates = set(vec![
        claim("说法一", &["1"]),
        claim("说法二", &["2", "3", "4"]),
    ]);
    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(selection.selected_index(), Some(1));
}

#[test]
fn independent_sources_break_a_tie() {
    let candidates = set(vec![
        claim("说法一", &["1", "2"]),
        claim("说法二", &["3", "4"]),
    ]);
    let reviews = vec![CandidateReview::new(
        1,
        vec![
            support(VerificationKind::IndependentSource, "x"),
            support(VerificationKind::IndependentSource, "y"),
        ],
    )];

    let selection = select(&candidates, reviews, &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(selection.selected_index(), Some(1));
}

#[test]
fn a_tool_check_does_not_count_as_independent_support() {
    // 同一条证据被工具检查两次不构成两次独立支持（§7.2 的证据去重原则）。
    let candidates = set(vec![
        claim("说法一", &["1", "2"]),
        claim("说法二", &["3", "4"]),
    ]);
    let reviews = vec![CandidateReview::new(
        1,
        vec![
            support(VerificationKind::Tool, "x"),
            support(VerificationKind::Tool, "y"),
        ],
    )];

    let selection = select(&candidates, reviews, &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(
        selection.selected_index(),
        Some(0),
        "工具检验不算独立来源，因此并列时取先出现的那条"
    );
}

#[test]
fn inconclusive_does_not_count_as_support() {
    let candidates = set(vec![
        claim("说法一", &["1", "2"]),
        claim("说法二", &["3", "4"]),
    ]);
    let reviews = vec![CandidateReview::new(
        1,
        vec![VerificationOutcome {
            kind: VerificationKind::IndependentSource,
            verdict: Verdict::Inconclusive,
            evidence_refs: vec![evidence_ref("x")],
        }],
    )];

    let selection = select(&candidates, reviews, &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(
        selection.selected_index(),
        Some(0),
        "无法判定不是支持，不能拿来当筹码"
    );
}

#[test]
fn a_tie_is_broken_deterministically() {
    // §13 要求可复现：同样的输入必须得到同样的选择。
    let candidates = set(vec![
        claim("说法一", &["1", "2"]),
        claim("说法二", &["3", "4"]),
    ]);
    let first = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    let second = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    assert_eq!(first.selected_index(), Some(0));
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// 预算与空集
// ---------------------------------------------------------------------------

#[test]
fn the_check_budget_is_enforced() {
    // §4.1 L3 点名的"有预算的……工作流"。没有预算的检验流程会变成"想验多少验多少"。
    let candidates = set(vec![claim("说法一", &["1"])]);
    let reviews = vec![CandidateReview::new(
        0,
        (0..9)
            .map(|index| support(VerificationKind::Tool, &format!("e{index}")))
            .collect(),
    )];

    let result = select(&candidates, reviews, &policy(), ActionLevel::A0);
    assert_eq!(
        result,
        Err(ContractError::ContextLimitExceeded {
            field: "selection.checks",
            limit: 8,
            actual: 9
        })
    );
}

#[test]
fn an_empty_candidate_set_selects_nothing() {
    let selection = select(&CandidateSet::empty(), Vec::new(), &policy(), ActionLevel::A0)
        .expect("选择");
    assert_eq!(selection.outcome, SelectionOutcome::NothingToPursue);
    assert_eq!(selection.selected_index(), None);
}

// ---------------------------------------------------------------------------
// 拒绝台账与容量闸（§13.1）
// ---------------------------------------------------------------------------

#[test]
fn every_candidate_ends_up_either_selected_or_rejected_with_a_retry_condition() {
    // §13.1："最**多 8 个待核验正式候选**。……**拒绝有原因和可重试条件**。"
    //
    // 最要紧的是这条**记账完整性**：每条候选要么被选中、要么有一条带理由和重试条件的
    // 拒绝记录，没有第三种去处。此前它们是"没被选中"就消失了，而"消失"与"被拒"在界面上
    // 一样，在调度器那里也一样。
    let candidates = set(vec![
        claim("文件是 sha256:aaa", &["1", "2", "3"]),
        claim("文件是 sha256:bbb", &["4"]),
        observation("file:other.md"),
    ]);
    let reviews = vec![CandidateReview::new(
        1,
        vec![VerificationOutcome {
            kind: VerificationKind::CounterExample,
            verdict: Verdict::Refuted,
            evidence_refs: vec![evidence_ref("counter")],
        }],
    )];

    let selection = select(&candidates, reviews, &policy(), ActionLevel::A2).expect("选择");
    let selected = selection.selected_index().expect("应当选出一条");

    for index in 0..candidates.candidates.len() {
        assert!(
            index == selected || selection.rejection_for(index).is_some(),
            "候选 #{index} 既没被选中也没有拒绝记录"
        );
    }
    for rejection in &selection.rejections {
        assert!(!rejection.reason.is_empty(), "拒绝要给理由");
    }

    // 而三类拒绝要能分开：这一条**进了核验、够格、只是没赢**。
    // 与"名额满了"（根本没进核验）混成一个取值的话，提出方收到的反馈会是同一句话，
    // 而"我没排上队"与"我比了但没赢"要做的事不一样。
    assert_eq!(
        selection.rejection_for(2).expect("要有记录").retry_when,
        RetryWhen::WhenNextRound
    );
    assert_eq!(
        selection.rejection_for(1).expect("要有记录").retry_when,
        RetryWhen::WhenEvidenceChanges
    );
}

#[test]
fn a_claim_below_the_bar_says_how_many_more_it_needs() {
    // 门槛是风险等级的函数，而"没达标"要能说出**还差几条**。只说"证据不足"的话，
    // 提出方下一轮只能瞎猜该补几条——而它恰好知道该补几条，因为门槛是公开的。
    let candidates = set(vec![claim("文件是 sha256:aaa", &["1"])]);
    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A2).expect("选择");

    assert_eq!(selection.selected_index(), None, "没达标就不该被选中");

    let rejection = selection.rejection_for(0).expect("要有拒绝记录");
    assert_eq!(
        rejection.retry_when,
        RetryWhen::MoreEvidence { short_by: 2 },
        "A2 门槛 3 条，手上 1 条，还差 2 条：{}",
        rejection.reason
    );
}

#[test]
fn a_refutation_says_which_kind_refuted_it() {
    // 只说"被否定"的话，操作员得自己去翻档案——而翻出来之后要做的事恰恰取决于答案：
    // 证据失效要去重新授权，反例成立要去看那条反例还在不在。
    let candidates = set(vec![claim("文件是 sha256:aaa", &["1"])]);
    let reviews = vec![CandidateReview::new(
        0,
        vec![VerificationOutcome {
            kind: VerificationKind::EvidenceAccess,
            verdict: Verdict::Refuted,
            evidence_refs: vec![evidence_ref("1")],
        }],
    )];
    let selection = select(&candidates, reviews, &policy(), ActionLevel::A0).expect("选择");

    let rejection = selection.rejection_for(0).expect("要有拒绝记录");
    assert!(
        rejection.reason.contains("evidence_access"),
        "理由要点明是哪一类检验：{}",
        rejection.reason
    );
    // 而"证据没了"是**会变**的——重新授权之后它就能回来。报成 `Never` 的话，
    // 这条候选再也不会被提起。
    assert_eq!(rejection.retry_when, RetryWhen::WhenEvidenceChanges);
    assert!(rejection.retry_when.is_retryable());
}

#[test]
fn only_eight_candidates_go_to_verification_and_the_rest_wait_for_a_slot() {
    // §13.1 的容量闸。不设它的话，"提出 32 条"就等于"核验 32 条"，而核验预算被摊薄到
    // 每条只够查一下——于是**没有一条被查清**。
    //
    // 注意闸门在**核验之前**：第 9 条不该有一条检验档案，因为它压根没进核验。
    let candidates = set((0..12)
        .map(|index| claim(&format!("结论 {index}"), &[&format!("e{index}")]))
        .collect());
    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");

    let verified: Vec<usize> = selection
        .reviews
        .iter()
        .map(|review| review.candidate_index)
        .collect();
    assert!(
        verified.iter().all(|index| *index < MAX_FORMAL_CANDIDATES),
        "超额的候选不该被核验过：{verified:?}"
    );

    let waiting: Vec<usize> = selection
        .rejections
        .iter()
        .filter(|rejection| {
            matches!(
                rejection.retry_when,
                RetryWhen::WhenCapacityFreed { held_by } if held_by == MAX_FORMAL_CANDIDATES
            )
        })
        .map(|rejection| rejection.candidate_index)
        .collect();
    assert_eq!(
        waiting.len(),
        12 - MAX_FORMAL_CANDIDATES,
        "超额的应当在等名额：{waiting:?}"
    );

    // 而它们等的是**名额**，不是证据——这两者在界面上该显示成不同的东西。
    for index in &waiting {
        let rejection = selection.rejection_for(*index).expect("有记录");
        assert!(rejection.retry_when.is_retryable(), "等名额是会好的");
        assert!(
            rejection.reason.contains("名额"),
            "理由要说是名额问题：{}",
            rejection.reason
        );
    }
}

#[test]
fn a_structurally_broken_set_is_refused_before_anything_is_compared() {
    // 工作对象必须是合法集合。让一条不带证据的结论进来比一比，等于承认它是一条候选。
    let broken = set(vec![Candidate::Claim {
        statement: "无凭无据".to_string(),
        evidence_refs: Vec::new(),
    }]);
    assert!(matches!(
        select(&broken, Vec::new(), &policy(), ActionLevel::A0),
        Err(ContractError::MissingRefs { .. })
    ));
}
