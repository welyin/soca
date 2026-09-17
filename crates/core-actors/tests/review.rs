//! 证据台账与 L3 检验器的回归测试（§4.3「证据与风险评估」、§6 第 4 步）。
//!
//! 三组最要紧的性质：
//!
//! * **同一段录屏转写出的两份摘要不是两次独立观测。** 用它们凑门槛，等于用一次观测的重量
//!   压两次秤。
//! * **"没找到反例"不等于"结论成立"。** 反例搜索空手而归还报 `Supported`，就等于把
//!   "没查出来"当成"没问题"。
//! * **引用了一条真实证据、却断言别的东西，要能被抓住。** 结构校验看不出来（引用合法、
//!   下标存在），只有把命题文本和证据值对一遍才发现得了。

use soca_contracts::{
    ActionLevel, Candidate, CandidateSet, EvidenceRef, SelectionOutcome, SelectionPolicy, UnitId,
    VerificationKind, Verdict, select,
};
use soca_core_actors::{
    ReviewPolicy, check_claim_grounding, check_source_independence, review_all, review_candidate,
    search_counter_example, EvidenceLedger, EvidenceRecord, MAX_EVIDENCE_RECORDS,
};

fn reference(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn unit(name: &str) -> UnitId {
    UnitId::new(format!("unit:{name}")).expect("固定单元")
}

/// 造一条记录。
fn record(
    evidence: &str,
    subject: &str,
    value: &str,
    by: &str,
    derived_from: &[&str],
) -> EvidenceRecord {
    EvidenceRecord {
        evidence_ref: reference(evidence),
        subject_ref: subject.to_string(),
        observed_value: value.to_string(),
        derived_from: derived_from.iter().map(|name| reference(name)).collect(),
        observed_by: unit(by),
    }
}

fn ledger_with(records: Vec<EvidenceRecord>) -> EvidenceLedger {
    let mut ledger = EvidenceLedger::new();
    for item in records {
        ledger.record(item);
    }
    ledger
}

fn claim(statement: &str, evidence: &[&str]) -> Candidate {
    Candidate::Claim {
        statement: statement.to_string(),
        evidence_refs: evidence.iter().map(|name| reference(name)).collect(),
    }
}

// ---------------------------------------------------------------------------
// 来源独立性
// ---------------------------------------------------------------------------

#[test]
fn two_readings_by_different_units_are_independent() {
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    let first = ledger.get(&reference("a")).expect("存在");
    let second = ledger.get(&reference("b")).expect("存在");
    assert!(first.is_independent_of(second));
    assert_eq!(
        ledger.independent_source_count(&[reference("a"), reference("b")]),
        2
    );
}

#[test]
fn two_readings_by_the_same_unit_are_not_independent() {
    // 同一个单元读两遍，如果它读错了，两遍都错。那不构成交叉核对——它只构成同一个错误
    // 被记录了两次。
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader", &[]),
        record("b", "file:x", "sha256:aaa", "reader", &[]),
    ]);
    assert_eq!(
        ledger.independent_source_count(&[reference("a"), reference("b")]),
        1
    );
}

#[test]
fn a_derived_observation_is_not_independent_of_its_source() {
    let ledger = ledger_with(vec![
        record("raw", "file:x", "sha256:aaa", "reader", &[]),
        record("summary", "file:x", "sha256:aaa", "summarizer", &["raw"]),
    ]);
    assert_eq!(
        ledger.independent_source_count(&[reference("raw"), reference("summary")]),
        1
    );
}

#[test]
fn two_summaries_of_one_transcript_do_not_add_up_to_two_sources() {
    // 这是整个来源核对存在的理由。两个不同的单元、两条不同的证据引用、观测到同一个值——
    // 看起来像两个来源，其实是同一次观测被抄了两遍。
    let ledger = ledger_with(vec![
        record("raw", "audio:meeting", "transcript-v1", "recorder", &[]),
        record("s1", "audio:meeting", "transcript-v1", "summarizer-1", &["raw"]),
        record("s2", "audio:meeting", "transcript-v1", "summarizer-2", &["raw"]),
    ]);
    assert_eq!(
        ledger.independent_source_count(&[reference("s1"), reference("s2")]),
        1,
        "两份摘要同源，不能凑成两个来源"
    );
    assert_eq!(
        ledger.independent_source_count(&[
            reference("raw"),
            reference("s1"),
            reference("s2")
        ]),
        1,
        "把原始观测也算上，三者的来源仍然只有一个"
    );
}

