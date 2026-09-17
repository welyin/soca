//! 认知单元快照与生命周期（§3.2、§9.1、§9.2）。
//!
//! 两条约束在这里变成类型和状态机：
//!
//! 1. 快照中不存裸内存指针、活连接句柄、密钥、GPU 指针或整个模型权重（§3.2）。做法是
//!    让每个字段都是带前缀校验的引用类型，结构上就没有能放句柄的位置。
//! 2. "存在不明副作用时，不能靠卸载单元来解决它"（§9.2）。做法是进入 `COLD` 之前必须
//!    先把未决动作移交给在线动作账，否则迁移被拒绝。

use serde::{Deserialize, Serialize};

use crate::validate::assert_unique;
use crate::{
    ActionId, BlobRef, BudgetRef, CapabilityPolicyRef, ContractError, DomainId, EvidenceRef,
    GoalId, ModelProfileRef, RelationRef, StrategyVersion, TaskContractVersion, UnitId,
    SCHEMA_VERSION,
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
