//! 证据台账与 L3 检验器的回归测试（§4.3「证据与风险评估」、§6 第 4 步）。
//!
//! 四组最要紧的性质：
//!
//! * **同一段录屏转写出的两份摘要不是两次独立观测。** 用它们凑门槛，等于用一次观测的重量
//!   压两次秤。
//! * **"没找到反例"不等于"结论成立"。** 反例搜索空手而归还报 `Supported`，就等于把
//!   "没查出来"当成"没问题"。
//! * **引用了一条真实证据、却断言别的东西，要能被抓住。** 结构校验看不出来（引用合法、
//!   下标存在），只有把命题文本和证据值对一遍才发现得了。
//! * **证据"存在过"与"现在还能用"是两件事。** 撤回权限之后，一份完全自洽、证据也确实
//!   存在过的结论仍然必须出局（§7.2）。

use std::collections::BTreeMap;

use soca_contracts::{
    ActionLevel, BlobRef, Candidate, CandidateSet, EvidenceRef, SelectionOutcome, SelectionPolicy,
    UnitId, VerificationKind, Verdict, select,
};
use soca_core_actors::{
    ReviewPolicy, check_claim_grounding, check_evidence_access, check_source_independence,
    review_all, review_candidate, search_counter_example, BodySource, EvidenceLedger,
    EvidenceRecord, NoBodies, MAX_EVIDENCE_RECORDS,
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
        // 这些用例不关心正文；正文能不能取回由 `soca-core` 那一边测。
        body_ref: None,
        derived_from: derived_from.iter().map(|name| reference(name)).collect(),
        observed_by: unit(by),
        retracted: None,
    }
}

fn blob_ref(name: &str) -> BlobRef {
    BlobRef::new(format!("blob:personal:{name}")).expect("固定引用")
}

/// 造一条**带正文**的记录。
///
/// 不带正文文本——记录里只有**引用**，正文在内容仓那一侧（这里是 [`InlineBodies`]）。
/// 让这个辅助函数同时收一份正文，会诱使调用方以为"把正文写进记录里"也行；而那样一份记录
/// 一旦落盘，§9.3 好不容易分开的两半就又粘回去了。
fn record_with_a_body(evidence: &str, subject: &str, value: &str) -> EvidenceRecord {
    EvidenceRecord {
        body_ref: Some(blob_ref(evidence)),
        ..record(evidence, subject, value, "reader-1", &[])
    }
}

/// 一份按引用直接给正文的来源。
///
/// 用它可以在不建内容仓的前提下测到正文那两条核对。真接内容仓的路径由 `soca-core` 那一端
/// 端到端测——两层分开是有意的：这一层要能证明"给定这些正文，判定是这样"，而不是
/// "临时目录建对了，判定是这样"。
struct InlineBodies(BTreeMap<String, String>);

impl InlineBodies {
    fn new(pairs: Vec<(BlobRef, &str)>) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(reference, body)| (reference.to_string(), body.to_string()))
                .collect(),
        )
    }
}

