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
fn an_unresolved_question_blocks_selection_too() {
    let mut candidates = set(vec![claim("文件是 sha256:aaa", &["1"])]);
    candidates.unresolved.push(Unresolved {
        question: "哪一个版本才是当前版本".to_string(),
        missing: vec!["需要一次同刻观测".to_string()],
    });

    let selection = select(&candidates, Vec::new(), &policy(), ActionLevel::A0).expect("选择");
    match selection.outcome {
        SelectionOutcome::NeedsMoreInformation { missing } => {
            assert!(missing[0].contains("同刻观测"), "实际：{missing:?}");
        }
        other => panic!("有未决问题时不该选出任何一条，实际：{other:?}"),
    }
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
