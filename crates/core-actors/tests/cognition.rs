//! §4.3 首批叶单元与 §4.2 能力簇的回归测试。
//!
//! 每个测试针对一条能被证伪的判据，而不是"看起来跑通了"。最要紧的两条：
//!
//! * **证据不足时提出的是观测请求，不是猜出来的结论**（§3.1 的"已知/未知必须分开"）；
//! * **被现实否定过的东西不会被说成成功**（§6 第 8 步）。

use soca_contracts::{
    ActionId, ActionLevel, BlobRef, BootId, Candidate, CapabilityPolicyRef, CognitiveUnit,
    DataClass, Envelope, EventId, EvidenceRef, Expectation, IdempotencyKey, MediaType, Monotonic,
    Observation, OutcomeVerified, PayloadRef, PermissionScope, PredictionRef, Provenance,
    Sha256Hex, SourceId, TaskId, UnitId, UnitKind, Verdict, WallClock, Workspace,
};
use soca_core_actors::{
    ActionPrecondition, DesktopAndFilesCluster, FileVersion, PostconditionVerify, Precondition,
};

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";
const CAPABILITY: &str = "cap:read-selected-folder";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn boot() -> BootId {
    BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot")
}

fn evidence(name: &str) -> EvidenceRef {
    EvidenceRef::new(format!("obs:{name}")).expect("固定证据")
}

fn observation(subject: &str, value: &str, evidence_name: &str) -> Observation {
    Observation {
        subject: subject.to_string(),
        value: value.to_string(),
        evidence_ref: evidence(evidence_name),
        derived_from: Vec::new(),
        observed_by: UnitId::new("unit:test").expect("固定单元"),
    }
}

fn event_of(observation: &Observation, uuid: &str) -> Envelope {
    Envelope::new(
        EventId::parse(uuid).expect("固定 UUID"),
        SourceId::new("device:file-watcher").expect("固定来源"),
        1,
        boot(),
        1,
        TaskId::new("task:write").expect("固定任务"),
        Vec::new(),
        at(0),
        Monotonic::new(boot(), 1_000),
        Provenance::Sensor {
            adapter: SourceId::new("device:file-watcher").expect("固定适配器"),
        },
        PayloadRef::Inline {
            media_type: MediaType::new("application/json").expect("固定媒体类型"),
            body: serde_json::to_string(observation).expect("观测可序列化"),
        },
        PermissionScope {
            capability_policy_ref: CapabilityPolicyRef::new(CAPABILITY).expect("固定能力策略"),
            max_action_level: ActionLevel::A1,
        },
        DataClass::Personal,
        None,
        IdempotencyKey::new("idem:cognition-1").expect("固定幂等键"),
    )
}

const EVENT_A: &str = "22222222-2222-4222-8222-222222222222";
const EVENT_B: &str = "33333333-3333-4333-8333-333333333333";

fn outcome(action: &str, verdict: Verdict, observation_refs: Vec<EvidenceRef>) -> OutcomeVerified {
    OutcomeVerified::new(
        ActionId::new(action).expect("固定动作"),
        PredictionRef::new("prediction:test").expect("固定预测引用"),
        verdict,
        observation_refs,
    )
    .expect("合法判定")
}

// ---------------------------------------------------------------------------
// 文件版本
// ---------------------------------------------------------------------------

#[test]
fn file_version_does_not_guess_before_it_observes() {
    let watcher = FileVersion::new(WATCHED).expect("构造");
    let set = watcher.propose(at(0)).expect("提出候选");

    assert!(set.candidates.is_empty(), "没看见过就不下结论");
    assert_eq!(set.unresolved.len(), 1);
    assert!(
        set.unresolved[0].missing[0].contains("尚未收到"),
        "未决条件必须写明缺的是观测"
    );
    assert_eq!(watcher.current(), None);
}

#[test]
fn file_version_ignores_events_about_other_objects() {
    // §3.2 把 scope 定义成必需字段，就是为了让这个判断有依据。
    let mut watcher = FileVersion::new(WATCHED).expect("构造");
    let other = observation("file:D:\\别的目录\\other.md", "sha256:zzz", "other");
    watcher
        .observe(&event_of(&other, EVENT_A), at(0))
        .expect("收到事件");

    assert_eq!(watcher.current(), None);
    assert!(
        watcher.snapshot().evidence_refs.is_empty(),
        "域外事件不产生证据"
    );
    assert_eq!(watcher.snapshot().belief_revision, 0);
}

