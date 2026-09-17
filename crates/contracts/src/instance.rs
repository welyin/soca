//! 单元实例与拓扑世代（实施规格 §6、§7；主架构 §3.2、§9.2）。
//!
//! 本模块区分两种"单元记录"，它们回答的是不同问题：
//!
//! * [`crate::UnitSnapshot`]（§3.2）回答"这个单元现在知道什么"——信念修订、证据引用、
//!   待处理动作、消费游标。它是运行状态。
//! * [`UnitInstance`]（本模块）回答"这个槽是谁的、属于哪个主体、绑在哪个模板与分片上、
//!   当前是第几个拓扑世代"。它是**注册与所有权**状态。
//!
//! 两者分开是有意的：§6.1 要求"没有 split / merge 函数的有状态单元不能自动拆并"，而是否
//! 可拆并取决于模板与分片，不取决于它此刻的信念。把它们塞进一张表，会让"这个槽能不能迁移"
//! 变成需要解析运行状态才能回答的问题。
//!
//! §7 的 SQL 把两件事合在 `unit_instances` 一行里（含 `snapshot_ref`）。本实现把运行快照
//! 留在 `units.snapshot_json`，把注册信息放同一行的其余列——仍然是**一行一个单元**，因为
//! 拆成两张表就会出现"注册说它在跑、快照说它是冷的"这种没法裁决的分歧。

use serde::{Deserialize, Serialize};

use crate::ids::opaque_string;
use crate::{
    BlobRef, ContractError, ReservationId, SubjectId, TopologyPlan, TransactionId, UnitId, UnitState,
    WallClock,
};

opaque_string!(
    /// 已签名单元模板标识。
    ///
    /// §6.1 要求模板声明 `partition_key`、`split`、`merge`、兼容版本、不可迁移状态、
    /// 最大孩子数、快照大小与预测唤醒成本。新槽只能绑定已签名模板；模型自述"我有新能力"
    /// 不能作为模板准入依据。
    TemplateId,
    "template_id",
    96
);

opaque_string!(
    /// 分片键。同一分片只允许一个可执行所有者（§6.2）。
    PartitionKey,
    "partition_key",
    256
);

/// 拓扑世代。
///
/// §6.3 的两条规则在这里成为类型约束：
///
/// * 世代从 1 起，0 无法与"未迁移"区分；
/// * **不能回退**。需要回退时必须创建更大的世代，而不是重新启用旧的 fencing token。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct TopologyEpoch(u64);

impl TopologyEpoch {
    /// 初始世代。
    pub const INITIAL: Self = Self(1);

    /// 取出数值。
    pub fn get(self) -> u64 {
        self.0
    }

    /// 下一个世代。
    pub fn next(self) -> Result<Self, ContractError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(ContractError::InvalidEpoch("世代已到上限，无法再递增"))
    }

    /// 判断本世代是否晚于另一个。
    pub fn is_newer_than(self, other: Self) -> bool {
        self.0 > other.0
    }
}

impl TryFrom<u64> for TopologyEpoch {
    type Error = ContractError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value == 0 {
            return Err(ContractError::InvalidEpoch("拓扑世代必须至少为 1"));
        }
        Ok(Self(value))
    }
}

impl From<TopologyEpoch> for u64 {
    fn from(value: TopologyEpoch) -> Self {
        value.0
    }
}

impl std::fmt::Display for TopologyEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// 单元实例：注册与所有权记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitInstance {
    /// 单元标识。同一标识跨世代复用，不按新空白单元丢掉经验。
    pub unit_id: UnitId,
    /// 所属主体。
    pub subject_id: SubjectId,
    /// 父单元。主体本身为 `None`。
    pub parent_id: Option<UnitId>,
    /// 绑定的已签名模板。
    pub template_id: TemplateId,
    /// 负责的分片。
    pub partition_key: PartitionKey,
    /// 生命周期状态。与同名快照字段必须一致。
    pub lifecycle: UnitState,
    /// 状态修订号。每次状态变化递增，用于 CAS。
    pub state_revision: u64,
    /// 已消费的事件游标。
    pub consumed_sequence: u64,
    /// 所属拓扑世代。
    pub topology_epoch: TopologyEpoch,
}

impl UnitInstance {
    /// 登记一个新的冷实例。
    pub fn new(
        unit_id: UnitId,
        subject_id: SubjectId,
        parent_id: Option<UnitId>,
        template_id: TemplateId,
        partition_key: PartitionKey,
    ) -> Self {
        Self {
            unit_id,
            subject_id,
            parent_id,
            template_id,
            partition_key,
            lifecycle: UnitState::Cold,
            state_revision: 0,
            consumed_sequence: 0,
            topology_epoch: TopologyEpoch::INITIAL,
        }
    }

