//! L5 记忆条目（§2、§4.1 L5、§12.3、§13.2）。
//!
//! §2 把"记忆条目"与"认知单元"严格分开：
//!
//! > 记忆条目：事实、事件、技能、关系或摘要，带来源与有效期 | 不是"必须常驻的活跃 agent"
//!
//! 以及：
//!
//! > 不为每条知识创建一个自主 actor。
//!
//! 所以记忆是**数据**，不是一批小 agent。本模块把它做成类型。
//!
//! 三条在结构上无法违反的约束：
//!
//! 1. **没有证据的不算记忆**。每个条目必须至少引用一条可核验证据（§7.2 的真相来源是原始
//!    证据，不是"模型说的"）。这条与 [`crate::Candidate`] 的结论要求同源：没有证据的东西
//!    可以存在于草稿里，但不能进入任何被当作事实使用的存储。
//! 2. **修订不覆盖原证据**。§13.2 要求"记忆/策略改进……**不覆盖原证据**"。所以修订产出的是
//!    **新条目**，旧条目转 [`MemoryStatus::Superseded`] 并指向继任者，而不是被原地改写。
//! 3. **删除先隐藏、后清理**。§12.3："先写 tombstone 使查询立即不可见，再异步清理，并给用户
//!    完成状态。"[`MemoryStatus::Tombstoned`] 就是那个"立即不可见"的状态，它由持久层负责
//!    过滤，而不是由调用方自觉跳过。

use serde::{Deserialize, Serialize};

use crate::validate::assert_unique;
use crate::{
    CalibratedProbability, ContractError, DataClass, EvidenceRef, MemoryId, Provenance,
    RelationRef, SubjectId, TaskId, UnitId, WallClock,
};

/// 记忆条目的类别（§2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// 语义事实与长期偏好。
    Fact,
    /// 情景：一次对话或一次任务过程。
    Episode,
    /// 技能：可回放的策略或操作序列（§3.3 的"技能回放"）。
    Skill,
    /// 关系：来源谱系、依赖、冲突边（§3.3 的"关系合并"）。
    Relation,
    /// 摘要：对既有条目的压缩（§4.1 L5 的"保留期与摘要"）。
    Summary,
}

impl MemoryKind {
    /// 全部类别。
    pub const ALL: [Self; 5] = [
        Self::Fact,
        Self::Episode,
        Self::Skill,
        Self::Relation,
        Self::Summary,
    ];

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Episode => "episode",
            Self::Skill => "skill",
            Self::Relation => "relation",
            Self::Summary => "summary",
        }
    }

    /// §12.3 给出的第一版默认保留期（天）。`None` 表示不自动过期。
    ///
    /// 逐条对应 §12.3 那张表：
    ///
    /// | §12.3 的行 | 本模块的对应 |
    /// |---|---|
    /// | 对话与转写，初值 7 天 | [`MemoryKind::Episode`] → 7 |
    /// | 长期偏好/语义事实，只有明确需记忆的内容才提升 | [`MemoryKind::Fact`] → 不自动过期 |
    /// | 授权文件派生索引，引用源版本 | 靠证据失效传播（`tombstone_by_evidence`），不靠 TTL |
    ///
    /// 摘要按情景同样处理：它是从有保留期的内容派生的，自身不该活得比来源更久。
    pub fn default_retention_days(self) -> Option<i64> {
        match self {
            Self::Episode | Self::Summary => Some(7),
            Self::Fact | Self::Skill | Self::Relation => None,
        }
    }
}

/// 记忆条目的可见状态（§12.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// 可被检索到。
    Active,
    /// 已被新修订取代。仍然可见于审计，但不参与检索（§13.2：不覆盖原证据）。
    Superseded,
    /// 已删除。**立即不可见**，等待异步清理（§12.3）。
    Tombstoned,
}

impl MemoryStatus {
    /// 是否参与正常检索。
    ///
    /// 只有 [`MemoryStatus::Active`] 为真。持久层在查询时按本方法过滤，而不是由调用方
    /// 记得跳过——§12.3 要的是"立即不可见"，不是"读的人自觉不看"。
    pub fn is_visible(self) -> bool {
        matches!(self, Self::Active)
    }

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// 一条记忆条目。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEntry {
    /// 条目标识。
    pub memory_id: MemoryId,
    /// 类别。
    pub kind: MemoryKind,
    /// 所有者。§4.1 L5："横切；**按所有者和任务隔离**"。
    pub owner: SubjectId,
    /// 所属任务。`None` 表示跨任务的长期条目。
    pub task_id: Option<TaskId>,
    /// 写入这条记忆的单元。
    pub unit_id: Option<UnitId>,
    /// 命题原文。
    pub claim: String,
    /// 支持它的证据。**必须非空**。
    pub evidence_refs: Vec<EvidenceRef>,
    /// 关系边。
    pub relation_refs: Vec<RelationRef>,
    /// 出处。
    pub provenance: Provenance,
    /// 数据类别。
    pub data_class: DataClass,
    /// 校准过的置信度。未校准的自评分数不允许出现在这里（§3.2）。
    pub confidence: Option<CalibratedProbability>,
    /// 修订号，从 1 开始。每次取代都产生一条新条目并加一。
    pub revision: u32,
    /// 被本条取代的旧条目。
    pub supersedes: Option<MemoryId>,
    /// 取代本条的继任条目。由持久层在取代时回填。
    pub superseded_by: Option<MemoryId>,
    /// 写入时刻。
    pub recorded_at: WallClock,
    /// 失效时刻。`None` 表示不自动过期。
    pub valid_until: Option<WallClock>,
    /// 当前状态。
    pub status: MemoryStatus,
    /// 被 tombstone 的时刻。
    pub tombstoned_at: Option<WallClock>,
    /// 被 tombstone 的原因（用户删除、权限撤回、TTL 到期等）。
    pub tombstone_reason: Option<String>,
}