impl BodySource for InlineBodies {
    fn body_of(&self, blob_ref: &BlobRef) -> Option<String> {
        self.0.get(blob_ref.as_str()).cloned()
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
    let review = review_candidate(
        0,
        &claim("版本是 sha256:aaa", &["a"]),
        &ledger,
        &NoBodies,
        &low,
    );
    assert!(
        review
            .outcomes
            .iter()
            .all(|outcome| outcome.kind != VerificationKind::CounterExample),
        "低风险不搜反例"
    );

    let high = ReviewPolicy::for_risk(ActionLevel::A3, ActionLevel::A2, 8);
    assert!(high.counter_example);
    let review = review_candidate(
        0,
        &claim("版本是 sha256:aaa", &["a"]),
        &ledger,
        &NoBodies,
        &high,
    );
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
        &NoBodies,
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
    let outcome = check_claim_grounding(
        &claim("file:x 的版本是 sha256:aaa", &["real"]),
        &ledger,
        &NoBodies,
    )
    .expect("适用");
    assert_eq!(outcome.verdict, Verdict::Supported);
}

#[test]
fn a_prose_claim_without_values_is_simply_not_checked() {
    // "不适用"与"无法判定"是两件事。报成 Inconclusive 会让每次审查都看起来做了三件事，
    // 而实际上可能一件也没做。
    let ledger = ledger_with(vec![record("real", "file:x", "sha256:aaa", "reader", &[])]);
    assert!(
        check_claim_grounding(&claim("摘要文件已经更新", &["real"]), &ledger, &NoBodies).is_none(),
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
    let reviews = review_all(&set, &ledger, &NoBodies, &policy);
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
    let reviews = review_all(&set, &ledger, &NoBodies, &policy);
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

    let first = review_all(&set, &ledger, &NoBodies, &policy);
    let second = review_all(&set, &ledger, &NoBodies, &policy);
    assert_eq!(first, second);

    let selection_policy = SelectionPolicy::default();
    let left = select(&set, first, &selection_policy, ActionLevel::A3).expect("选择");
    let right = select(&set, second, &selection_policy, ActionLevel::A3).expect("选择");
    assert_eq!(left, right);
}

// ---------------------------------------------------------------------------
// 数字来源（§15.1 第 3 步）
// ---------------------------------------------------------------------------

/// 一份正文。
const SUMMARY_BODY: &str = "资料摘要\n- 项目代号：晨星\n- 下一次评审：2026-10-15\n";

#[test]
fn a_date_that_is_not_in_the_cited_body_is_refuted() {
    // §15.1 第 3 步："核验单元检查引用存在、**数字来源**和遗漏。"
    //
    // 在正文进上下文之前，这条查不出来：核对器只能拿版本摘要跟命题对，而版本摘要里
    // 没有"2026-10-15"这种东西。草稿写一个日期，谁也拦不住。
    let ledger = ledger_with(vec![record_with_a_body("a", "file:summary.md", "sha256:aaa")]);
    let bodies = InlineBodies::new(vec![(blob_ref("a"), SUMMARY_BODY)]);

    let outcome = check_claim_grounding(
        &claim("摘要里说下一次评审是 2026-12-01", &["a"]),
        &ledger,
        &bodies,
    )
    .expect("正文在手，这一条适用");

    assert_eq!(outcome.verdict, Verdict::Refuted);
    assert_eq!(
        outcome.evidence_refs,
        vec![reference("a")],
        "数字找不到出处时，该看的是命题自己引的那些——它们本该提供出处"
    );
}

#[test]
fn a_date_that_is_in_the_cited_body_is_supported() {
    // 对照组。少了它，"挡住了"与"全挡了"分不开——而一条永远否定的规则看起来和一条
    // 正确的规则一模一样。
    let ledger = ledger_with(vec![record_with_a_body("a", "file:summary.md", "sha256:aaa")]);
    let bodies = InlineBodies::new(vec![(blob_ref("a"), SUMMARY_BODY)]);

    let outcome = check_claim_grounding(
        &claim("摘要里说下一次评审是 2026-10-15", &["a"]),
        &ledger,
        &bodies,
    )
    .expect("适用");
    assert_eq!(outcome.verdict, Verdict::Supported);
}

#[test]
fn the_number_check_does_not_apply_when_no_cited_evidence_has_a_body() {
    // 拿一份版本摘要去核"下一次评审是 2026-12-01"，只会把所有带数字的命题一律判成否定。
    // 那不是发现问题，那是**把"没有材料"误报成"材料不对"**。
    let ledger = ledger_with(vec![record("a", "file:summary.md", "sha256:aaa", "reader", &[])]);

    assert!(
        check_claim_grounding(
            &claim("摘要里说下一次评审是 2026-12-01", &["a"]),
            &ledger,
            &NoBodies
        )
        .is_none(),
        "没有可核的正文时，这一条不适用——而不是否定"
    );
}

#[test]
fn a_short_number_is_not_treated_as_an_assertion() {
    // 这条划出的正是这个检查的能力边界，写出来免得它被当成比实际更强的东西。
    //
    // 单个数字在正文里太容易撞上（"第 3 步"、"共 4 项"），把它当断言核，结果是每一份写得
    // 正常的草稿都被判成否定。所以只认**日期**和**长度不少于四位**的数字串——代价是
    // `版本 42` 这种短数字不会被查。
    let ledger = ledger_with(vec![record_with_a_body("a", "file:summary.md", "sha256:aaa")]);
    let bodies = InlineBodies::new(vec![(blob_ref("a"), SUMMARY_BODY)]);

    assert!(
        check_claim_grounding(&claim("本次共 3 项变更", &["a"]), &ledger, &bodies).is_none(),
        "短数字不核——不适用，而不是支持"
    );
}

#[test]
fn a_body_that_cannot_be_fetched_makes_the_claim_unusable() {
    // §7.2 的"引用必须能解析为存在**且仍可访问**的证据"，落到正文这一层就是：
    // **这条内容还读不读得回来**。一条引用了已经按保留期清理掉的正文的结论，
    // 与一条引用了已撤回证据的结论，对"还该不该算数"的答案是一样的。
    let ledger = ledger_with(vec![record_with_a_body("a", "file:summary.md", "sha256:aaa")]);

    let outcome = check_evidence_access(
        &claim("摘要里说下一次评审是 2026-10-15", &["a"]),
        &ledger,
        &NoBodies,
    )
    .expect("引用指着一条取不回的正文");
    assert_eq!(outcome.kind, VerificationKind::EvidenceAccess);
    assert_eq!(outcome.verdict, Verdict::Refuted);
}

#[test]
fn a_bare_four_digit_run_is_also_checked() {
    // 四位以上的数字串（不是日期的一部分）同样要核。量级类的错——预算、条数、版本号——
    // 都落在这一类里。
    let body = "本轮预算上限 4096 个 token。";
    let ledger = ledger_with(vec![record_with_a_body("a", "file:x", "sha256:aaa")]);
    let bodies = InlineBodies::new(vec![(blob_ref("a"), body)]);

    assert_eq!(
        check_claim_grounding(&claim("本轮预算上限 4096", &["a"]), &ledger, &bodies)
            .expect("适用")
            .verdict,
        Verdict::Supported
    );
    assert_eq!(
        check_claim_grounding(&claim("本轮预算上限 8192", &["a"]), &ledger, &bodies)
            .expect("适用")
            .verdict,
        Verdict::Refuted
    );
}

// ---------------------------------------------------------------------------
// 证据可用性（§7.2）
// ---------------------------------------------------------------------------

#[test]
fn a_retracted_evidence_is_still_on_the_ledger_but_no_longer_material() {
    // §7.2 要求引用"存在**且仍可访问**"。撤回删的不是记录，是它的身份：从"可以拿来下结论
    // 的材料"变成"只在审计里看得见的历史"。
    let mut ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader-1", &[])]);
    assert_eq!(ledger.retract(&[reference("a")], "capability_revoked"), 1);

    assert!(
        ledger.get(&reference("a")).is_none(),
        "默认入口只给能用的东西"
    );
    let audit = ledger
        .get_including_retracted(&reference("a"))
        .expect("审计还看得见");
    assert_eq!(audit.observed_value, "sha256:aaa", "它当时说了什么仍然可查");
    assert_eq!(audit.retracted.as_deref(), Some("capability_revoked"));
    assert_eq!(ledger.records().len(), 1, "记录没有被删掉");
    assert_eq!(ledger.retracted_count(), 1);
    assert!(ledger.about("file:x").is_empty(), "它不再在手上");
}

#[test]
fn retracting_the_same_evidence_twice_keeps_the_first_reason() {
    // 第一次撤回的原因才是它为什么不可用的原因。后到的只是一次重放——而同一条证据被同一次
    // 撤回重放多次，是这套流程里最常见的情形（每个引用它的候选都会走一遍）。
    let mut ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader-1", &[])]);
    assert_eq!(ledger.retract(&[reference("a")], "capability_revoked"), 1);
    assert_eq!(ledger.retract(&[reference("a")], "retention_expired"), 0);
    assert_eq!(
        ledger
            .get_including_retracted(&reference("a"))
            .expect("存在")
            .retracted
            .as_deref(),
        Some("capability_revoked")
    );
}

#[test]
fn retracting_a_reference_the_ledger_never_saw_changes_nothing() {
    // 那是"缺失来源"，归 L2 的存在性校验管，不该在这里被算成一次生效的撤回——把两者混成
    // 一件事，撤回报告里的条数就会把程序缺陷算成权限变化。
    let mut ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader-1", &[])]);
    assert_eq!(
        ledger.retract(&[reference("ghost")], "capability_revoked"),
        0
    );
    assert_eq!(ledger.retracted_count(), 0);
}

#[test]
fn a_claim_citing_retracted_evidence_is_refuted() {
    let mut ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader-1", &[])]);
    let target = claim("file:x 的版本是 sha256:aaa", &["a"]);
    assert!(
        check_evidence_access(&target, &ledger, &NoBodies).is_none(),
        "撤回之前没有问题可报"
    );

    ledger.retract(&[reference("a")], "capability_revoked");

    let outcome =
        check_evidence_access(&target, &ledger, &NoBodies).expect("撤回之后要报出来");
    assert_eq!(outcome.kind, VerificationKind::EvidenceAccess);
    assert_eq!(outcome.verdict, Verdict::Refuted);
    assert_eq!(outcome.evidence_refs, vec![reference("a")]);

    // 而且它真的出局——不是只多了一条"注意"。
    let review = review_candidate(
        0,
        &target,
        &ledger,
        &NoBodies,
        &ReviewPolicy::for_risk(ActionLevel::A1, ActionLevel::A2, 8),
    );
    assert!(review.is_refuted(), "被否定的候选不该再参与竞争");
}

#[test]
fn a_claim_citing_only_available_evidence_reports_nothing() {
    // 这条判定只在**发现问题**时上报。全部可用时它没有正面结论可报——"这些证据都能用"
    // 是一条没有信息量的判定，写进每一份档案只会让每条候选多一行不变的话。
    let ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader-1", &[])]);
    assert!(
        check_evidence_access(
            &claim("file:x 的版本是 sha256:aaa", &["a"]),
            &ledger,
            &NoBodies
        )
        .is_none()
    );
}

#[test]
fn a_retracted_observation_stops_counting_toward_independent_sources() {
    // 撤回的证据不能拿去凑独立来源——那正是"用作废材料压下结论"最直接的一种。
    let mut ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    let refs = vec![reference("a"), reference("b")];
    assert_eq!(ledger.independent_source_count(&refs), 2);

    ledger.retract(&[reference("b")], "capability_revoked");
    assert_eq!(
        ledger.independent_source_count(&refs),
        1,
        "撤回之后只剩一个来源，门槛不该还按两个算"
    );
}

#[test]
fn a_retracted_observation_stops_counting_as_a_counter_example() {
    // 反例也不行。一条被撤回的观测不是"另一个观测者看到了别的东西"，它是一份作废的材料——
    // 拿它去否定一条结论，等于用撤回掉的东西推翻撤回之后的结论。
    let mut ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:bbb", "reader-2", &[]),
    ]);
    let target = claim("file:x 的版本是 sha256:aaa", &["a"]);
    assert_eq!(
        search_counter_example(&target, &ledger)
            .expect("有反例")
            .verdict,
        Verdict::Refuted
    );

