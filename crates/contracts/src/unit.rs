//! 最小认知单元：一轮闭环的合同（§3.1）与快照、生命周期（§3.2、§9.1、§9.2）。
//!
//! 三组约束在这里变成类型、状态机和 trait：
//!
//! 1. **一轮闭环的形状**（§3.1）。`observe → propose → predict → handle_result → snapshot`
//!    就是 §3.1 那串"收到事件→取证与更新局部信念→提出候选及其预期后果→请求允许的动作→
//!    等待新结果→比较预测与观测→更新可持久化状态"的类型化形式。§4.2 要求**复合单元对外
//!    暴露同一份合同**，所以簇和主体实现的是同一个 trait，不是各自另立一套。
//! 2. §3.2 的快照中不存裸内存指针、活连接句柄、密钥、GPU 指针或整个模型权重。做法是
//!    让每个字段都是带前缀校验的引用类型，结构上就没有能放句柄的位置。
//! 3. "存在不明副作用时，不能靠卸载单元来解决它"（§9.2）。做法是进入 `COLD` 之前必须
//!    先把未决动作移交给在线动作账，否则迁移被拒绝。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::validate::assert_unique;
use crate::{
    ActionId, ActionIntent, BlobRef, BudgetRef, CandidateKind, CapabilityPolicyRef, ContractError,
    DomainId, Envelope, EvidenceRef, GoalId, ModelProfileRef, OutcomeVerified, Prediction,
    RelationRef, StrategyVersion, TaskContractVersion, ToolId, UnitId, WallClock, SCHEMA_VERSION,
};

/// 单元种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// 叶单元：执行具体任务。
    Leaf,
    /// 能力簇：统一对外任务合同，协调若干叶单元。
    Cluster,
    /// 完整主体内核。
    Subject,
}

/// 单元的适用任务域（§3.2）。
///
/// §3 的研究结论是"状态充分性依赖允许的未来任务"：一个不知道自己在哪个任务域里的单元，
/// 其状态充分性无法判定。所以 scope 是必需字段而不是元数据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// 领域名，例如 `document-summary`。
    pub domain: DomainId,
    /// 任务合同版本，例如 `summary-v1`。
    pub task_contract: TaskContractVersion,
}

/// 单元生命周期状态（§9.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UnitState {
    /// 只在注册表和持久邮箱中存在，不保留活线程。
    Cold,
    /// 正在加载快照、迁移版本、重验权限。
    Loading,
    /// 已就绪，可接受租约。
    Ready,
    /// 正在执行一轮闭环。
    Running,
    /// 等待新结果（动作回执、模型返回、外部观测）。
    Waiting,
    /// 正在提交未决动作、游标、状态与 outbox 事务。
    Checkpointing,
    /// 出错了。
    Faulted,
    /// 被隔离，需要人工处理。
    Quarantined,
}

impl UnitState {
    /// 全部状态。
    pub const ALL: [Self; 8] = [
        Self::Cold,
        Self::Loading,
        Self::Ready,
        Self::Running,
        Self::Waiting,
        Self::Checkpointing,
        Self::Faulted,
        Self::Quarantined,
    ];

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "COLD",
            Self::Loading => "LOADING",
            Self::Ready => "READY",
            Self::Running => "RUNNING",
            Self::Waiting => "WAITING",
            Self::Checkpointing => "CHECKPOINTING",
            Self::Faulted => "FAULTED",
            Self::Quarantined => "QUARANTINED",
        }
    }

    /// 是否处于热态（占用 RAM 与可能占用模型会话）。
    pub fn is_hot(self) -> bool {
        matches!(
            self,
            Self::Loading | Self::Ready | Self::Running | Self::Waiting | Self::Checkpointing
        )
    }

    /// 是否允许迁移到 `next`。
    ///
    /// 依据 §9.2 的状态图。两处文档没有规定、由本实现补齐的边已在下方注释标出。
    pub fn can_transition_to(self, next: Self) -> bool {
        use UnitState::{Checkpointing, Cold, Faulted, Loading, Quarantined, Ready, Running, Waiting};

        if self == next {
            return false;
        }

        match (self, next) {
            // §9.2：任意状态 -> FAULTED / QUARANTINED
            (_, Faulted) | (_, Quarantined) => true,

            (Cold, Loading) => true,
            // §9.2：LOADING 超时可取消
            (Loading, Ready) | (Loading, Cold) => true,
            (Ready, Running) | (Ready, Checkpointing) => true,
            (Running, Waiting) | (Running, Ready) | (Running, Checkpointing) => true,
            (Waiting, Ready) | (Waiting, Checkpointing) => true,
            (Checkpointing, Cold) => true,

            // 文档未规定故障与隔离后的回归路径，以下为工程选择。
            (Faulted, Loading) | (Faulted, Cold) => true,
            (Quarantined, Cold) => true,

            // 注意：READY/WAITING -> COLD 不直接放行，必须经过 CHECKPOINTING（§9.2 降温步骤）。
            _ => false,
        }
    }
}

