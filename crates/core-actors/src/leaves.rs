//! §4.3 首批叶单元（"桌面与文件"能力簇）。
//!
//! §4.3 那张表列了 8 簇 × 8 叶 = 64 个职责槽，但同一节紧接着写了约束：
//!
//! > 初期只实现首批有明确测试的单元，**不让尚无实际逻辑的角色充数**。
//!
//! 所以本模块只实现三个槽，每个槽的职责都能被一句话说清、也能被一个测试证伪：
//!
//! | §4.3"桌面与文件"簇的槽位 | 单元 | 它唯一负责回答的问题 |
//! |---|---|---|
//! | 文件版本 | [`FileVersion`] | 这个对象当前的版本是什么 |
//! | 动作前提 | [`ActionPrecondition`] | 这次动作声明的前提，有没有落在证据上 |
//! | 动作后验证 | [`PostconditionVerify`] | 哪次动作的预测被现实否定了 |
//! | 待推进动作 | [`PendingAction`] | 目标侧投递的这个动作，该不该进 L3 参与竞争 |
//!
//! 三者都不调用模型、不持有 OS 句柄。§3.1 明确"不要求每个单元都有 OS 写权限"——它们能做的
//! 事只有三件：收到事件、更新局部信念、提出候选。

use std::collections::{BTreeMap, BTreeSet};

use soca_contracts::{
    ActionId, ActionIntent, BlobRef, BudgetRef, Candidate, CandidateSet, CapabilityPolicyRef,
    CognitiveUnit, ContractError, DomainId, Envelope, EvidenceRef, Expectation, GoalId,
    ModelProfileRef, Observation, OutcomeVerified, PayloadRef, Prediction, PredictionRef, Scope,
    Sha256Hex, StrategyVersion, TaskContractVersion, TimeWindow, Uncertainty, UnitId, UnitKind,
    UnitSnapshot, UnitState, Unresolved, Verdict, WallClock, SCHEMA_VERSION,
};

/// 从 §7.1 公共信封中取出公开观测。
///
/// 大载荷只传引用（§10.4），而本 crate 不持有内容仓句柄。因此非内联载荷一律当作"没收到"，
/// 而不是去猜内容——§11.1 的同一条原则：拿不到就说不知道，不伪装成"没有发生"。
pub(crate) fn observation_of(event: &Envelope) -> Option<Observation> {
    let PayloadRef::Inline { body, .. } = &event.payload_ref else {
        return None;
    };
    serde_json::from_str(body).ok()
}

/// 叶单元共有的那部分状态（对应 §3.2 快照里与职责无关的那些字段）。
#[derive(Debug)]
struct LeafCore {
    unit_id: UnitId,
    scope: Scope,
    strategy_version: StrategyVersion,
    belief_revision: u64,
    evidence_refs: Vec<EvidenceRef>,
}

impl LeafCore {
    fn new(
        unit_id: &str,
        domain: &str,
        task_contract: &str,
        strategy: &str,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            unit_id: UnitId::new(unit_id)?,
            scope: Scope {
                domain: DomainId::new(domain)?,
                task_contract: TaskContractVersion::new(task_contract)?,
            },
            strategy_version: StrategyVersion::new(strategy)?,
            belief_revision: 0,
            evidence_refs: Vec::new(),
        })
    }

    /// 记下一条证据。重复证据不重复记账（§7.2 的证据去重原则）。
    fn note_evidence(&mut self, reference: &EvidenceRef) {
        if !self.evidence_refs.contains(reference) {
            self.evidence_refs.push(reference.clone());
        }
    }

    /// 局部信念发生了变化，推进修订号。
    fn note_belief_change(&mut self) {
        self.belief_revision = self.belief_revision.saturating_add(1);
    }

    fn snapshot(&self) -> UnitSnapshot {
        // 用摘要而不是直接拼单元标识：`blob:belief-` 加一个最长 256 字节的标识会超出引用
        // 长度上限，而摘要长度固定。
        let digest = Sha256Hex::of_bytes(self.unit_id.as_str().as_bytes());
        UnitSnapshot {
            unit_id: self.unit_id.clone(),
            kind: UnitKind::Leaf,
            schema_version: SCHEMA_VERSION,
            scope: self.scope.clone(),
            goal_refs: Vec::new(),
            belief_revision: self.belief_revision,
            belief_snapshot_ref: BlobRef::new(format!("blob:belief-{digest}"))
                .expect("摘要定长，必然合法"),
            evidence_refs: self.evidence_refs.clone(),
            relation_refs: Vec::new(),
            strategy_version: self.strategy_version.clone(),
            model_profile_ref: ModelProfileRef::new("profile:none-deterministic")
                .expect("固定模型画像"),
            capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                .expect("固定能力策略"),
            budget_ref: BudgetRef::new("budget:task-cluster").expect("固定预算"),
            pending_action_ids: Vec::new(),
            last_applied_sequence: 0,
            state: UnitState::Ready,
        }
    }
}

