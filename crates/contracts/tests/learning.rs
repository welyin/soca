//! 策略改进的准入（§13.2 的第二种学习）。
//!
//! §13.2 那两句是：
//!
//! > 记忆/策略改进：摘要、规则和技能作为候选，经**回放与保留任务集检验**后**版本化启用**；
//! > 不覆盖原证据。
//! > ……候选策略必须经过准入。系统可建议新的单元或拓扑，但**没有自行安装可执行代码、
//! > 修改签名策略或提高权限的能力**。
//!
//! 三组最要紧的性质：
//!
//! * **只能变严。** 一个把门槛降下来的候选，不论它在保留集上表现多好，都不该被放行——
//!   而它在保留集上确实表现得好：放宽当然不会误伤。
//! * **误伤是唯一的代价。** 提高门槛会挡住已知正确的结论，而那是拒绝准入的理由，
//!   不是"再看一眼"的理由。
//! * **版本号由内容派生。** 同一版号两种内容，会让"这一轮用的是哪一版"永远答不上来。

use soca_contracts::{
    ActionLevel, HoldoutSet, RecordedConclusion, RetryWhen, SelectionPolicy, StrategyAdmission,
    StrategyCandidate, admit, evaluate_holdout, propose_strategy, strategy_version_of,
};

fn policy(base: usize, extra: usize, from: ActionLevel, checks: usize) -> SelectionPolicy {
    SelectionPolicy {
        base_evidence: base,
        high_risk_extra: extra,
        high_risk_from: from,
        max_checks: checks,
    }
}

fn incumbent() -> SelectionPolicy {
    policy(1, 2, ActionLevel::A2, 8)
}

fn conclusion(memory_id: &str, evidence_count: usize) -> RecordedConclusion {
    RecordedConclusion {
        memory_id: memory_id.to_string(),
        evidence_count,
        why: "测试".to_string(),
    }
}

fn holdout(right: &[(&str, usize)], wrong: &[(&str, usize)]) -> HoldoutSet {
    HoldoutSet {
        known_right: right
            .iter()
            .map(|(id, count)| conclusion(id, *count))
            .collect(),
        known_wrong: wrong
            .iter()
            .map(|(id, count)| conclusion(id, *count))
            .collect(),
    }
}