/// 单元持久化快照（§3.2 的字段集合）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSnapshot {
    /// 单元标识。
    pub unit_id: UnitId,
    /// 单元种类。
    pub kind: UnitKind,
    /// 契约版本。
    pub schema_version: u16,
    /// 任务域。
    pub scope: Scope,
    /// 受托目标。
    pub goal_refs: Vec<GoalId>,
    /// 信念修订号。
    pub belief_revision: u64,
    /// 当前信念快照引用（小项入事务库，大项引用内容仓）。
    pub belief_snapshot_ref: BlobRef,
    /// 证据引用。
    pub evidence_refs: Vec<EvidenceRef>,
    /// 关系边引用。
    pub relation_refs: Vec<RelationRef>,
    /// 策略版本。
    pub strategy_version: StrategyVersion,
    /// 模型画像引用。注意是引用，不是模型权重本身。
    pub model_profile_ref: ModelProfileRef,
    /// 能力策略引用。
    pub capability_policy_ref: CapabilityPolicyRef,
    /// 预算账引用。
    pub budget_ref: BudgetRef,
    /// 尚未了结的动作。
    pub pending_action_ids: Vec<ActionId>,
    /// 已应用到的事件序列号。
    pub last_applied_sequence: u64,
    /// 当前状态。
    pub state: UnitState,
}

impl UnitSnapshot {
    /// 校验快照自洽性。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ContractError::SchemaVersionMismatch {
                expected: SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        assert_unique(&self.goal_refs, "unit.goal_refs")?;
        assert_unique(&self.evidence_refs, "unit.evidence_refs")?;
        assert_unique(&self.relation_refs, "unit.relation_refs")?;
        assert_unique(&self.pending_action_ids, "unit.pending_action_ids")?;

        // 冷态快照不允许残留未决动作：冷态只在注册表和持久邮箱中存在（§9.2）。
        if self.state == UnitState::Cold && !self.pending_action_ids.is_empty() {
            return Err(ContractError::UnresolvedPendingActions {
                to: UnitState::Cold.as_str(),
                count: self.pending_action_ids.len(),
                sample: self.pending_action_ids[0].to_string(),
            });
        }
        Ok(())
    }

    /// 把未决动作移交给在线动作账，返回被移交的动作。
    ///
    /// §9.2：存在不明副作用时由持久在线动作账继续核对，不以卸载单元"解决"它。调用本方法
    /// 表示这些动作的所有权已经转交，单元之后才可以降温。
    pub fn drain_pending_for_action_ledger(&mut self) -> Vec<ActionId> {
        std::mem::take(&mut self.pending_action_ids)
    }

    /// 迁移状态。
    ///
    /// 进入 `COLD` 前必须已经没有未决动作，否则拒绝并返回
    /// [`ContractError::UnresolvedPendingActions`]。
    pub fn transition_to(&mut self, next: UnitState) -> Result<(), ContractError> {
        if !self.state.can_transition_to(next) {
            return Err(ContractError::LifecycleViolation {
                from: self.state.as_str(),
                to: next.as_str(),
            });
        }
        if next == UnitState::Cold && !self.pending_action_ids.is_empty() {
            return Err(ContractError::UnresolvedPendingActions {
                to: next.as_str(),
                count: self.pending_action_ids.len(),
                sample: self.pending_action_ids[0].to_string(),
            });
        }
        self.state = next;
        Ok(())
    }
}