/// 构造一个只作用于某个对象、时间窗为 60 秒的预测。
fn prediction_about(
    prediction_ref: &str,
    subject_ref: &str,
    expected_change: String,
    expectation: Expectation,
    failure_condition: String,
    at: WallClock,
) -> Result<Prediction, ContractError> {
    Prediction::new(
        PredictionRef::new(prediction_ref)?,
        subject_ref.to_string(),
        expected_change,
        expectation,
        TimeWindow::new(at, at.plus_seconds(60))?,
        vec![failure_condition],
        Uncertainty {
            probability: None,
            notes: vec!["确定性观测，不需要概率字段".to_string()],
        },
    )
}

// ---------------------------------------------------------------------------
// 文件版本
// ---------------------------------------------------------------------------

/// §4.3"桌面与文件"簇的"文件版本"槽。
///
/// 它只守望一个对象，只回答一个问题。它**不**记录"文件内容是什么"——那是文档结构的槽位；
/// 它也不判断内容好不好。范围越窄，"把地图删掉行为就退化"这类判据才越容易成立。
#[derive(Debug)]
pub struct FileVersion {
    core: LeafCore,
    /// 守望的对象引用。观测与核验必须用同一个引用，否则判定会因字符串不一致而静默失败。
    watched: String,
    /// 最近一次观测到的版本。
    ///
    /// `None` 表示**尚未看见过**，绝不表示"对象不存在"。把两者混起来，单元会拿"没见过"
    /// 当"没有"，然后基于一个假事实提出候选。
    current: Option<String>,
}

impl FileVersion {
    /// 创建一个只守望给定对象的版本单元。
    pub fn new(watched: impl Into<String>) -> Result<Self, ContractError> {
        Ok(Self {
            core: LeafCore::new(
                "unit:file-version",
                "desktop-and-files",
                "file-version-v1",
                "file-version-v1",
            )?,
            watched: watched.into(),
            current: None,
        })
    }

    /// 当前已知版本。`None` 表示未知，**不是**缺席。
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }
}

impl CognitiveUnit for FileVersion {
    fn unit_id(&self) -> &UnitId {
        &self.core.unit_id
    }

    fn kind(&self) -> UnitKind {
        UnitKind::Leaf
    }

    fn observe(&mut self, event: &Envelope, _at: WallClock) -> Result<(), ContractError> {
        let Some(observation) = observation_of(event) else {
            return Ok(());
        };
        if observation.subject != self.watched {
            // 不属于本单元的任务域。忽略，而不是硬塞进局部信念——§3.2 把 scope 定义成
            // 必需字段，就是为了让这个判断有依据。
            return Ok(());
        }
        self.core.note_evidence(&observation.evidence_ref);
        self.current = Some(observation.value);
        self.core.note_belief_change();
        Ok(())
    }

