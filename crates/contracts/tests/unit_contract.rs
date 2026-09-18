//! §3.1 最小认知单元合同的回归测试。
//!
//! 这些测试针对的是主架构 §3.1 那句判据：
//!
//! > 纯被动组件可以作为单元的工具，但**没有这个合同的函数不标为完整主体**。
//!
//! 所以本文件除了校验候选集合的结构规则，还真的实现一个叶单元跑完一轮，用来说明这份合同
//! 是可实现的，而不是一份没人能满足的规格。

use serde_json::json;
use soca_contracts::*;

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

fn base_time() -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("基准时间必须是合法 RFC 3339")
}

fn at(offset_seconds: i64) -> WallClock {
    base_time().plus_seconds(offset_seconds)
}

fn unit(name: &str) -> UnitId {
    UnitId::new(format!("unit:{name}")).expect("固定单元")
}

fn evidence(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn window(offset_start: i64, offset_end: i64) -> TimeWindow {
    TimeWindow::new(at(offset_start), at(offset_end)).expect("合法时间窗")
}

fn uncertainty_none() -> Uncertainty {
    Uncertainty {
        probability: None,
        notes: vec!["确定性换算，不需要概率字段".to_string()],
    }
}

fn position(by: &str, statement: &str, evidence_name: &str) -> ConflictPosition {
    ConflictPosition {
        statement: statement.to_string(),
        by: unit(by),
        evidence_refs: vec![evidence(evidence_name)],
    }
}

fn claim(statement: &str, evidence_name: &str) -> Candidate {
    Candidate::Claim {
        statement: statement.to_string(),
        evidence_refs: vec![evidence(evidence_name)],
    }
}

fn write_intent() -> ActionIntent {
    ActionIntent::new(
        ActionId::new("action:1").expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new("D:\\资料\\摘要").expect("固定范围"),
        json!({"path": "D:\\资料\\摘要\\summary.md", "content": "新的摘要内容"}),
        vec!["目标目录已授权".to_string()],
        PredictionRef::new("prediction:pred-1").expect("固定预测引用"),
        ActionLevel::A1,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 10,
        },
        unit("file-summary:07"),
    )
    .expect("合法动作意图")
}

// ---------------------------------------------------------------------------
// 候选集合的结构规则
// ---------------------------------------------------------------------------

#[test]
fn an_empty_candidate_set_is_legal() {
    // §6 第 9 步允许"有限预算内结束"。一个只能靠报错才能安静的单元会污染审计账。
    let set = CandidateSet::empty();
    assert!(set.is_empty());
    assert!(set.validate().is_ok());
    assert!(!set.requests_side_effect());
}