    ledger.retract(&[reference("b")], "capability_revoked");
    assert_eq!(
        search_counter_example(&target, &ledger)
            .expect("仍然有一份材料")
            .verdict,
        Verdict::Inconclusive,
        "撤回之后没有可用的反例了"
    );
}

#[test]
fn a_retracted_value_is_still_recognized_so_a_claim_asserting_it_gets_caught() {
    // `known_values` **不过滤**已撤回的值，这是一个刻意的例外。它认得出那个值，才能说清
    // "你断言的是一条已经作废的观测里的东西"；把已撤回的值也过滤掉，同一份命题会变成
    // "断言了一个谁也没见过的值"——那是另一个问题，而且难归因得多。
    let mut ledger = ledger_with(vec![record("a", "file:x", "sha256:aaa", "reader-1", &[])]);
    ledger.retract(&[reference("a")], "capability_revoked");

    assert_eq!(ledger.known_values(), vec!["sha256:aaa"], "仍然认得出来");
    let outcome = check_claim_grounding(
        &claim("file:x 的版本是 sha256:aaa", &["a"]),
        &ledger,
        &NoBodies,
    )
    .expect("有东西可核");
    assert_eq!(outcome.verdict, Verdict::Refuted);
}

#[test]
fn the_access_check_runs_before_the_others() {
    // 对一条引用了已撤回证据的候选做"结论依据核对"和"来源核对"，等于在一个已经不该存在的
    // 问题上花两次预算——而 §4.1 L3 是有预算的。
    let mut ledger = ledger_with(vec![
        record("a", "file:x", "sha256:aaa", "reader-1", &[]),
        record("b", "file:x", "sha256:aaa", "reader-2", &[]),
    ]);
    ledger.retract(&[reference("a"), reference("b")], "capability_revoked");

    let review = review_candidate(
        0,
        &claim("file:x 的版本是 sha256:aaa", &["a", "b"]),
        &ledger,
        &NoBodies,
        &ReviewPolicy::for_risk(ActionLevel::A1, ActionLevel::A2, 8),
    );
    assert_eq!(
        review.outcomes[0].kind,
        VerificationKind::EvidenceAccess,
        "可用性排在最前面：{:?}",
        review.outcomes
    );
    assert!(review.is_refuted());
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
    let reviews = review_all(&set, &ledger, &NoBodies, &policy);
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