    fn propose(&self, _at: WallClock) -> Result<CandidateSet, ContractError> {
        match &self.current {
            Some(value) => Ok(CandidateSet {
                candidates: vec![Candidate::Claim {
                    statement: format!("{} 的版本是 {value}", self.watched),
                    evidence_refs: self.core.evidence_refs.clone(),
                }],
                conflicts: Vec::new(),
                unresolved: Vec::new(),
            }),
            None => Ok(CandidateSet {
                // **知道自己缺什么，就把它要出来。** 只报"未决"而不提请求，会让闭环每一轮
                // 都停在"需要更多信息"上——它说得出缺哪条观测，却没有任何人去取。
                // 同一个簇里的 `ActionPrecondition` 一直是这么做的（缺前提就申请观测），
                // 这里原本漏了，表现是闭环第一轮之后再也推不动。
                candidates: vec![Candidate::RequestObservation {
                    subject_ref: self.watched.clone(),
                    reason: "尚无该对象的任何观测，无法给出结论".to_string(),
                }],
                conflicts: Vec::new(),
                unresolved: vec![Unresolved {
                    question: format!("{} 当前是什么版本", self.watched),
                    missing: vec!["尚未收到该对象的任何观测".to_string()],
                }],
            }),
        }
    }

    fn predict(&self, candidate: &Candidate, at: WallClock) -> Result<Prediction, ContractError> {
        let Candidate::Claim { statement, .. } = candidate else {
            return Err(ContractError::MissingRefs {
                field: "prediction.subject",
            });
        };
        let Some(expected) = &self.current else {
            return Err(ContractError::MissingRefs {
                field: "prediction.subject",
            });
        };
        prediction_about(
            "prediction:file-version",
            &self.watched,
            statement.clone(),
            Expectation::VersionEquals {
                subject_ref: self.watched.clone(),
                expected: expected.clone(),
            },
            format!("{} 的版本不再是 {expected}", self.watched),
            at,
        )
    }

    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        _at: WallClock,
    ) -> Result<(), ContractError> {
        for reference in &outcome.observation_refs {
            self.core.note_evidence(reference);
        }
        if outcome.verdict == Verdict::Refuted {
            // 预测被现实否定，手里的版本已经不可信。清掉它并推进修订号：下一轮会退回
            // "申请观测"，而不是继续拿一个已经被证伪的值下注。
            self.current = None;
            self.core.note_belief_change();
        }
        Ok(())
    }

    fn snapshot(&self) -> UnitSnapshot {
        self.core.snapshot()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// 动作前提
// ---------------------------------------------------------------------------

/// 一次动作声明的前提，以及能为它提供证据的对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Precondition {
    /// 前提原文，来自 `ActionIntent::preconditions`。
    pub statement: String,
    /// 必须被观测过、才能算这条前提成立的对象引用。
    pub subject_ref: String,
}

impl Precondition {
    /// 构造一条前提。
    pub fn new(statement: impl Into<String>, subject_ref: impl Into<String>) -> Self {
        Self {
            statement: statement.into(),
            subject_ref: subject_ref.into(),
        }
    }
}

/// §4.3"桌面与文件"簇的"动作前提"槽。
///
/// 它只回答一个问题：这次动作声明的前提，**有没有落在证据上**。
///
/// 三种可能的结果被严格区分，不允许压缩成两种：
///
/// * 前提有证据 → 本次它不提出任何候选（它不是执行者，安静是它的正确行为）；
/// * 前提没有证据 → 提出 `RequestObservation`，去把证据拿回来；
/// * 它什么都没见过 → 对所有前提都提出 `RequestObservation`。
///
/// 关键的是**不存在**第四条路："没有证据 → 当成前提成立，照常动作"。§12.2 要求执行许可绑定
/// 具体参数与版本，正是为了让这一步的失败无法被跳到下一步去。
#[derive(Debug)]
pub struct ActionPrecondition {
    core: LeafCore,
    preconditions: Vec<Precondition>,
    /// 已被观测证据确认成立的前提原文。
    confirmed: BTreeSet<String>,
}