#[test]
fn a_conclusion_without_evidence_is_refused() {
    // §6 第 4 步："L2 验证候选结构与证据存在性"。没有证据的结论不构成候选。
    let set = CandidateSet {
        candidates: vec![Candidate::Claim {
            statement: "文件已经更新".to_string(),
            evidence_refs: Vec::new(),
        }],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    assert_eq!(
        set.validate(),
        Err(ContractError::MissingRefs {
            field: "candidate.claim.evidence_refs"
        })
    );
}

#[test]
fn a_conflict_needs_two_sides() {
    // 一个立场不构成冲突，那是"主张"。
    let set = CandidateSet {
        candidates: Vec::new(),
        conflicts: vec![Conflict {
            subject_ref: "file:summary.md".to_string(),
            positions: vec![position("a", "哈希是 X", "obs-1")],
        }],
        unresolved: Vec::new(),
    };
    assert_eq!(
        set.validate(),
        Err(ContractError::ConflictNeedsTwoSides {
            subject_ref: "file:summary.md".to_string(),
            positions: 1
        })
    );
}

#[test]
fn a_conflict_within_one_unit_is_not_a_conflict() {
    // §4.2 说的冲突指的是**子单元之间**的分歧。同一个单元跟自己不构成冲突。
    let set = CandidateSet {
        candidates: Vec::new(),
        conflicts: vec![Conflict {
            subject_ref: "file:summary.md".to_string(),
            positions: vec![
                position("same", "哈希是 X", "obs-1"),
                position("same", "哈希是 Y", "obs-2"),
            ],
        }],
        unresolved: Vec::new(),
    };
    assert_eq!(
        set.validate(),
        Err(ContractError::ConflictNeedsDistinctUnits {
            subject_ref: "file:summary.md".to_string()
        })
    );
}

#[test]
fn a_conflict_position_without_evidence_is_refused() {
    // §7.2 的真相来源是原始可核验证据，不是多数意见。立场必须自己带证据才成立，
    // 否则"用多数意见覆盖矛盾"就还是可行的。
    let mut bare = position("other", "哈希是 Y", "obs-2");
    bare.evidence_refs.clear();

    let set = CandidateSet {
        candidates: Vec::new(),
        conflicts: vec![Conflict {
            subject_ref: "file:summary.md".to_string(),
            positions: vec![position("a", "哈希是 X", "obs-1"), bare],
        }],
        unresolved: Vec::new(),
    };
    assert_eq!(
        set.validate(),
        Err(ContractError::MissingRefs {
            field: "conflict.position.evidence_refs"
        })
    );
}

#[test]
fn an_unresolved_question_must_name_what_is_missing() {
    // 只写"还没想清楚"等于没写。写明缺口，上层才能决定补证据、升级审批还是收手。
    let set = CandidateSet {
        candidates: Vec::new(),
        conflicts: Vec::new(),
        unresolved: vec![Unresolved {
            question: "摘要该写多长".to_string(),
            missing: Vec::new(),
        }],
    };
    assert_eq!(
        set.validate(),
        Err(ContractError::MissingRefs {
            field: "unresolved.missing"
        })
    );
}

#[test]
fn too_many_candidates_are_refused() {
    // §4.1 L2："黑板有界"。界是编译期常量，不由提出方自行决定。
    let set = CandidateSet {
        candidates: (0..=MAX_CANDIDATES)
            .map(|index| claim(&format!("主张 {index}"), &format!("obs-{index}")))
            .collect(),
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    assert_eq!(
        set.validate(),
        Err(ContractError::CandidateLimitExceeded {
            limit: MAX_CANDIDATES,
            actual: MAX_CANDIDATES + 1
        })
    );
}

#[test]
fn only_a_request_action_may_have_a_side_effect() {
    // 申请观测、请求工具、提交结论都不改变世界。把它们和动作混为一谈，
    // "只有执行许可能触发副作用"（§7.2）这条边界在候选层就先糊掉了。
    let observation = Candidate::RequestObservation {
        subject_ref: "file:summary.md".to_string(),
        reason: "需要动作后的版本".to_string(),
    };
    let tool = Candidate::RequestTool {
        tool_id: ToolId::new("calc.eval").expect("固定工具"),
        parameters: json!({"expr": "1+1"}),
    };
    let action = Candidate::RequestAction {
        intent: Box::new(write_intent()),
    };

    assert!(!observation.may_have_side_effect());
    assert!(!tool.may_have_side_effect());
    assert!(!claim("已经写完", "obs-1").may_have_side_effect());
    assert!(action.may_have_side_effect());

    let set = CandidateSet {
        candidates: vec![observation, tool, claim("已经写完", "obs-1"), action],
        conflicts: Vec::new(),
        unresolved: Vec::new(),
    };
    assert!(set.validate().is_ok());
    assert!(set.requests_side_effect());
    assert_eq!(set.evidence_refs().len(), 1, "只有结论自带证据");
}

#[test]
fn a_cluster_keeps_child_conflicts_instead_of_voting() {
    // §4.2：父单元输出"包含子结果、关系索引、冲突、未决条件和证据引用，
    // 不能只拼接子摘要或用多数意见覆盖矛盾"。
    //
    // 这里三个兄弟叶单元里有两个维护不同版本、一个未表态。集合必须**原样保留**两方立场，
    // 而不是在 propose 里选边后只交出胜者。
    let set = CandidateSet {
        candidates: vec![claim("待核对", "obs-3")],
        conflicts: vec![Conflict {
            subject_ref: "file:summary.md".to_string(),
            positions: vec![
                position("version-watcher", "版本是 sha256:aaa", "obs-1"),
                position("structure-reader", "版本是 sha256:bbb", "obs-2"),
            ],
        }],
        unresolved: vec![Unresolved {
            question: "哪一个版本才是当前版本".to_string(),
            missing: vec!["两条证据的时间窗不同，需要一次同刻观测".to_string()],
        }],
    };

    assert!(set.validate().is_ok());

    let conflict = &set.conflicts[0];
    assert_eq!(conflict.positions.len(), 2, "两方立场都必须留存");
    assert_ne!(
        conflict.positions[0].statement, conflict.positions[1].statement,
        "选边消解会把两条主张压成一条，这正是被禁止的"
    );
    assert!(
        !set.unresolved.is_empty(),
        "矛盾未消解时必须留下未决条件，不能悄悄当成已解决"
    );
}

// ---------------------------------------------------------------------------
// 合同可实现：一个真的叶单元跑完一轮
// ---------------------------------------------------------------------------

/// §3.3 的"指定文件版本守望单元"的一个最小版本。
///
/// 它只负责一个对象：记录最近看到的内容版本，并在收到结果后比较预测与实际。
/// 它不常驻模型，也不持有任何 OS 写权限——§3.1 明确"不要求每个单元都有 OS 写权限"。
#[derive(Debug)]
struct VersionWatcher {
    unit_id: UnitId,
    scope: Scope,
    belief_revision: u64,
    last_value: Option<String>,
    evidence_refs: Vec<EvidenceRef>,
    last_verdict: Option<Verdict>,
}

impl VersionWatcher {
    fn new() -> Self {
        Self {
            unit_id: unit("version-watcher"),
            scope: Scope {
                domain: DomainId::new("document-summary").expect("固定领域"),
                task_contract: TaskContractVersion::new("summary-v1").expect("固定合同"),
            },
            belief_revision: 0,
            last_value: None,
            evidence_refs: Vec::new(),
            last_verdict: None,
        }
    }
}

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";

impl CognitiveUnit for VersionWatcher {
    fn unit_id(&self) -> &UnitId {
        &self.unit_id
    }

    fn kind(&self) -> UnitKind {
        UnitKind::Leaf
    }

    fn observe(&mut self, event: &Envelope, _at: WallClock) -> Result<(), ContractError> {
        // 事件载荷是公开观测。§6 第 1 步：原始内容与来源分开存。
        let PayloadRef::Inline { body, .. } = &event.payload_ref else {
            // 大载荷只传引用（§10.4）。这一版没有内容仓句柄，按"看不懂就忽略"处理，
            // 而不是猜内容。
            return Ok(());
        };
        let observation: Observation =
            serde_json::from_str(body).map_err(|_| ContractError::MalformedPayload {
                field: "envelope.payload_ref.body",
            })?;
        if observation.subject != WATCHED {
            // 不属于本单元的任务域。忽略，而不是硬塞进信念。
            return Ok(());
        }
        self.last_value = Some(observation.value);
        if !self.evidence_refs.contains(&observation.evidence_ref) {
            self.evidence_refs.push(observation.evidence_ref);
        }
        self.belief_revision += 1;
        Ok(())
    }

    fn propose(&self, _at: WallClock) -> Result<CandidateSet, ContractError> {
        let Some(value) = &self.last_value else {
            return Ok(CandidateSet {
                candidates: Vec::new(),
                conflicts: Vec::new(),
                unresolved: vec![Unresolved {
                    question: format!("{WATCHED} 当前是什么版本"),
                    missing: vec!["尚未收到该对象的任何观测".to_string()],
                }],
            });
        };
        Ok(CandidateSet {
            candidates: vec![Candidate::Claim {
                statement: format!("{WATCHED} 的版本是 {value}"),
                evidence_refs: self.evidence_refs.clone(),
            }],
            conflicts: Vec::new(),
            unresolved: Vec::new(),
        })
    }

    fn predict(&self, candidate: &Candidate, _at: WallClock) -> Result<Prediction, ContractError> {
        // 候选必须能落成一条**可检查**的预测。这里对"再观测一次会看到什么"下注。
        let Candidate::Claim { statement, .. } = candidate else {
            return Err(ContractError::MissingRefs {
                field: "prediction.subject",
            });
        };
        let expected = self.last_value.clone().unwrap_or_default();
        Prediction::new(
            PredictionRef::new("prediction:watch-1")?,
            WATCHED.to_string(),
            statement.clone(),
            Expectation::VersionEquals {
                subject_ref: WATCHED.to_string(),
                expected: expected.clone(),
            },
            window(0, 60),
            vec![format!("{WATCHED} 的版本不是 {expected}")],
            uncertainty_none(),
        )
    }

    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        _at: WallClock,
    ) -> Result<(), ContractError> {
        // §6 第 8 步：先比较旧预测与新观测，再更新 belief。
        // 被否定说明先前那份belief已经不可信，因此修订号必须前进。
        if outcome.verdict == Verdict::Refuted {
            self.belief_revision += 1;
            self.last_value = None;
        }
        self.last_verdict = Some(outcome.verdict);
        Ok(())
    }

    fn snapshot(&self) -> UnitSnapshot {
        UnitSnapshot {
            unit_id: self.unit_id.clone(),
            kind: self.kind(),
            schema_version: SCHEMA_VERSION,
            scope: self.scope.clone(),
            goal_refs: Vec::new(),
            belief_revision: self.belief_revision,
            belief_snapshot_ref: BlobRef::new("blob:belief-version-watcher").expect("固定对象"),
            evidence_refs: self.evidence_refs.clone(),
            relation_refs: Vec::new(),
            strategy_version: StrategyVersion::new("version-watch-v1").expect("固定策略"),
            model_profile_ref: ModelProfileRef::new("profile:none-deterministic")
                .expect("固定画像"),
            capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                .expect("固定能力策略"),
            budget_ref: BudgetRef::new("budget:task-watch").expect("固定预算"),
            pending_action_ids: Vec::new(),
            last_applied_sequence: 0,
            state: UnitState::Ready,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn observation_event(event_uuid: &str, value: &str) -> Envelope {
    let event_id = EventId::parse(event_uuid).expect("固定 UUID");
    let observation = Observation {
        subject: WATCHED.to_string(),
        value: value.to_string(),
        body_ref: None,
        evidence_ref: evidence(event_uuid),
        derived_from: Vec::new(),
        observed_by: unit("version-watcher"),
    };
    Envelope::new(
        event_id,
        SourceId::new("device:file-watcher").expect("固定来源"),
        1,
        BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
        1,
        TaskId::new("task:watch").expect("固定任务"),
        Vec::new(),
        at(0),
        Monotonic::new(
            BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
            1_000,
        ),
        Provenance::Sensor {
            adapter: SourceId::new("device:file-watcher").expect("固定适配器"),
        },
        PayloadRef::Inline {
            media_type: MediaType::new("application/json").expect("固定媒体类型"),
            body: serde_json::to_string(&observation).expect("观测可序列化"),
        },
        PermissionScope {
            capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                .expect("固定能力策略"),
            max_action_level: ActionLevel::A1,
        },
        DataClass::Personal,
        None,
        IdempotencyKey::new("idem:watch-1").expect("固定幂等键"),
    )
}

#[test]
fn a_leaf_unit_can_run_the_full_round() {
    let mut watcher = VersionWatcher::new();

    // 还没有任何观测：它应该报告"缺什么"，而不是编一个版本号出来。
    let empty = watcher.propose(at(0)).expect("提出候选");
    assert!(empty.candidates.is_empty());
    assert_eq!(empty.unresolved.len(), 1);
    assert!(empty.validate().is_ok());

    // 收到事件 → 取证 → 更新局部信念。
    watcher
        .observe(&observation_event("22222222-2222-4222-8222-222222222222", "sha256:aaa"), at(0))
        .expect("收到观测");
    assert_eq!(watcher.snapshot().belief_revision, 1);

    // 提出候选及其预期后果。
    let set = watcher.propose(at(0)).expect("提出候选");
    assert!(set.validate().is_ok());
    assert_eq!(set.candidates.len(), 1);
    assert!(!set.requests_side_effect(), "守望单元没有 OS 写权限");

    // 候选必须能落成可检查的预测。
    let prediction = watcher.predict(&set.candidates[0], at(0)).expect("生成预测");
    assert_eq!(prediction.subject, WATCHED);
    assert!(
        matches!(
            &prediction.expectation,
            Expectation::VersionEquals { expected, .. } if expected == "sha256:aaa"
        ),
        "预测必须携带机器可检查的期望，而不是只有散文"
    );
    assert!(!prediction.failure_conditions.is_empty());

    // 动作后观测与预测一致 → 支持。
    let supported = OutcomeVerified::new(
        ActionId::new("action:watch-1").expect("固定动作"),
        prediction.prediction_ref.clone(),
        Verdict::Supported,
        vec![evidence("obs-after")],
    )
    .expect("合法判定");
    watcher.handle_result(&supported, at(1)).expect("处理结果");
    assert_eq!(
        watcher.snapshot().belief_revision,
        1,
        "被支持的预测不改变修订号"
    );

    // 世界变了：新观测与旧预测不符 → 被否定 → 修订号前进且旧值作废。
    watcher
        .observe(&observation_event("33333333-3333-4333-8333-333333333333", "sha256:bbb"), at(2))
        .expect("收到新观测");
    let refuted = OutcomeVerified::new(
        ActionId::new("action:watch-2").expect("固定动作"),
        prediction.prediction_ref.clone(),
        Verdict::Refuted,
        vec![evidence("obs-after-2")],
    )
    .expect("合法判定");
    watcher.handle_result(&refuted, at(2)).expect("处理结果");

    assert_eq!(
        watcher.snapshot().belief_revision,
        3,
        "观测两次各加一，被否定再加一"
    );
    assert!(watcher.snapshot().validate().is_ok());
}

#[test]
fn a_leaf_unit_ignores_events_outside_its_task_domain() {
    // 不属于本单元任务域的事件必须被忽略，而不是被硬塞进信念。
    let mut watcher = VersionWatcher::new();
    let mut event = observation_event("44444444-4444-4444-8444-444444444444", "sha256:ccc");
    if let PayloadRef::Inline { body, .. } = &mut event.payload_ref {
        let mut observation: Observation = serde_json::from_str(body).expect("观测可解析");
        observation.subject = "file:D:\\别的目录\\other.md".to_string();
        *body = serde_json::to_string(&observation).expect("观测可序列化");
    }

    watcher.observe(&event, at(0)).expect("收到事件");
    assert_eq!(
        watcher.snapshot().belief_revision,
        0,
        "域外事件不改变信念"
    );
    assert!(watcher.snapshot().evidence_refs.is_empty());
}