/// 快照中有意的字段名清单，用于回归测试断言"没有混入句柄类字段"。
pub const UNIT_SNAPSHOT_FIELDS: &[&str] = &[
    "unit_id",
    "kind",
    "schema_version",
    "scope",
    "goal_refs",
    "belief_revision",
    "belief_snapshot_ref",
    "evidence_refs",
    "relation_refs",
    "strategy_version",
    "model_profile_ref",
    "capability_policy_ref",
    "budget_ref",
    "pending_action_ids",
    "last_applied_sequence",
    "state",
];

// ---------------------------------------------------------------------------
// §3.1 一轮闭环的合同
// ---------------------------------------------------------------------------

/// 一轮里允许提出的最大候选数。
///
/// §4.1 L2 要求"黑板有界"。候选列表是黑板的主要消费者，所以它的界必须是一个编译期常量，
/// 而不是由提出方自行决定——否则"有界"就只是建议。
pub const MAX_CANDIDATES: usize = 32;

/// 一个冲突里允许的最大立场数。
pub const MAX_CONFLICT_POSITIONS: usize = 8;

/// 单元在一轮里能提出的东西（§3.1）。
///
/// §3.1 明确"单元'行动'可以只是申请新观测、提交带证据的候选或请求调用计算器；**不要求每个
/// 单元都有 OS 写权限**"。所以候选不是"动作"的同义词：只有 [`Candidate::RequestAction`]
/// 可能推动副作用，而它走的是既有的 [`ActionIntent`] 通道，因此仍然受"预测先于动作"与
/// 执行许可的约束——候选这一层无法绕开它们，只是把它们包了一层。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Candidate {
    /// 申请一次新观测。用于"证据不足"，而不是"先猜一下"。
    RequestObservation {
        /// 想观测的对象引用。
        subject_ref: String,
        /// 为什么需要它。
        reason: String,
    },
    /// 提请一次动作。**尚未获许可**，也还没有副作用。
    RequestAction {
        /// 动作意图。它自身已强制携带 `prediction_ref`（§6.3）。
        intent: Box<ActionIntent>,
    },
    /// 提请一次工具调用。
    RequestTool {
        /// 工具标识。
        tool_id: ToolId,
        /// 结构化参数。不接受拼接出来的命令行字符串（§12.2）。
        parameters: Value,
    },
    /// 提交一条带证据的结论。
    Claim {
        /// 命题。
        statement: String,
        /// 支持它的证据。必须非空。
        evidence_refs: Vec<EvidenceRef>,
    },
}

impl Candidate {
    /// 候选的种类。
    ///
    /// 用途不是分类学，而是授权：§8 的上下文携带一份 [`crate::OutputSchema`]，里面列出本次
    /// 允许的候选种类。只读任务给出的 schema 里没有 [`CandidateKind::Action`]，因此模型即使
    /// 想提动作也过不了校验。
    pub fn kind(&self) -> CandidateKind {
        match self {
            Self::RequestObservation { .. } => CandidateKind::Observation,
            Self::RequestAction { .. } => CandidateKind::Action,
            Self::RequestTool { .. } => CandidateKind::Tool,
            Self::Claim { .. } => CandidateKind::Claim,
        }
    }

    /// 这条候选是否可能推动外部副作用。
    ///
    /// 只有 [`Candidate::RequestAction`] 为真。申请观测、请求工具、提交结论都不改变世界；
    /// 把它们和动作混为一谈，会让"只有执行许可能触发副作用"这条边界在候选层就糊掉。
    pub fn may_have_side_effect(&self) -> bool {
        matches!(self, Self::RequestAction { .. })
    }

    /// 这条候选自己携带的证据。
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        match self {
            Self::Claim { evidence_refs, .. } => evidence_refs,
            _ => &[],
        }
    }
}