impl ActionPrecondition {
    /// 创建一个核验给定前提的单元。
    pub fn new(preconditions: Vec<Precondition>) -> Result<Self, ContractError> {
        Ok(Self {
            core: LeafCore::new(
                "unit:action-precondition",
                "desktop-and-files",
                "action-precondition-v1",
                "action-precondition-v1",
            )?,
            preconditions,
            confirmed: BTreeSet::new(),
        })
    }

    /// 已被确认成立的前提。
    pub fn confirmed(&self) -> &BTreeSet<String> {
        &self.confirmed
    }
}

impl CognitiveUnit for ActionPrecondition {
    fn unit_id(&self) -> &UnitId {
        &self.core.unit_id
    }

    fn kind(&self) -> UnitKind {
        UnitKind::Leaf
    }

    fn observe(&mut self, event: &Envelope, _at: WallClock) -> Result<(), ContractError> {
        let Some(observation) = observation_of(event) else {
            return Ok(());
        };
        let mut changed = false;
        for precondition in &self.preconditions {
            if precondition.subject_ref == observation.subject
                && self.confirmed.insert(precondition.statement.clone())
            {
                self.core.note_evidence(&observation.evidence_ref);
                changed = true;
            }
        }
        if changed {
            self.core.note_belief_change();
        }
        Ok(())
    }

    fn propose(&self, _at: WallClock) -> Result<CandidateSet, ContractError> {
        let mut set = CandidateSet::empty();
        for precondition in &self.preconditions {
            if self.confirmed.contains(&precondition.statement) {
                continue;
            }
            set.candidates.push(Candidate::RequestObservation {
                subject_ref: precondition.subject_ref.clone(),
                reason: format!("前提「{}」尚无证据，不能当作成立", precondition.statement),
            });
        }
        if !set.candidates.is_empty() {
            set.unresolved.push(Unresolved {
                question: "本次动作的前提是否全部成立".to_string(),
                missing: set
                    .candidates
                    .iter()
                    .filter_map(|candidate| match candidate {
                        Candidate::RequestObservation { subject_ref, .. } => Some(subject_ref.clone()),
                        _ => None,
                    })
                    .collect(),
            });
        }
        Ok(set)
    }