#[test]
fn file_version_claims_what_it_saw_and_can_prove_it() {
    let mut watcher = FileVersion::new(WATCHED).expect("构造");
    let seen = observation(WATCHED, "sha256:aaa", "1");
    watcher
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");

    assert_eq!(watcher.current(), Some("sha256:aaa"));
    let set = watcher.propose(at(0)).expect("提出候选");
    set.validate().expect("候选必须自带证据");

    let prediction = watcher.predict(&set.candidates[0], at(0)).expect("生成预测");
    assert_eq!(prediction.subject, WATCHED);
    assert!(
        matches!(
            &prediction.expectation,
            Expectation::VersionEquals { expected, .. } if expected == "sha256:aaa"
        ),
        "预测必须携带机器可检查的期望"
    );
    assert!(!prediction.failure_conditions.is_empty());
}

#[test]
fn file_version_drops_a_refuted_version() {
    let mut watcher = FileVersion::new(WATCHED).expect("构造");
    let seen = observation(WATCHED, "sha256:aaa", "1");
    watcher
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");
    assert_eq!(watcher.snapshot().belief_revision, 1);

    let refuted = outcome("action:1", Verdict::Refuted, vec![evidence("after")]);
    watcher.handle_result(&refuted, at(1)).expect("处理结果");

    assert_eq!(
        watcher.current(),
        None,
        "被否定的版本不能再拿来说事"
    );
    assert_eq!(
        watcher.snapshot().belief_revision,
        2,
        "被否定必须推进修订号"
    );
    let set = watcher.propose(at(1)).expect("提出候选");
    assert!(
        set.candidates.is_empty(),
        "退回未知状态，而不是继续用旧值"
    );
}

// ---------------------------------------------------------------------------
// 动作前提
// ---------------------------------------------------------------------------

#[test]
fn action_precondition_asks_for_evidence_instead_of_assuming() {
    // 这条测试是"动作前提"槽存在的理由。没有证据时它绝不提出动作——
    // 若它默认前提成立，§12.2 的参数绑定就成了唯一一道闸，而那道闸在动作已经构造好之后才关。
    let checker =
        ActionPrecondition::new(vec![Precondition::new("目标目录已授权", CAPABILITY)]).expect("构造");

    let set = checker.propose(at(0)).expect("提出候选");
    assert_eq!(set.candidates.len(), 1);
    assert!(matches!(
        &set.candidates[0],
        Candidate::RequestObservation { subject_ref, .. } if subject_ref == CAPABILITY
    ));
    assert!(!set.requests_side_effect(), "拿不到证据时绝不提出动作");
    assert_eq!(set.unresolved.len(), 1);
}

#[test]
fn action_precondition_goes_quiet_once_it_has_evidence() {
    let mut checker =
        ActionPrecondition::new(vec![Precondition::new("目标目录已授权", CAPABILITY)]).expect("构造");
    let granted = observation(CAPABILITY, "granted", "1");
    checker
        .observe(&event_of(&granted, EVENT_A), at(0))
        .expect("收到观测");

    assert!(checker.confirmed().contains("目标目录已授权"));
    let set = checker.propose(at(0)).expect("提出候选");
    assert!(set.is_empty(), "前提已确认时它不该再说什么");
}

#[test]
fn action_precondition_predicts_existence_not_a_value() {
    let checker =
        ActionPrecondition::new(vec![Precondition::new("目标目录已授权", CAPABILITY)]).expect("构造");
    let set = checker.propose(at(0)).expect("提出候选");
    let prediction = checker.predict(&set.candidates[0], at(0)).expect("生成预测");

    // 申请观测时还不知道会看到什么值，但可以押注"这个对象确实存在"。
    assert_eq!(prediction.subject, CAPABILITY);
    assert!(matches!(
        &prediction.expectation,
        Expectation::Present { subject_ref } if subject_ref == CAPABILITY
    ));
}

// ---------------------------------------------------------------------------
// 动作后验证
// ---------------------------------------------------------------------------

#[test]
fn postcondition_verify_translates_evidence_back_to_subjects() {
    let mut verify = PostconditionVerify::new().expect("构造");
    let seen = observation(WATCHED, "sha256:bbb", "after-1");
    verify
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");

    // §7.2 的判定结果只给观测引用，不给对象。靠 observe 阶段攒下的映射翻译回来。
    let refuted = outcome("action:1", Verdict::Refuted, vec![evidence("after-1")]);
    verify.handle_result(&refuted, at(1)).expect("处理结果");

    assert!(verify.refuted_subjects().contains(WATCHED));
    let set = verify.propose(at(1)).expect("提出候选");
    set.validate().expect("候选必须自带证据");
    assert!(matches!(
        &set.candidates[0],
        Candidate::RequestObservation { subject_ref, .. } if subject_ref == WATCHED
    ));
    assert!(
        !set.requests_side_effect(),
        "修正办法是重新观测，不是把动作再做一遍（§7.3）"
    );
    assert_eq!(set.unresolved.len(), 1);
}