#[test]
fn reciting_the_same_reference_twice_keeps_the_first_record() {
    // §4.1 L2 要求"证据不能被执行层改写"，而"后来的同引用覆盖旧记录"正是改写的一种形式。
    let mut ledger = ledger_with(vec![record("a", "file:x", "sha256:old", "reader", &[])]);
    ledger.record(record("a", "file:x", "sha256:new", "attacker", &[]));
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        ledger.get(&reference("a")).expect("存在").observed_value,
        "sha256:old"
    );
}

#[test]
fn the_ledger_is_bounded_and_keeps_its_index_correct_after_eviction() {
    // 淘汰之后落下的下标映射，会让台账悄悄指向错的记录——而"核对用的材料指错了"比
    // "没有材料"更危险，因为它会给出一个看似有依据的判定。
    let mut ledger = EvidenceLedger::new();
    for index in 0..(MAX_EVIDENCE_RECORDS + 8) {
        ledger.record(record(
            &format!("e{index}"),
            "file:x",
            &format!("v{index}"),
            "reader",
            &[],
        ));
    }
    assert_eq!(ledger.len(), MAX_EVIDENCE_RECORDS);

    for index in 8..(MAX_EVIDENCE_RECORDS + 8) {
        let found = ledger
            .get(&reference(&format!("e{index}")))
            .unwrap_or_else(|| panic!("e{index} 应当还在"));
        assert_eq!(
            found.observed_value,
            format!("v{index}"),
            "下标映射必须仍然指向同一条记录"
        );
    }
    assert!(
        ledger.get(&reference("e0")).is_none(),
        "最早的那条被淘汰了"
    );
}

#[test]
fn known_values_are_deduplicated_and_sorted() {
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:bbb", "r1", &[]),
        record("b", "file:y", "sha256:aaa", "r2", &[]),
        record("c", "file:z", "sha256:bbb", "r3", &[]),
    ]);
    assert_eq!(ledger.known_values(), vec!["sha256:aaa", "sha256:bbb"]);
}

// ---------------------------------------------------------------------------
// 来源核对
// ---------------------------------------------------------------------------

#[test]
fn a_claim_resting_on_two_independent_sources_is_supported() {
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    let outcome =
        check_source_independence(&claim("版本是 sha256:aaa", &["a", "b"]), &ledger).expect("适用");
    assert_eq!(outcome.kind, VerificationKind::IndependentSource);
    assert_eq!(outcome.verdict, Verdict::Supported);
    assert_eq!(outcome.evidence_refs.len(), 2);
}

#[test]
fn a_claim_resting_on_one_source_is_inconclusive_not_supported() {
    // 孤证不是支持。说成支持会让一个来源看起来像两个。
    let ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader", &[])]);
    let outcome =
        check_source_independence(&claim("版本是 sha256:aaa", &["a"]), &ledger).expect("适用");
    assert_eq!(outcome.verdict, Verdict::Inconclusive);
}

#[test]
fn an_observation_request_has_nothing_to_source_check() {
    let ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader", &[])]);
    let candidate = Candidate::RequestObservation {
        subject_ref: "file:x".to_string(),
        reason: "证据不足".to_string(),
    };
    assert!(
        check_source_independence(&candidate, &ledger).is_none(),
        "申请观测不是断言，没有可核对的来源"
    );
}

// ---------------------------------------------------------------------------
// 反例搜索
// ---------------------------------------------------------------------------

#[test]
fn a_contradicting_observation_refutes_the_claim() {
    let ledger = ledger_with(vec![
        record("mine", "file:x", "sha256:aaa", "reader", &[]),
        record("other", "file:x", "sha256:bbb", "watcher", &[]),
    ]);
    let outcome =
        search_counter_example(&claim("版本是 sha256:aaa", &["mine"]), &ledger).expect("适用");
    assert_eq!(outcome.verdict, Verdict::Refuted);
    assert_eq!(outcome.evidence_refs, vec![reference("other")]);
}