    fn predict(&self, candidate: &Candidate, at: WallClock) -> Result<Prediction, ContractError> {
        // 申请观测时还不知道会看到什么值，但可以押注"这个对象确实存在"。押错（观测到缺席）
        // 会变成 Refuted，这仍然是有用信息：它说明找错了对象。
        let Candidate::RequestObservation { subject_ref, .. } = candidate else {
            return Err(ContractError::MissingRefs {
                field: "prediction.subject",
            });
        };
        prediction_about(
            "prediction:action-precondition",
            subject_ref,
            format!("{subject_ref} 能被观测到"),
            Expectation::Present {
                subject_ref: subject_ref.clone(),
            },
            format!("{subject_ref} 观测不到，前提无法核验"),
            at,
        )
    }

    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        _at: WallClock,
    ) -> Result<(), ContractError> {
        for reference in &outcome.observation_refs {
            self.core.note_evidence(reference);
        }
        if outcome.verdict == Verdict::Refuted {
            self.core.note_belief_change();
        }
        Ok(())
    }

    fn snapshot(&self) -> UnitSnapshot {
        self.core.snapshot()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// 动作后验证
// ---------------------------------------------------------------------------

/// §4.3"桌面与文件"簇的"动作后验证"槽。
///
/// 它维护一条**证据到对象**的映射，因为 §7.2 的判定结果只给观测引用，不给对象。没有这张
/// 映射，单元在收到"预测被否定"时就无法说出被否定的是哪个对象，只能报告一个数字。
///
/// 它保证一件事：**被现实否定过的对象，不会被说成成功。**
#[derive(Debug)]
pub struct PostconditionVerify {
    core: LeafCore,
    /// 证据引用 → 该证据描述的对象。
    seen: BTreeMap<EvidenceRef, String>,
    /// 被现实否定过、尚待重新观测的对象。
    refuted_subjects: BTreeSet<String>,
    /// 已被现实支持的动作。
    supported_actions: BTreeSet<ActionId>,
}

impl PostconditionVerify {
    /// 创建一个动作后验证单元。
    pub fn new() -> Result<Self, ContractError> {
        Ok(Self {
            core: LeafCore::new(
                "unit:postcondition-verify",
                "desktop-and-files",
                "postcondition-verify-v1",
                "postcondition-verify-v1",
            )?,
            seen: BTreeMap::new(),
            refuted_subjects: BTreeSet::new(),
            supported_actions: BTreeSet::new(),
        })
    }

    /// 被现实否定、尚待重新观测的对象。
    pub fn refuted_subjects(&self) -> &BTreeSet<String> {
        &self.refuted_subjects
    }

    /// 已被现实支持的动作数。
    pub fn supported_count(&self) -> usize {
        self.supported_actions.len()
    }
}

impl CognitiveUnit for PostconditionVerify {
    fn unit_id(&self) -> &UnitId {
        &self.core.unit_id
    }

    fn kind(&self) -> UnitKind {
        UnitKind::Leaf
    }

    fn observe(&mut self, event: &Envelope, _at: WallClock) -> Result<(), ContractError> {
        let Some(observation) = observation_of(event) else {
            return Ok(());
        };
        // 只记映射，不改变判断。这张映射的作用是让 handle_result 能说出对象名。
        self.seen
            .insert(observation.evidence_ref, observation.subject);
        Ok(())
    }

    fn propose(&self, _at: WallClock) -> Result<CandidateSet, ContractError> {
        let mut set = CandidateSet::empty();
        for subject_ref in &self.refuted_subjects {
            // §7.3 明令禁止重复副作用。修正的办法是重新观测，不是把动作再执行一遍。
            set.candidates.push(Candidate::RequestObservation {
                subject_ref: subject_ref.clone(),
                reason: "该对象上的上一次预测被现实否定，需要重新观测才能重新判定".to_string(),
            });
        }
        if !self.refuted_subjects.is_empty() {
            set.unresolved.push(Unresolved {
                question: format!(
                    "{} 个对象上的预测被现实否定，如何处置",
                    self.refuted_subjects.len()
                ),
                missing: vec!["需要一次新的观测，或一次修正后的动作".to_string()],
            });
        }
        Ok(set)
    }

    fn predict(&self, candidate: &Candidate, at: WallClock) -> Result<Prediction, ContractError> {
        let Candidate::RequestObservation { subject_ref, .. } = candidate else {
            return Err(ContractError::MissingRefs {
                field: "prediction.subject",
            });
        };
        prediction_about(
            "prediction:postcondition-verify",
            subject_ref,
            format!("{subject_ref} 能被重新观测到"),
            Expectation::Present {
                subject_ref: subject_ref.clone(),
            },
            format!("{subject_ref} 观测不到，无法重新判定"),
            at,
        )
    }

    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        _at: WallClock,
    ) -> Result<(), ContractError> {
        match outcome.verdict {
            Verdict::Supported => {
                self.supported_actions.insert(outcome.action_id.clone());
                // 重新观测成功，之前挂起的对象可以从待办里去掉。
                for reference in &outcome.observation_refs {
                    if let Some(subject) = self.seen.get(reference) {
                        self.refuted_subjects.remove(subject);
                    }
                }
            }
            Verdict::Refuted => {
                // 判定结果只说"哪些观测支撑了它"，不说对象。靠 observe 阶段攒下的映射
                // 把它翻译回对象名；翻译不出来的就明确留空，不猜。
                for reference in &outcome.observation_refs {
                    if let Some(subject) = self.seen.get(reference) {
                        self.refuted_subjects.insert(subject.clone());
                    }
                }
            }
            Verdict::Inconclusive => {}
        }
        for reference in &outcome.observation_refs {
            self.core.note_evidence(reference);
        }
        self.core.note_belief_change();
        Ok(())
    }

    fn snapshot(&self) -> UnitSnapshot {
        self.core.snapshot()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// 待推进的动作
// ---------------------------------------------------------------------------

/// 一个待推进的动作，连同它预期的后果与它所属的目标。
#[derive(Debug)]
struct QueuedAction {
    /// 提出这次动作的目标。
    ///
    /// 没有它的话，一个为 A 目标投递的写入会在 B 目标下被执行——因为权限范围是按**当前**
    /// 目标核对的，而不是按提出它的那个目标。两者都能通过检查，但后者是错的：B 这个任务
    /// 不该执行 A 的动作，而用户放弃 A 的意图更不该在别处生效。
    goal_ref: GoalId,
    intent: ActionIntent,
    expectation: Expectation,
}

/// §4.3"推理与计划／候选方案"槽位的最小替身。
///
/// **先说清它是什么，以及它不是什么。** 它**不是**计划单元：真正的候选方案由计划单元从目标
/// 与局部信念里生成，而那需要问题分解、模拟与代价预测（§4.3 的另三个槽），目前都还没有。
/// 它做的是一件窄得多的事——把目标侧已经决定要做的动作**提交给 L3**，让它与其它候选一起
/// 竞争，并且把它预期的后果如实带上来。
///
/// 为什么不等到计划单元写好再一起做：动作类候选在这之前根本没有入口。§6 第 5–8 步
/// （选择、许可、执行、后置条件核对）整条链虽然都实现了、也各自有测试，却从来没有被真实
/// 数据走过一遍。**一个没被跑过的路径和一个不存在的路径，在故障面前没有区别。**
///
/// 它也**不**自己编造动作、不自己放宽任何东西：投递进来的意图要依次过 L2 的证据存在性校验、
/// L3 的候选竞争、以及策略代理的许可判定三道关。
#[derive(Debug)]
pub struct PendingAction {
    core: LeafCore,
    queued: Vec<QueuedAction>,
}

impl PendingAction {
    /// 构造。
    pub fn new() -> Result<Self, ContractError> {
        Ok(Self {
            core: LeafCore::new(
                "unit:leaf:pending-action",
                "desktop-and-files",
                "file-write-v1",
                "pending-action-v1",
            )?,
            queued: Vec::new(),
        })
    }

    /// 投递一个待推进的动作。
    ///
    /// 意图与期望必须一起给出。§6 第 3 步要求"单元读取授权证据，在**动作前**记录可检查的
    /// 预测"——一个不说明自己期望什么就请求动作的单元，事后无法判断自己是否判断错了，而
    /// §6 第 8 步的"先比较旧预测与新观测"就成了无米之炊。
    ///
    /// 同一个动作重复投递是幂等的，**不**用后来的期望覆盖先到的：动作标识一旦使用就代表
    /// 那一次具体动作，改期望说明改的其实是另一次动作，那应该用一个新的标识。
    pub fn queue(
        &mut self,
        goal_ref: GoalId,
        intent: ActionIntent,
        expectation: Expectation,
    ) -> Result<(), ContractError> {
        if self
            .queued
            .iter()
            .any(|queued| queued.intent.action_id == intent.action_id)
        {
            return Ok(());
        }
        self.queued.push(QueuedAction {
            goal_ref,
            intent,
            expectation,
        });
        self.core.note_belief_change();
        Ok(())
    }

    /// 丢掉某个目标名下尚未推进的动作。返回丢掉几个。
    ///
    /// §6 第 9 步："结束后能力簇解散临时队伍，单元转温/冷态，**计划外动作不继续后台执行**。"
    /// 目标结束（达成或放弃）之后，它名下那些还没来得及做的动作就不该再等了——它们的存在
    /// 理由是那个目标，而那个目标已经不在了。
    pub fn release_goal(&mut self, goal_ref: &GoalId) -> usize {
        let before = self.queued.len();
        self.queued.retain(|queued| &queued.goal_ref != goal_ref);
        let dropped = before - self.queued.len();
        if dropped > 0 {
            self.core.note_belief_change();
        }
        dropped
    }

    /// 某个目标名下还有几个待推进的动作。
    pub fn pending_for(&self, goal_ref: &GoalId) -> usize {
        self.queued
            .iter()
            .filter(|queued| &queued.goal_ref == goal_ref)
            .count()
    }

    /// 还有几个待推进。
    pub fn pending(&self) -> usize {
        self.queued.len()
    }

    /// 尚未推进的动作标识。
    pub fn pending_ids(&self) -> Vec<String> {
        self.queued
            .iter()
            .map(|queued| queued.intent.action_id.to_string())
            .collect()
    }

    /// 取走一个动作（推进完成之后由 [`CognitiveUnit::handle_result`] 调用）。
    pub fn release(&mut self, action_id: &ActionId) -> bool {
        let before = self.queued.len();
        self.queued
            .retain(|queued| queued.intent.action_id != *action_id);
        let removed = self.queued.len() != before;
        if removed {
            self.core.note_belief_change();
        }
        removed
    }
}

impl CognitiveUnit for PendingAction {
    fn unit_id(&self) -> &UnitId {
        &self.core.unit_id
    }

    fn kind(&self) -> UnitKind {
        UnitKind::Leaf
    }

    /// 动作意图不是从事件里来的。收到事件时本单元不做任何事——把它硬塞进"待推进"，
    /// 等于让任何一条消息都能提议一次副作用。
    fn observe(&mut self, _event: &Envelope, _at: WallClock) -> Result<(), ContractError> {
        Ok(())
    }

    fn propose(&self, _at: WallClock) -> Result<CandidateSet, ContractError> {
        let mut set = CandidateSet::empty();
        for queued in &self.queued {
            set.candidates.push(Candidate::RequestAction {
                intent: Box::new(queued.intent.clone()),
            });
        }
        Ok(set)
    }

    fn predict(&self, candidate: &Candidate, at: WallClock) -> Result<Prediction, ContractError> {
        let Candidate::RequestAction { intent } = candidate else {
            return Err(ContractError::MissingRefs {
                field: "prediction.subject",
            });
        };
        let queued = self
            .queued
            .iter()
            .find(|queued| queued.intent.action_id == intent.action_id)
            .ok_or(ContractError::MissingRefs {
                field: "prediction.subject",
            })?;

        // 预测引用取自意图本身，而不是在这里新起一个：§6 第 3 步要求"动作前记录可检查的
        // 预测"，而许可与受理都要靠这个引用把"当初预期什么"找回来。另起一个引用，
        // 会让意图指向一份预测、账上记着另一份。
        prediction_about(
            intent.prediction_ref.as_str(),
            intent.object_scope.as_str(),
            format!("{} 对 {} 执行后应满足预期", intent.tool_id, intent.object_scope),
            queued.expectation.clone(),
            "执行后目标对象没有变成预期的样子".to_string(),
            at,
        )
    }

    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        _at: WallClock,
    ) -> Result<(), ContractError> {
        // 三种判定都算走过了一轮：支持说明做对了，否定说明判断错了（同簇的
        // [`PostconditionVerify`] 会据此要求重新观测），inconclusive 说明受了外部影响。
        // 三种情况下这个动作都完成了一次闭环，继续挂着它只会让 L3 每一轮都重新提议它。
        self.release(&outcome.action_id);
        for reference in &outcome.observation_refs {
            self.core.note_evidence(reference);
        }
        Ok(())
    }

    fn snapshot(&self) -> UnitSnapshot {
        self.core.snapshot()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