#[test]
fn postcondition_verify_does_not_invent_a_subject_it_never_saw() {
    // 翻译不出来的观测引用就留空，不猜对象名。猜出来的对象会导致"重新观测一个不存在的
    // 东西"，而那比报告未知更糟——它把未知伪装成了已知。
    let mut verify = PostconditionVerify::new().expect("构造");
    let refuted = outcome("action:1", Verdict::Refuted, vec![evidence("never-seen")]);
    verify.handle_result(&refuted, at(1)).expect("处理结果");

    assert!(verify.refuted_subjects().is_empty());
    assert!(verify.propose(at(1)).expect("提出候选").is_empty());
}

#[test]
fn postcondition_verify_clears_a_subject_once_it_is_supported() {
    let mut verify = PostconditionVerify::new().expect("构造");
    let seen = observation(WATCHED, "sha256:bbb", "after-1");
    verify
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");

    verify
        .handle_result(
            &outcome("action:1", Verdict::Refuted, vec![evidence("after-1")]),
            at(1),
        )
        .expect("处理结果");
    assert!(verify.refuted_subjects().contains(WATCHED));

    verify
        .handle_result(
            &outcome("action:2", Verdict::Supported, vec![evidence("after-1")]),
            at(2),
        )
        .expect("处理结果");
    assert!(
        !verify.refuted_subjects().contains(WATCHED),
        "重新观测成功之后，待办要清掉；否则未决条件会永远累积"
    );
    assert_eq!(verify.supported_count(), 1);
}

// ---------------------------------------------------------------------------
// 能力簇
// ---------------------------------------------------------------------------

fn cluster() -> DesktopAndFilesCluster {
    DesktopAndFilesCluster::new(
        WATCHED,
        vec![Precondition::new("目标目录已授权", CAPABILITY)],
    )
    .expect("装配能力簇")
}

#[test]
fn a_cluster_is_a_cognitive_unit_like_a_leaf() {
    // §4.2："复合单元对外暴露同一 observe/propose/predict/handle_result/snapshot 合同"。
    // 这里断言的是簇的合同产物与叶单元同形——同一个 trait、同一份快照校验。
    let cluster = cluster();
    assert_eq!(cluster.kind(), UnitKind::Cluster);
    assert_eq!(cluster.leaf_count(), 3);

    let snapshot = cluster.snapshot();
    assert_eq!(snapshot.kind, UnitKind::Cluster);
    snapshot.validate().expect("簇快照必须自洽");
    assert_eq!(cluster.leaf_ids().len(), 3);
}

#[test]
fn a_cluster_asks_for_observations_before_it_has_evidence() {
    let cluster = cluster();
    let set = cluster.propose(at(0)).expect("提出候选");
    set.validate().expect("结构合法");

    assert!(!set.requests_side_effect(), "证据不足时绝不提出动作");
    assert!(
        set.candidates
            .iter()
            .all(|candidate| matches!(candidate, Candidate::RequestObservation { .. })),
        "此刻簇能提出的只有观测请求"
    );
    assert!(
        !set.unresolved.is_empty(),
        "未决条件必须被保留，不能在合并时被丢掉"
    );
}

#[test]
fn a_cluster_passes_the_l2_gate_once_evidence_is_on_the_board() {
    let mut cluster = cluster();
    let seen = observation(WATCHED, "sha256:aaa", "1");
    cluster
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");

    // 观测先成为 L2 上的一条发现，再交给子单元——顺序反了就会撞上自己的门。
    assert!(cluster.workspace().has_evidence(&evidence("1")));
    assert_eq!(cluster.workspace().topic_count(), 1);
    assert_eq!(cluster.ingested(), 1);

    let set = cluster.propose(at(0)).expect("L2 门必须放行正常路径");
    set.validate().expect("结构合法");
    assert!(
        set.candidates
            .iter()
            .any(|candidate| matches!(candidate, Candidate::Claim { .. })),
        "有了证据，文件版本单元应当给出带证据的结论"
    );
}