impl MemoryEntry {
    /// 构造一条记忆。
    ///
    /// `valid_until` 按 [`MemoryKind::default_retention_days`] 自动填好；要覆盖它请用
    /// [`MemoryEntry::with_valid_until`]。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory_id: MemoryId,
        kind: MemoryKind,
        owner: SubjectId,
        task_id: Option<TaskId>,
        claim: impl Into<String>,
        evidence_refs: Vec<EvidenceRef>,
        provenance: Provenance,
        data_class: DataClass,
        recorded_at: WallClock,
    ) -> Result<Self, ContractError> {
        let claim = claim.into();
        let entry = Self {
            memory_id,
            kind,
            owner,
            task_id,
            unit_id: None,
            claim,
            evidence_refs,
            relation_refs: Vec::new(),
            provenance,
            data_class,
            confidence: None,
            revision: 1,
            supersedes: None,
            superseded_by: None,
            recorded_at,
            valid_until: kind
                .default_retention_days()
                .map(|days| recorded_at.plus_seconds(days.saturating_mul(86_400))),
            status: MemoryStatus::Active,
            tombstoned_at: None,
            tombstone_reason: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    /// 指定失效时刻，覆盖默认保留期。
    pub fn with_valid_until(mut self, valid_until: Option<WallClock>) -> Self {
        self.valid_until = valid_until;
        self
    }

    /// 指定写入它的单元。
    pub fn with_unit(mut self, unit_id: UnitId) -> Self {
        self.unit_id = Some(unit_id);
        self
    }

    /// 附加关系边。
    pub fn with_relations(mut self, relation_refs: Vec<RelationRef>) -> Self {
        self.relation_refs = relation_refs;
        self
    }

    /// 附加校准过的置信度。
    pub fn with_confidence(mut self, confidence: CalibratedProbability) -> Self {
        self.confidence = Some(confidence);
        self
    }

    /// 声明本条取代了哪一条旧记忆。
    pub fn superseding(mut self, previous: MemoryId, revision: u32) -> Self {
        self.supersedes = Some(previous);
        self.revision = revision;
        self
    }

    /// 校验。
    ///
    /// 逐条拒绝：
    /// * 没有任何证据——没有证据的东西可以存在于草稿里，但不能进入被当作事实使用的存储；
    /// * 命题为空；
    /// * 失效时刻不晚于写入时刻（那就是一条生下来就已经过期的记忆）；
    /// * 置信度未校准（§3.2）。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.evidence_refs.is_empty() {
            return Err(ContractError::MissingRefs {
                field: "memory.evidence_refs",
            });
        }
        if self.claim.trim().is_empty() {
            return Err(ContractError::EmptyField {
                field: "memory.claim",
            });
        }
        assert_unique(&self.evidence_refs, "memory.evidence_refs")?;
        assert_unique(&self.relation_refs, "memory.relation_refs")?;

        if let Some(valid_until) = self.valid_until
            && valid_until <= self.recorded_at
        {
            return Err(ContractError::InvalidTimeWindow {
                start: self.recorded_at.to_string(),
                end: valid_until.to_string(),
            });
        }

        if let Some(confidence) = &self.confidence {
            confidence.validate()?;
        }
        Ok(())
    }

    /// 在本条被取代成为历史之后，构造下一条修订。
    ///
    /// 返回的是**新条目**，而不是对原条目就地修改。§13.2 要求记忆改进"不覆盖原证据"，
    /// 原地改写会让"当初判断的依据是什么"这个问题永远无法回答。
    pub fn next_revision(
        &self,
        memory_id: MemoryId,
        claim: impl Into<String>,
        evidence_refs: Vec<EvidenceRef>,
        recorded_at: WallClock,
    ) -> Result<Self, ContractError> {
        let mut next = Self::new(
            memory_id,
            self.kind,
            self.owner.clone(),
            self.task_id.clone(),
            claim,
            evidence_refs,
            self.provenance.clone(),
            self.data_class,
            recorded_at,
        )?;
        next.unit_id = self.unit_id.clone();
        next.relation_refs = self.relation_refs.clone();
        next.supersedes = Some(self.memory_id.clone());
        next.revision = self.revision.saturating_add(1);
        Ok(next)
    }

    /// 当前是否可被检索到。
    pub fn is_visible(&self) -> bool {
        self.status.is_visible()
    }

    /// 在给定时刻是否已经超过保留期。
    ///
    /// 到期**不等于**已经删除：§12.3 要求删除走 tombstone 流程并给用户完成状态，所以这里
    /// 只回答"该清理了"，由持久层显式发起。
    pub fn is_expired_at(&self, at: WallClock) -> bool {
        self.is_visible() && self.valid_until.is_some_and(|until| until <= at)
    }
}