    /// 校验。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.parent_id.as_ref() == Some(&self.unit_id) {
            return Err(ContractError::SelfCausalParent(
                self.unit_id.to_string(),
            ));
        }
        Ok(())
    }

    /// 推进状态修订号，返回新值。
    pub fn bump_revision(&mut self) -> u64 {
        self.state_revision = self.state_revision.saturating_add(1);
        self.state_revision
    }
}

/// 主体的当前路由。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectRoute {
    /// 主体标识。
    pub subject_id: SubjectId,
    /// 当前世代。CAS 切换的目标就是它（§6.2"路由提交"）。
    pub current_epoch: TopologyEpoch,
    /// 当前拓扑图的引用。
    pub graph_ref: BlobRef,
}

/// 迁移事务状态（§6.2 的状态机）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScaleTransactionState {
    /// 计划已生成。
    Planned,
    /// 峰值资源已预留。失败不启动迁移。
    Reserved,
    /// 已停止新租约，持久邮箱仍收件。
    Draining,
    /// 快照已单事务写入。
    Snapshotted,
    /// 影子已恢复，只读。
    ShadowReady,
    /// 路由已 CAS 切换并发出 fencing token。
    RouteCommitted,
    /// 旧 actor 正在退休。
    Retiring,
    /// 完成。
    Done,
    /// 提交前失败。
    RolledBack,
    /// 提交后失败，正在补偿。
    Recovering,
}

impl ScaleTransactionState {
    /// 稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "PLANNED",
            Self::Reserved => "RESERVED",
            Self::Draining => "DRAINING",
            Self::Snapshotted => "SNAPSHOTTED",
            Self::ShadowReady => "SHADOW_READY",
            Self::RouteCommitted => "ROUTE_COMMITTED",
            Self::Retiring => "RETIRING",
            Self::Done => "DONE",
            Self::RolledBack => "ROLLED_BACK",
            Self::Recovering => "RECOVERING",
        }
    }

    /// 是否尚未越过提交点。提交前失败可以回滚，提交后只能补偿。
    pub fn is_before_commit(self) -> bool {
        matches!(
            self,
            Self::Planned | Self::Reserved | Self::Draining | Self::Snapshotted | Self::ShadowReady
        )
    }

    /// 是否已经终结。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::RolledBack)
    }

    /// 是否允许迁移到 `next`。
    ///
    /// 依据 §6.2 的状态机。提交点前只能顺序前进或回滚；提交点后不能回滚，只能补偿——
    /// "需要回退时创建更大的 epoch，不能重新启用旧 fencing token"。
    pub fn can_transition_to(self, next: Self) -> bool {
        use ScaleTransactionState::{
            Done, Draining, Planned, Recovering, Reserved, Retiring, RolledBack, RouteCommitted,
            ShadowReady, Snapshotted,
        };
        match (self, next) {
            (Planned, Reserved | RolledBack) => true,
            (Reserved, Draining | RolledBack) => true,
            (Draining, Snapshotted | RolledBack) => true,
            (Snapshotted, ShadowReady | RolledBack) => true,
            (ShadowReady, RouteCommitted | RolledBack) => true,
            // 提交点之后：只能前进，或进入补偿。
            (RouteCommitted, Retiring | Recovering) => true,
            (Retiring, Done | Recovering) => true,
            (Recovering, Done) => true,
            _ => false,
        }
    }
}

/// 一次拓扑迁移事务。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleTransaction {
    /// 事务标识。
    pub transaction_id: TransactionId,
    /// 主体。
    pub subject_id: SubjectId,
    /// 旧世代。
    pub old_epoch: TopologyEpoch,
    /// 新世代。必须晚于旧世代。
    pub new_epoch: TopologyEpoch,
    /// 当前状态。
    pub state: ScaleTransactionState,
    /// 触发本次迁移的计划。
    pub plan: TopologyPlan,
    /// 资源预约标识。
    pub resource_reservation_id: ReservationId,
    /// 到期时刻。到期不是自动杀死有未决副作用的 Broker（§6.2）。
    pub deadline: WallClock,
}

impl ScaleTransaction {
    /// 校验。
    pub fn validate(&self) -> Result<(), ContractError> {
        if !self.new_epoch.is_newer_than(self.old_epoch) {
            return Err(ContractError::InvalidEpoch(
                "新世代必须晚于旧世代；回退要创建更大的世代而不是重用旧世代",
            ));
        }
        Ok(())
    }

    /// 是否仍在提交点之前。
    pub fn is_before_commit(&self) -> bool {
        self.state.is_before_commit()
    }
}