#[test]
fn a_different_subject_is_not_a_counter_example() {
    // 另一个文件有另一个版本，不是对"这个文件是这个版本"的反例。
    let ledger = ledger_with(vec![
        record("mine", "file:x", "sha256:aaa", "reader", &[]),
        record("other", "file:y", "sha256:bbb", "reader-2", &[]),
    ]);
    let outcome =
        search_counter_example(&claim("版本是 sha256:aaa", &["mine"]), &ledger).expect("适用");
    assert_eq!(outcome.verdict, Verdict::Inconclusive);
}

#[test]
fn finding_no_counter_example_is_inconclusive_not_supported() {
    // 这条测试盯的是整份文档里最容易写错的一处诚实性：把"没找到反例"写成"支持"。
    // 两者混同，等于把"没查出来"当成"没问题"。
    let ledger = ledger_with(vec![record("mine", "file:x", "sha256:aaa", "reader", &[])]);
    let outcome =
        search_counter_example(&claim("版本是 sha256:aaa", &["mine"]), &ledger).expect("适用");
    assert_eq!(
        outcome.verdict,
        Verdict::Inconclusive,
        "找了，没找到；这既不是支持也不是否定"
    );
    assert!(outcome.evidence_refs.is_empty());
}

#[test]
fn a_low_risk_task_does_not_search_for_counter_examples() {
    // §6 第 4 步："低风险纯格式任务不强行编造反方观点。" 硬凑一个反方观点只会让审计账里
    // 塞满"已检验"的标记，而实际上什么也没检验。
    let ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "r1", &[])]);

    let low = ReviewPolicy::for_risk(ActionLevel::A1, ActionLevel::A2, 8);
    assert!(!low.counter_example);
    let review = review_candidate(0, &claim("版本是 sha256:aaa", &["a"]), &ledger, &low);
    assert!(
        review
            .outcomes
            .iter()
            .all(|outcome| outcome.kind != VerificationKind::CounterExample),
        "低风险不搜反例"
    );

    let high = ReviewPolicy::for_risk(ActionLevel::A3, ActionLevel::A2, 8);
    assert!(high.counter_example);
    let review = review_candidate(0, &claim("版本是 sha256:aaa", &["a"]), &ledger, &high);
    assert!(
        review
            .outcomes
            .iter()
            .any(|outcome| outcome.kind == VerificationKind::CounterExample),
        "高风险要搜"
    );
}

// ---------------------------------------------------------------------------
// 结论依据核对
// ---------------------------------------------------------------------------

#[test]
fn a_claim_asserting_a_value_absent_from_its_own_evidence_is_refuted() {
    // 这是针对模型幻觉的一条具体防线：引用下标 0（一条真实存在的证据），然后写下证据里
    // 根本没有的结论。结构校验看不出来——引用合法、下标存在、证据确实在上下文里。
    let ledger = ledger_with(vec![
        record("real", "file:x", "sha256:aaa", "reader", &[]),
        record("elsewhere", "file:y", "sha256:bbb", "watcher", &[]),
    ]);
    let outcome = check_claim_grounding(
        &claim("file:x 的版本是 sha256:bbb", &["real"]),
        &ledger,
    )
    .expect("命题断言了可核对的值，因此适用");

    assert_eq!(outcome.kind, VerificationKind::Tool);
    assert_eq!(outcome.verdict, Verdict::Refuted);
    assert_eq!(
        outcome.evidence_refs,
        vec![reference("elsewhere")],
        "判定要指出它本该引用却漏掉的那条证据"
    );
}

#[test]
fn a_claim_that_quotes_its_own_evidence_is_supported() {
    let ledger = ledger_with(vec![record("real", "file:x", "sha256:aaa", "reader", &[])]);
    let outcome = check_claim_grounding(&claim("file:x 的版本是 sha256:aaa", &["real"]), &ledger)
        .expect("适用");
    assert_eq!(outcome.verdict, Verdict::Supported);
}