/// 造一个候选。版本号由内容派生——不这么造的话，第一条检查就会把它挡下来。
fn candidate(p: SelectionPolicy, based_on: &[&str]) -> StrategyCandidate {
    StrategyCandidate {
        version: strategy_version_of(&p).expect("派生版本"),
        policy: p,
        rationale: "测试".to_string(),
        based_on: based_on.iter().map(|id| id.to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// 只能变严（§13.2 的"没有提高权限的能力"）
// ---------------------------------------------------------------------------

#[test]
fn the_stricter_of_two_policies_is_taken_on_every_dimension() {
    // 四个维度的方向不一样，而搞反一维不会有任何报错——它只会让系统在某类动作上悄悄松一点。
    //
    // 注意 `high_risk_extra` 两边都是 2：一个 `extra` 更低的策略**不是**更严的——它只是
    // 在高风险档上松一点，而那正是最容易看漏的那一维。
    let strict = policy(5, 2, ActionLevel::A0, 8);
    let loose = policy(1, 2, ActionLevel::A2, 4);

    let merged = loose.tightened_by(&strict);
    assert_eq!(merged.base_evidence, 5, "证据条数越高越严");
    assert_eq!(merged.high_risk_extra, 2, "高风险加码越高越严");
    assert_eq!(
        merged.high_risk_from,
        ActionLevel::A0,
        "『从哪一档算高风险』越低越严——更多档被当成高风险"
    );
    assert_eq!(merged.max_checks, 8, "核验预算给少了就是放松");

    // 而"更严的夹更松的"还是它自己：这条性质是准入检查能用一句话写出来的原因。
    assert!(strict.is_at_least_as_strict_as(&loose));
    assert!(!loose.is_at_least_as_strict_as(&strict));
}

#[test]
fn a_candidate_that_loosens_anything_is_refused_however_good_it_looks() {
    // §13.2："系统……**没有自行……提高权限的能力**。"
    //
    // 注意它在保留集上的表现**很好**：放宽门槛当然不会误伤任何一条已知正确的结论。
    // 所以检查的顺序不能反——先查候选本身合不合法，再看它表现好不好。
    let set = holdout(&[("m:right", 1)], &[("m:wrong", 4)]);
    let loosened = candidate(policy(1, 1, ActionLevel::A2, 8), &["m:wrong"]);

    let admission = admit(&loosened, &incumbent(), &set, ActionLevel::A0).expect("判定");
    match admission {
        StrategyAdmission::Refused {
            reason,
            retry_when,
            ..
        } => {
            assert!(reason.contains("更松"), "理由要说是放宽：{reason}");
            assert_eq!(
                retry_when,
                RetryWhen::Never,
                "放宽不是等出来的——它要有人去改配置"
            );
        }
        other => panic!("放宽的候选不该被放行：{other:?}"),
    }
}

#[test]
fn a_candidate_that_only_tightens_can_pass() {
    // 对照组。少了它，"只能变严"与"什么也不许改"分不开。
    let set = holdout(&[("m:right", 3)], &[("m:wrong", 2)]);
    let tightened = candidate(policy(3, 2, ActionLevel::A2, 8), &["m:wrong"]);

    let admission = admit(&tightened, &incumbent(), &set, ActionLevel::A0).expect("判定");
    assert!(admission.is_admitted(), "{admission:?}");
    assert_eq!(admission.report().bar, 3, "A0 下 bar = base");
}

// ---------------------------------------------------------------------------
// 保留任务集（§13.2 的"回放与保留任务集检验"）
// ---------------------------------------------------------------------------

#[test]
fn blocking_a_known_right_conclusion_is_the_reason_to_refuse() {
    // 误伤是这类改动唯一真正的代价。§13.2 要"经保留任务集检验"，而这条检验要回答的
    // 问题就一句：**这个改动会不会把对的也一起挡掉？**
    let set = holdout(&[("m:right", 1)], &[("m:wrong", 2)]);
    let too_high = candidate(policy(3, 2, ActionLevel::A2, 8), &["m:wrong"]);

    let admission = admit(&too_high, &incumbent(), &set, ActionLevel::A0).expect("判定");
    match admission {
        StrategyAdmission::Refused {
            reason,
            retry_when,
            report,
        } => {
            assert!(reason.contains("误伤"), "要说清是哪一类问题：{reason}");
            assert_eq!(report.would_block_right, vec!["m:right".to_string()]);
            // 等那条**样本自己**变了：被更正、被撤回，或者补上了更多证据。
            assert_eq!(retry_when, RetryWhen::WhenEvidenceChanges);
        }
        other => panic!("会误伤的候选不该被放行：{other:?}"),
    }
}

#[test]
fn a_candidate_that_does_not_fix_the_mistake_it_cites_is_refused() {
    // 只看"没误伤"是不够的：一个把门槛提到天上、把所有结论都挡掉的策略也不会误伤。
    // 它得真的解决了**它声称解决的**那一条。
    let set = holdout(&[], &[("m:wrong", 5)]);
    let too_low = candidate(policy(2, 2, ActionLevel::A2, 8), &["m:wrong"]);

    let admission = admit(&too_low, &incumbent(), &set, ActionLevel::A0).expect("判定");
    match admission {
        StrategyAdmission::Refused {
            reason,
            retry_when,
            report,
        } => {
            assert!(reason.contains("没有解决它声称"), "{reason}");
            assert_eq!(report.still_admitted_wrong, vec!["m:wrong".to_string()]);
            // 门槛至少要提到 6 条（那条错案手上有 5 条）。差多少要说得出数字。
            assert_eq!(retry_when, RetryWhen::MoreEvidence { short_by: 4 });
        }
        other => panic!("没解决它声称的问题的候选不该被放行：{other:?}"),
    }
}

#[test]
fn a_candidate_citing_nothing_real_is_refused() {
    // 没有依据的改动是一次猜测，而猜测不该借着"学习"的名义改掉判定标准。
    let set = holdout(&[], &[("m:wrong", 2)]);
    let unfounded = candidate(policy(3, 2, ActionLevel::A2, 8), &[]);

    let admission = admit(&unfounded, &incumbent(), &set, ActionLevel::A0).expect("判定");
    assert!(!admission.is_admitted());

    // 而引了一条**不在保留集里**的记录，也不等于有依据：它得指向一条真被判错过的结论。
    let nowhere = candidate(policy(3, 2, ActionLevel::A2, 8), &["m:imaginary"]);
    let admission = admit(&nowhere, &incumbent(), &set, ActionLevel::A0).expect("判定");
    assert!(
        !admission.is_admitted(),
        "引一条不存在的记录不算依据：{admission:?}"
    );
}

#[test]
fn an_empty_holdout_admits_nothing() {
    // 空集不是"检验通过"。没有已知答案时，准入闸没有任何依据——
    // 而"没有依据"与"依据支持它"在报告上必须长得不一样。
    let proposal = candidate(policy(3, 2, ActionLevel::A2, 8), &["m:wrong"]);
    let admission = admit(&proposal, &incumbent(), &HoldoutSet::default(), ActionLevel::A0)
        .expect("判定");
    match admission {
        StrategyAdmission::Refused { reason, .. } => {
            assert!(reason.contains("空的"), "{reason}");
        }
        other => panic!("空保留集不该放行任何东西：{other:?}"),
    }
}

#[test]
fn the_holdout_report_counts_both_directions() {
    let set = holdout(&[("m:r1", 1), ("m:r2", 5)], &[("m:w1", 2), ("m:w2", 9)]);
    let report = evaluate_holdout(&policy(3, 0, ActionLevel::A2, 8), &set, ActionLevel::A0);

    assert_eq!(report.bar, 3);
    assert_eq!(report.would_block_right, vec!["m:r1".to_string()], "1 条的会被挡");
    assert_eq!(
        report.still_admitted_wrong,
        vec!["m:w2".to_string()],
        "9 条的仍然放行——那是没清完的账，不是失败"
    );
}

// ---------------------------------------------------------------------------
// 版本化启用（§13.2）
// ---------------------------------------------------------------------------

#[test]
fn the_version_is_derived_from_the_content() {
    let first = policy(1, 2, ActionLevel::A2, 8);
    assert_eq!(
        strategy_version_of(&first).expect("派生"),
        strategy_version_of(&first).expect("派生"),
        "同一个策略永远得到同一个版本号"
    );

    // 四个维度随便动一个，版本号都要变——包括"核验预算"这种看起来与门槛无关的。
    for changed in [
        policy(2, 2, ActionLevel::A2, 8),
        policy(1, 3, ActionLevel::A2, 8),
        policy(1, 2, ActionLevel::A1, 8),
        policy(1, 2, ActionLevel::A2, 7),
    ] {
        assert_ne!(
            strategy_version_of(&first).expect("派生"),
            strategy_version_of(&changed).expect("派生"),
            "换了内容就该换版本号：{changed:?}"
        );
    }
}

#[test]
fn a_version_that_does_not_match_its_content_is_refused() {
    // §7.2 的"标识不得复用"在这里也成立：同一版号、两种内容，会让"这一轮用的是哪一版"
    // 永远答不上来——而那是出事之后第一个要回答的问题。
    let set = holdout(&[], &[("m:wrong", 2)]);
    let mut forged = candidate(policy(3, 2, ActionLevel::A2, 8), &["m:wrong"]);
    forged.version = strategy_version_of(&incumbent()).expect("派生");

    let admission = admit(&forged, &incumbent(), &set, ActionLevel::A0).expect("判定");
    match admission {
        StrategyAdmission::Refused { reason, .. } => {
            assert!(reason.contains("对不上"), "{reason}");
        }
        other => panic!("版本号与内容不符的候选不该被放行：{other:?}"),
    }
}

#[test]
fn a_candidate_carries_data_and_nothing_executable() {
    // §13.2："没有自行安装**可执行代码**、修改**签名策略**或**提高权限**的能力。"
    //
    // 前两条是结构性的，所以这条测试做的是**看一眼能表达什么**：候选序列化出来的字段就这些，
    // 里面没有函数、没有脚本，也没有任何通向签名策略（`PolicyVersion`）的路。
    let encoded = serde_json::to_value(candidate(
        policy(3, 2, ActionLevel::A2, 8),
        &["m:wrong"],
    ))
    .expect("可序列化");
    let mut keys: Vec<&str> = encoded
        .as_object()
        .expect("对象")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["based_on", "policy", "rationale", "version"]);

    let mut policy_keys: Vec<&str> = encoded["policy"]
        .as_object()
        .expect("对象")
        .keys()
        .map(String::as_str)
        .collect();
    policy_keys.sort_unstable();
    assert_eq!(
        policy_keys,
        vec![
            "base_evidence",
            "high_risk_extra",
            "high_risk_from",
            "max_checks"
        ],
        "能表达的只有这四个数字"
    );
}

// ---------------------------------------------------------------------------
// 提议（§13.2 的"系统可建议"）
// ---------------------------------------------------------------------------

#[test]
fn the_system_suggests_raising_the_bar_just_enough_to_block_the_worst_mistake() {
    // §13.2 说系统"可**建议**"。建议的东西也要过同一道闸——所以这里接着把它送进 `admit`，
    // 而不是直接相信提议方算出来的数。
    let set = holdout(&[("m:right", 3)], &[("m:wrong", 2)]);
    let suggestion = propose_strategy(&incumbent(), &set, ActionLevel::A0).expect("应当提一个");

    assert_eq!(suggestion.policy.base_evidence, 3, "bar 要刚好到 3");
    assert_eq!(suggestion.based_on, vec!["m:wrong".to_string()]);
    assert!(suggestion.rationale.contains("m:wrong"), "理由要指出依据");

    let admission = admit(&suggestion, &incumbent(), &set, ActionLevel::A0).expect("判定");
    assert!(admission.is_admitted(), "提议该过闸：{admission:?}");
}

#[test]
fn a_suggestion_the_holdout_cannot_support_is_refused_by_the_gate() {
    // 这条是这一项最要紧的样子：**系统提了一个它做不到的改动**，而闸挡住了它，
    // 并且说清了为什么。
    //
    // 那条对的结论手上只有 2 条证据。要把错案（3 条）挡在门外，门槛得到 4 —— 而 4 会连它一起挡。
    // 也就是说：这个错**不能用提高门槛来纠正**，得换个办法。这正是"保留任务集检验"存在的理由。
    let set = holdout(&[("m:right", 2)], &[("m:wrong", 3)]);
    let suggestion = propose_strategy(&incumbent(), &set, ActionLevel::A0).expect("应当提一个");
    assert_eq!(suggestion.policy.base_evidence, 4);

    let admission = admit(&suggestion, &incumbent(), &set, ActionLevel::A0).expect("判定");
    match admission {
        StrategyAdmission::Refused { reason, report, .. } => {
            assert!(reason.contains("误伤"), "{reason}");
            assert_eq!(report.would_block_right, vec!["m:right".to_string()]);
        }
        other => panic!("这个提议过不了闸：{other:?}"),
    }
}

#[test]
fn nothing_is_suggested_when_the_bar_already_covers_the_worst_mistake() {
    // 没有可提的时候要给 `None`，而不是提一个"再加一条"的空改动：
    // 后者会每一次都过闸、每一次都把版本号改掉，于是审计上看起来系统在不停地学，
    // 而它什么也没学到。
    //
    // 走到这一支的路是**换一个风险档**：同一条错案（2 条证据）在 A0 下够格、在 A2 下不够
    // （bar 是 base + extra = 3）。门槛已经拦得住它了，就没有什么好改的。
    let set = holdout(&[], &[("m:wrong", 2)]);
    assert!(
        propose_strategy(&incumbent(), &set, ActionLevel::A0).is_some(),
        "A0 下它刚好够格，是该提的"
    );
    assert!(
        propose_strategy(&incumbent(), &set, ActionLevel::A2).is_none(),
        "A2 下门槛本来就是 3，已经拦得住"
    );

    // 一条错案都没有时同理。
    assert!(propose_strategy(&incumbent(), &HoldoutSet::default(), ActionLevel::A0).is_none());
}