/// 冲突中的一方。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictPosition {
    /// 主张。
    pub statement: String,
    /// 提出这一方主张的单元。
    pub by: UnitId,
    /// 支持它的证据。**必须非空**。
    ///
    /// §7.2 的真相来源是原始可核验证据，不是多数意见。一个不带证据的立场如果不许写进来，
    /// 那么"用多数意见覆盖矛盾"就失去了操作对象——多数意见本身也需要各自带证据才成立。
    pub evidence_refs: Vec<EvidenceRef>,
}

/// 一个未被消解的冲突（§4.2）。
///
/// §4.2 要求父单元的输出"包含子结果、关系索引、**冲突**、未决条件和证据引用，不能只拼接
/// 子摘要或用多数意见覆盖矛盾"。把冲突做成候选集合里的一等成员（而不是在 `propose` 内部
/// 悄悄选边然后只交出胜者），是这条要求唯一可被机械检查的形式。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    /// 冲突围绕的对象或命题。
    pub subject_ref: String,
    /// 各方立场。至少两方，且必须来自不同单元。
    pub positions: Vec<ConflictPosition>,
}

/// 一个尚未解决的问题（§4.2 的"未决条件"）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unresolved {
    /// 待回答的问题。
    pub question: String,
    /// 缺什么才能回答它。**必须非空**。
    ///
    /// 只写"还没想清楚"等于没写。写明缺口（缺哪条证据、缺哪个权限、缺哪次观测）才能让
    /// 上层决定是补证据、升级审批，还是就此收手（§6 第 9 步的"请求澄清"）。
    pub missing: Vec<String>,
}

/// 单元一轮提出的候选集合（§3.1、§4.2）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSet {
    /// 候选本身。
    pub candidates: Vec<Candidate>,
    /// 未被消解的冲突。**原样保留**，不在这一层选边。
    pub conflicts: Vec<Conflict>,
    /// 尚未解决的问题。
    pub unresolved: Vec<Unresolved>,
}

impl CandidateSet {
    /// 空集合。这是合法输出：这一轮该单元无事可做。
    ///
    /// 不把"无事可做"表达成错误，是因为 §6 第 9 步允许"有限预算内结束"，而一个只能靠报错
    /// 才能安静的单元会污染审计账。
    pub fn empty() -> Self {
        Self {
            candidates: Vec::new(),
            conflicts: Vec::new(),
            unresolved: Vec::new(),
        }
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.conflicts.is_empty() && self.unresolved.is_empty()
    }

    /// 是否有候选会推动副作用。
    pub fn requests_side_effect(&self) -> bool {
        self.candidates.iter().any(Candidate::may_have_side_effect)
    }

    /// 集合里所有被引用的证据，按出现顺序。
    pub fn evidence_refs(&self) -> Vec<&EvidenceRef> {
        let mut found: Vec<&EvidenceRef> = Vec::new();
        for candidate in &self.candidates {
            found.extend(candidate.evidence_refs());
        }
        for conflict in &self.conflicts {
            for position in &conflict.positions {
                found.extend(position.evidence_refs.iter());
            }
        }
        found
    }