#[test]
fn a_prose_claim_without_values_is_simply_not_checked() {
    // "不适用"与"无法判定"是两件事。报成 Inconclusive 会让每次审查都看起来做了三件事，
    // 而实际上可能一件也没做。
    let ledger = ledger_with(vec![record("real", "file:x", "sha256:aaa", "reader", &[])]);
    assert!(
        check_claim_grounding(&claim("摘要文件已经更新", &["real"]), &ledger).is_none(),
        "命题没有断言任何可核对的值，这个工具对它不适用"
    );
}

// ---------------------------------------------------------------------------
// 审查与选择接起来
// ---------------------------------------------------------------------------

#[test]
fn review_all_stops_at_the_check_budget() {
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    let set = CandidateSet {
        candidates: (0..6)
            .map(|index| claim(&format!("第 {index} 条说是 sha256:aaa"), &["a", "b"]))
            .collect(),
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };

    let policy = ReviewPolicy {
        counter_example: true,
        max_checks: 3,
    };
    let reviews = review_all(&set, &ledger, &policy);
    let spent: usize = reviews.iter().map(|review| review.outcomes.len()).sum();
    assert!(spent <= 3, "超出预算：{spent}");
    assert!(reviews.len() < set.candidates.len(), "预算用满即停");
}

#[test]
fn a_refuted_claim_cannot_be_selected_even_with_plenty_of_evidence() {
    // 端到端：检验 → 选择。被反例推翻的候选带着三条证据，仍然出局。
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
        record("c", "file:x", "sha256:aaa", "reader-3", &[]),
        record("contra", "file:x", "sha256:zzz", "watcher", &[]),
    ]);
    let set = CandidateSet {
        candidates: vec![claim("file:x 的版本是 sha256:aaa", &["a", "b", "c"])],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };

    let policy = ReviewPolicy::for_risk(ActionLevel::A2, ActionLevel::A2, 8);
    let reviews = review_all(&set, &ledger, &policy);
    assert!(
        reviews[0].is_refuted(),
        "三条同值证据不能压过一条反例：{reviews:?}"
    );

    let selection = select(&set, reviews, &SelectionPolicy::default(), ActionLevel::A2)
        .expect("选择");
    assert_eq!(selection.outcome, SelectionOutcome::NothingToPursue);
}

#[test]
fn reviewing_the_same_input_twice_gives_the_same_verdicts() {
    // §13 要求可复现。检验与选择都是纯函数，所以"同一份台账加同一个候选集合跑两遍得到
    // 同一份档案"是一条能完整断言的等式——不像在主体那一层重跑一次，那里的证据引用
    // 本来就会不同。
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    let set = CandidateSet {
        candidates: vec![claim("file:x 的版本是 sha256:aaa", &["a", "b"])],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    let policy = ReviewPolicy::for_risk(ActionLevel::A3, ActionLevel::A2, 8);

    let first = review_all(&set, &ledger, &policy);
    let second = review_all(&set, &ledger, &policy);
    assert_eq!(first, second);

    let selection_policy = SelectionPolicy::default();
    let left = select(&set, first, &selection_policy, ActionLevel::A3).expect("选择");
    let right = select(&set, second, &selection_policy, ActionLevel::A3).expect("选择");
    assert_eq!(left, right);
}

#[test]
fn the_verdict_reached_end_to_end_is_traceable_to_the_evidence_that_caused_it() {
    // 审计要能回答"为什么选了它 / 为什么没选它"。检验档案里带着证据引用，就是为了这个。
    let ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    let set = CandidateSet {
        candidates: vec![claim("file:x 的版本是 sha256:aaa", &["a", "b"])],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };

    let policy = ReviewPolicy::for_risk(ActionLevel::A1, ActionLevel::A2, 8);
    let reviews = review_all(&set, &ledger, &policy);
    let selection = select(&set, reviews, &SelectionPolicy::default(), ActionLevel::A1)
        .expect("选择");

    assert_eq!(selection.selected_index(), Some(0));
    let cited: Vec<String> = selection.reviews[0]
        .evidence_refs()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        cited.contains(&"obs:a".to_string()) && cited.contains(&"obs:b".to_string()),
        "判定要能追溯到具体证据：{cited:?}"
    );
    assert!(selection.rationale.contains("门槛"), "{}", selection.rationale);
}