#[test]
fn a_cluster_predicts_through_the_child_that_proposed_it() {
    // 簇不自己编预测。预测引用指向哪个子单元，就说明它是谁下的注。
    let cluster = cluster();
    let set = cluster.propose(at(0)).expect("提出候选");
    let request = set
        .candidates
        .iter()
        .find(|candidate| matches!(candidate, Candidate::RequestObservation { .. }))
        .expect("此刻应当只有观测请求");

    let prediction = cluster.predict(request, at(0)).expect("生成预测");
    assert_eq!(
        prediction.prediction_ref.to_string(),
        "prediction:action-precondition",
        "只有动作前提单元会为观测请求下注，因此引用必然来自它"
    );
}

#[test]
fn a_clusters_snapshot_is_the_union_of_its_children() {
    // §4.2：父单元输出包含"子结果、关系索引、冲突、未决条件和证据引用"，
    // **不能只拼接子摘要**。所以证据是并集，而不是一条概括。
    let mut cluster = cluster();
    let seen = observation(WATCHED, "sha256:aaa", "1");
    cluster
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");
    cluster
        .handle_result(
            &outcome("action:1", Verdict::Supported, vec![evidence("after-1")]),
            at(1),
        )
        .expect("处理结果");

    let snapshot = cluster.snapshot();
    snapshot.validate().expect("并集不能有重复引用");
    assert!(
        snapshot.evidence_refs.contains(&evidence("1")),
        "来自观测的证据要在"
    );
    assert!(
        snapshot.evidence_refs.contains(&evidence("after-1")),
        "来自判定的证据也要在——它只进过动作后验证单元，没进过黑板以外的任何地方"
    );
    assert!(snapshot.belief_revision > 0);
}

#[test]
fn a_cluster_workspace_is_read_only_from_outside() {
    // §4.1："证据不能被执行层改写"。这不是靠约定，是靠没有那个方法：
    // `workspace()` 只借出只读引用，簇之外不存在拿到可变借用的入口。
    let mut cluster = cluster();
    let seen = observation(WATCHED, "sha256:aaa", "1");
    cluster
        .observe(&event_of(&seen, EVENT_A), at(0))
        .expect("收到观测");

    let workspace: &Workspace = cluster.workspace();
    assert_eq!(workspace.evidence().len(), 1);
    assert_eq!(workspace.posts(), 1);
    assert_eq!(workspace.entry(WATCHED).expect("主题存在").revision, 1);
}

#[test]
fn a_cluster_ignores_a_blob_payload_it_cannot_read() {
    // 大载荷只传引用（§10.4），而本 crate 不持有内容仓句柄。读不懂就当没收到，
    // 但**也不能**因此把"没收到"当成"那里什么都没有"。
    let mut cluster = cluster();
    let mut event = event_of(&observation(WATCHED, "sha256:aaa", "1"), EVENT_A);
    event.payload_ref = PayloadRef::Blob {
        blob_ref: BlobRef::new("blob:obs-1").expect("固定对象"),
        media_type: MediaType::new("application/json").expect("固定媒体类型"),
        bytes: 512,
        sha256: Sha256Hex::of_bytes(b"unreadable"),
    };

    cluster.observe(&event, at(0)).expect("收到事件");
    assert!(cluster.workspace().is_empty(), "读不懂的载荷不进黑板");
    assert_eq!(cluster.ingested(), 0);

    let set = cluster.propose(at(0)).expect("提出候选");
    assert!(
        !set.unresolved.is_empty(),
        "仍然承认自己不知道，而不是当作已经知道"
    );
}

#[test]
fn a_cluster_keeps_the_whole_round_consistent_across_several_events() {
    // 多轮之后：黑板上有两条发现、一条判定；候选集合仍然过 L2 门；快照自洽。
    let mut cluster = cluster();
    for (index, (subject, value)) in [
        (WATCHED, "sha256:aaa"),
        (CAPABILITY, "granted"),
    ]
    .into_iter()
    .enumerate()
    {
        let seen = observation(subject, value, &format!("{}", index + 1));
        let uuid = if index == 0 { EVENT_A } else { EVENT_B };
        cluster
            .observe(&event_of(&seen, uuid), at(index as i64))
            .expect("收到观测");
    }

    assert_eq!(cluster.workspace().topic_count(), 2);
    assert_eq!(cluster.workspace().evidence().len(), 2);

    let set = cluster.propose(at(0)).expect("L2 门放行");
    set.validate().expect("结构合法");
    assert!(
        !set.requests_side_effect(),
        "三个槽位都不持有 OS 写权限（§3.1）"
    );

    let snapshot = cluster.snapshot();
    snapshot.validate().expect("簇快照自洽");
    assert_eq!(snapshot.evidence_refs.len(), 2);
}