    /// 校验集合自洽性。
    ///
    /// 逐条拒绝：
    /// * 候选数超过 [`MAX_CANDIDATES`]（§4.1 L2 黑板有界）；
    /// * 结论类候选不带证据（§6 第 4 步"验证候选结构与证据存在性"）；
    /// * "冲突"只有一个立场，或立场全部出自同一个单元——那不是冲突，是主张；
    /// * 冲突立场不带证据，或立场数超过 [`MAX_CONFLICT_POSITIONS`]；
    /// * 未决问题不写明缺口。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.candidates.len() > MAX_CANDIDATES {
            return Err(ContractError::CandidateLimitExceeded {
                limit: MAX_CANDIDATES,
                actual: self.candidates.len(),
            });
        }

        for candidate in &self.candidates {
            if let Candidate::Claim { evidence_refs, .. } = candidate
                && evidence_refs.is_empty()
            {
                return Err(ContractError::MissingRefs {
                    field: "candidate.claim.evidence_refs",
                });
            }
        }

        for conflict in &self.conflicts {
            if conflict.positions.len() < 2 {
                return Err(ContractError::ConflictNeedsTwoSides {
                    subject_ref: conflict.subject_ref.clone(),
                    positions: conflict.positions.len(),
                });
            }
            if conflict.positions.len() > MAX_CONFLICT_POSITIONS {
                return Err(ContractError::ConflictTooManyPositions {
                    subject_ref: conflict.subject_ref.clone(),
                    limit: MAX_CONFLICT_POSITIONS,
                    actual: conflict.positions.len(),
                });
            }
            for position in &conflict.positions {
                if position.evidence_refs.is_empty() {
                    return Err(ContractError::MissingRefs {
                        field: "conflict.position.evidence_refs",
                    });
                }
            }
            // 同一个单元自己跟自己不构成冲突。§4.2 说的冲突指的是子单元之间的分歧。
            let first = &conflict.positions[0].by;
            if conflict.positions.iter().all(|position| &position.by == first) {
                return Err(ContractError::ConflictNeedsDistinctUnits {
                    subject_ref: conflict.subject_ref.clone(),
                });
            }
        }

        for unresolved in &self.unresolved {
            if unresolved.missing.is_empty() {
                return Err(ContractError::MissingRefs {
                    field: "unresolved.missing",
                });
            }
        }

        Ok(())
    }
}

/// 最小认知单元必须实现的合同（§3.1、§4.2）。
///
/// §3.1 的原话是"没有这个合同的函数不标为完整主体"。因此这个 trait 不是"建议实现"的接口，
/// 它是"完整主体"的定义本身。§4.2 进一步要求复合单元对外暴露**同一份**合同，所以能力簇和
/// 主体实现的是它，而不是另立一套只做汇总的接口。
///
/// 一个刻意的取舍：`propose` 收 `&self`，因为提出候选不应该改变单元状态——状态改变只发生在
/// [`CognitiveUnit::observe`] 与 [`CognitiveUnit::handle_result`] 这两处，也就是"收到事实"
/// 与"比较预测和现实"这两个时刻。让 `propose` 能改状态，等于允许单元在提出主张时顺手把自己
/// 的信念调成方便通过的样子。
///
/// 要求 `Send`：§4.1 L4 把重计算放进有上限的 worker 池，单元要能在池里移动。不要求 `Send`
/// 会把整个架构钉死在一条线程上，而那是一条等到最后才发现的结构性限制。
pub trait CognitiveUnit: Send {
    /// 单元标识。
    fn unit_id(&self) -> &UnitId;

    /// 单元种类。用于区分叶子与复合，但**不改变合同**。
    fn kind(&self) -> UnitKind;

    /// §3.1 前半：收到事件，取证，更新局部信念。
    ///
    /// `event` 是 §7.1 的公共信封。实现方必须先判断它是否属于自己的任务域，再决定是否采纳；
    /// 不属于的事件应当被忽略而不是被硬塞进信念。
    fn observe(&mut self, event: &Envelope, at: WallClock) -> Result<(), ContractError>;

    /// §3.1 中段：提出候选及其预期后果。
    fn propose(&self, at: WallClock) -> Result<CandidateSet, ContractError>;

    /// §3.1 中段：把某条候选的预期后果写成**可检查**的预测。
    ///
    /// §6.3 要求预测含对象、预计变化、时间窗、失败条件和不确定性。返回的
    /// [`Prediction`] 已经由契约层强制携带机器可检查的 `Expectation`，所以本方法不可能
    /// 交出一份只有散文的"预测"。
    fn predict(&self, candidate: &Candidate, at: WallClock) -> Result<Prediction, ContractError>;

    /// §3.1 后半：比较预测与观测，更新可持久化状态。
    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        at: WallClock,
    ) -> Result<(), ContractError>;

    /// §3.2 的可持久化状态。必须能通过 [`UnitSnapshot::validate`]。
    fn snapshot(&self) -> UnitSnapshot;
}
