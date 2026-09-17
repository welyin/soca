//! §4.1 L2 工作空间与 §6 第 4 步的回归测试。
//!
//! 针对的是两句话：
//!
//! > 黑板有界，证据不能被执行层改写
//!
//! > L2验证候选结构与证据存在性

use soca_contracts::*;

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("基准时间")
}

fn at(offset: i64) -> WallClock {
    base_time().plus_seconds(offset)
}

fn unit(name: &str) -> UnitId {
    UnitId::new(format!("unit:{name}")).expect("固定单元")
}

fn evidence(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn finding(statement: &str, evidence_name: &str) -> WorkspaceNote {
    WorkspaceNote::Finding {
        statement: statement.to_string(),
        evidence_refs: vec![evidence(evidence_name)],
    }
}

#[test]
fn a_note_without_evidence_is_refused() {
    // 每条笔记都必须携带证据。既然证据只能随笔记进入黑板，而笔记又必须带证据，
    // "黑板上有一条不带证据的结论"就不可能发生。
    let mut workspace = Workspace::new();
    let result = workspace.post(
        "file:X",
        WorkspaceNote::Finding {
            statement: "文件已经更新".to_string(),
            evidence_refs: Vec::new(),
        },
        &unit("a"),
        at(0),
    );
    assert_eq!(
        result,
        Err(ContractError::MissingRefs {
            field: "workspace_note.evidence_refs"
        })
    );
    assert!(workspace.is_empty(), "被拒的笔记不留痕");
}

#[test]
fn evidence_only_grows() {
    // §4.1："证据不能被执行层改写"。本类型没有任何删除或替换证据的方法，
    // 所以证据集合只能增。这个测试断言的是那个**不存在的** API 所造成的可观察后果。
    let mut workspace = Workspace::new();
    assert!(workspace.evidence().is_empty());

    workspace
        .post("file:X", finding("版本是 a", "1"), &unit("a"), at(0))
        .expect("写入");
    assert_eq!(workspace.evidence().len(), 1);

    workspace
        .post("file:X", finding("版本是 b", "2"), &unit("a"), at(1))
        .expect("写入");
    assert_eq!(workspace.evidence().len(), 2);

    // 同一条证据再写一次不重复计数。
    workspace
        .post("file:X", finding("版本仍是 a", "1"), &unit("a"), at(2))
        .expect("写入");
    assert_eq!(workspace.evidence().len(), 2);
    assert!(workspace.has_evidence(&evidence("1")));
    assert!(workspace.has_evidence(&evidence("2")));
}

#[test]
fn notes_on_a_topic_are_appended_not_overwritten() {
    // §4.2 的"不能只拼接子摘要"在 L2 上的对应要求：旧的子结果不能被新结论挤掉，
    // 否则重放时读不到当初的依据。
    let mut workspace = Workspace::new();
    workspace
        .post("file:X", finding("第一次看见 a", "1"), &unit("a"), at(0))
        .expect("写入");
    workspace
        .post("file:X", finding("第二次看见 b", "2"), &unit("b"), at(1))
        .expect("写入");

    let entry = workspace.entry("file:X").expect("主题存在");
    assert_eq!(entry.notes.len(), 2, "两条笔记都要在");
    assert_eq!(entry.revision, 2);
    assert_eq!(entry.posted_by, unit("b"), "最后写入者被更新");
    assert!(
        entry.notes[0].summary().contains('a'),
        "先写的那条仍然在原处"
    );
}

#[test]
fn the_topic_count_is_bounded() {
    // §4.1 L2："各组合边界有小空间，不无限复制"。
    let mut workspace = Workspace::new();
    for index in 0..MAX_WORKSPACE_TOPICS {
        workspace
            .post(
                format!("topic:{index}"),
                finding("看见了", &format!("e{index}")),
                &unit("a"),
                at(0),
            )
            .expect("前 N 个主题应当成功");
    }
    assert_eq!(workspace.topic_count(), MAX_WORKSPACE_TOPICS);

    let overflow = workspace.post(
        "topic:overflow",
        finding("看见了", "e-overflow"),
        &unit("a"),
        at(0),
    );
    assert_eq!(
        overflow,
        Err(ContractError::WorkspaceTopicLimitExceeded {
            limit: MAX_WORKSPACE_TOPICS,
            actual: MAX_WORKSPACE_TOPICS + 1
        })
    );
}

#[test]
fn the_byte_count_is_bounded() {
    // 字节界与主题界是两个独立的界：同一主题上无限追加笔记同样要拦住，
    // 否则"有界"只是"主题数量有界"。
    let mut workspace = Workspace::new();
    let bulk = "x".repeat(8 * 1024);

    let mut hit = None;
    for index in 0..200 {
        match workspace.post("topic:bulk", finding(&bulk, &format!("e{index}")), &unit("a"), at(0)) {
            Ok(()) => {}
            Err(error) => {
                hit = Some(error);
                break;
            }
        }
    }

    assert!(
        matches!(
            hit,
            Some(ContractError::WorkspaceByteLimitExceeded { .. })
        ),
        "同一主题上的无限追加必须被字节界拦住，实际：{hit:?}"
    );
    assert!(workspace.bytes() <= MAX_WORKSPACE_BYTES);
}

// ---------------------------------------------------------------------------
// §6 第 4 步：L2 验证证据存在性
// ---------------------------------------------------------------------------

#[test]
fn l2_refuses_a_claim_citing_evidence_it_never_saw() {
    // 这是 `EvidenceRef` 有分量的原因。没有这道门，它只是一个前缀正确的字符串，
    // 任何实现方——尤其是后面接上来的、由模型驱动的叶单元——都能编一个 `obs:xxx` 当证据
    // 用，而全部结构校验都会通过。
    let mut workspace = Workspace::new();
    workspace
        .post("file:X", finding("版本是 a", "1"), &unit("a"), at(0))
        .expect("写入");

    // 引用了黑板上确实有的证据 → 通过。
    let honest = CandidateSet {
        candidates: vec![Candidate::Claim {
            statement: "file:X 的版本是 a".to_string(),
            evidence_refs: vec![evidence("1")],
        }],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    assert!(workspace.validate_candidates(&honest).is_ok());

    // 引用了从没上过黑板的证据 → 拒绝。
    let fabricated = CandidateSet {
        candidates: vec![Candidate::Claim {
            statement: "file:X 的版本是 z".to_string(),
            evidence_refs: vec![evidence("never-posted")],
        }],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    assert_eq!(
        workspace.validate_candidates(&fabricated),
        Err(ContractError::EvidenceNotOnWorkspace {
            evidence_ref: "obs:never-posted".to_string()
        })
    );
}

#[test]
fn l2_also_checks_conflict_positions_against_the_workspace() {
    // 冲突立场里的证据同样要存在。只检查候选而放过冲突，等于给"编造的反方观点"
    // 开了一个后门——而 §4.2 恰恰要求反方必须带证据。
    let mut workspace = Workspace::new();
    workspace
        .post("file:X", finding("版本是 a", "1"), &unit("a"), at(0))
        .expect("写入");
    workspace
        .post("file:X", finding("版本是 b", "2"), &unit("b"), at(0))
        .expect("写入");

    let set = CandidateSet {
        candidates: Vec::new(),
        conflicts: vec![Conflict {
            subject_ref: "file:X".to_string(),
            positions: vec![
                ConflictPosition {
                    statement: "版本是 a".to_string(),
                    by: unit("a"),
                    evidence_refs: vec![evidence("1")],
                },
                ConflictPosition {
                    statement: "版本是 b".to_string(),
                    by: unit("b"),
                    evidence_refs: vec![evidence("fabricated")],
                },
            ],
        }],
        unresolved: Vec::new(),
    };
    assert_eq!(
        workspace.validate_candidates(&set),
        Err(ContractError::EvidenceNotOnWorkspace {
            evidence_ref: "obs:fabricated".to_string()
        })
    );
}

#[test]
fn l2_applies_the_candidate_set_rules_too() {
    // §6 第 4 步说的"验证候选**结构**"。结构规则不在 L2 里另写一份，而是复用候选集合
    // 自己的校验——两处各写一份，迟早会分叉。
    let workspace = Workspace::new();
    let structurally_invalid = CandidateSet {
        candidates: vec![Candidate::Claim {
            statement: "没有证据的结论".to_string(),
            evidence_refs: Vec::new(),
        }],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    assert_eq!(
        workspace.validate_candidates(&structurally_invalid),
        Err(ContractError::MissingRefs {
            field: "candidate.claim.evidence_refs"
        }),
        "结构规则先于证据存在性检查"
    );
}
