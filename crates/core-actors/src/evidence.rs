//! 证据台账：核对结论时手边的全部材料（§4.3「证据与风险评估」、§7.1）。
//!
//! 黑板（[`soca_contracts::Workspace`]）记的是**哪些证据进过这个簇**；台账记的是**每条证据
//! 到底观测到了什么、由谁观测、从哪里来**。两者分开是因为职责不同：黑板的那份必须小而有界
//! （§4.1 L2 的"不无限复制"），而核对来源时需要翻的是来源链。
//!
//! 台账里最要紧的一个判断是 [`EvidenceRecord::is_independent_of`]。§4.3 把"来源核对"列为
//! 「证据与风险评估」的第一个槽，§7.1 又要求"派生物必须指向原始观测"——这两条合起来意味着
//! 一件事：**同一段录屏转写出来的两份摘要，不是两次独立观测。** 它们只是同一次观测的两种
//! 抄写。把它们当成两条证据去凑门槛，等于用一次观测的重量压两次秤。

use std::collections::BTreeMap;

use soca_contracts::{EvidenceRef, Observation, UnitId};

/// 台账允许保留的最大证据条数。
///
/// 有界是 §4.1 L2 对黑板的同一要求。台账不参与上下文编译（那是主体的证据池的事），
/// 它服务的是"核对手边有什么"，因此保留最近的一批就够。
pub const MAX_EVIDENCE_RECORDS: usize = 256;

/// 一条证据的完整记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceRecord {
    /// 证据引用。
    pub evidence_ref: EvidenceRef,
    /// 观测对象。
    pub subject_ref: String,
    /// 观测到的值。
    pub observed_value: String,
    /// 派生来源。空表示这是一次直接观测（来自适配器或工具，不是从别的观测推出来的）。
    pub derived_from: Vec<EvidenceRef>,
    /// 记录它的单元。
    pub observed_by: UnitId,
}

impl EvidenceRecord {
    /// 从一条观测构造。
    pub fn from_observation(observation: &Observation) -> Self {
        Self {
            evidence_ref: observation.evidence_ref.clone(),
            subject_ref: observation.subject.clone(),
            observed_value: observation.value.clone(),
            derived_from: observation.derived_from.clone(),
            observed_by: observation.observed_by.clone(),
        }
    }

    /// 这条证据的来源集合。
    ///
    /// 派生观测的来源是它指向的原始观测；直接观测的来源是它自己。把"自己"也算进来源，
    /// 是为了让 [`EvidenceRecord::shares_origin_with`] 能正确处理一条直接观测与它的派生物
    /// 之间的关系——那两者当然同源。
    ///
    /// 只回溯一层。`Observed` 的契约只给了一层 `derived_from`，在这里假装能一路追到根，
    /// 就会让"独立性"建立在一个契约并不保证的推断上。
    fn origins(&self) -> Vec<&EvidenceRef> {
        if self.derived_from.is_empty() {
            vec![&self.evidence_ref]
        } else {
            self.derived_from.iter().collect()
        }
    }

    /// 与另一条是否同源。
    pub fn shares_origin_with(&self, other: &Self) -> bool {
        if self.evidence_ref == other.evidence_ref {
            return true;
        }
        let mine = self.origins();
        other
            .origins()
            .iter()
            .any(|candidate| mine.iter().any(|origin| origin == candidate))
    }

    /// 两条证据是否构成**互相独立的来源**（§4.3「来源核对」）。
    ///
    /// 两个条件同时成立才算独立：
    ///
    /// 1. **来源链没有交集。** 同源的两条证据是同一件事的两次抄写。
    /// 2. **观测者不是同一个单元。** 同一个单元读两遍，如果它读错了，两遍都错。那不构成
    ///    交叉核对——它只构成同一个错误被记录了两次。
    ///
    /// 第二条会让"独立来源"在很多场合难以满足，而那是**正确的**：只有一个观测者时，
    /// 结论确实没有得到独立核对，此时该说的是"无法判定"，不是"已获两个来源支持"。
    pub fn is_independent_of(&self, other: &Self) -> bool {
        !self.shares_origin_with(other) && self.observed_by != other.observed_by
    }
}

/// 证据台账。
///
/// 插入有序，因此超出上限时淘汰的是最早的那条，而不是按引用字典序随机淘汰。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvidenceLedger {
    records: Vec<EvidenceRecord>,
    index: BTreeMap<EvidenceRef, usize>,
}

impl EvidenceLedger {
    /// 一本空台账。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一条证据。
    ///
    /// 同一条引用重复写入时**保留先到的那份**。§4.1 L2 要求"证据不能被执行层改写"，而
    /// "后来的观测覆盖同引用的旧记录"正是改写的一种形式；何况同一个引用本就代表同一条观测。
    pub fn record(&mut self, record: EvidenceRecord) {
        if self.index.contains_key(&record.evidence_ref) {
            return;
        }
        if self.records.len() >= MAX_EVIDENCE_RECORDS {
            let evicted = self.records.remove(0);
            self.index.remove(&evicted.evidence_ref);
            self.reindex();
        }
        self.index
            .insert(record.evidence_ref.clone(), self.records.len());
        self.records.push(record);
    }

    /// 重建引用到下标的映射。
    ///
    /// 淘汰一条之后，它后面所有记录的下标都变了。落下这一步会让台账悄悄指向错的记录——
    /// 而"核对用的材料指错了"比"没有材料"更危险，因为它会给出一个看似有依据的判定。
    fn reindex(&mut self) {
        self.index.clear();
        for (position, record) in self.records.iter().enumerate() {
            self.index.insert(record.evidence_ref.clone(), position);
        }
    }

    /// 读一条记录。
    pub fn get(&self, reference: &EvidenceRef) -> Option<&EvidenceRecord> {
        self.index
            .get(reference)
            .and_then(|position| self.records.get(*position))
    }

    /// 全部记录，按写入顺序。
    pub fn records(&self) -> &[EvidenceRecord] {
        &self.records
    }

    /// 条数。
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 关于某个对象的全部观测。
    pub fn about(&self, subject_ref: &str) -> Vec<&EvidenceRecord> {
        self.records
            .iter()
            .filter(|record| record.subject_ref == subject_ref)
            .collect()
    }

    /// 一组引用里包含几个互不相同的来源（§4.3「来源核对」）。
    ///
    /// 做法是贪心地保留一批两两独立的记录。这不是最大独立集——那是指数级的，而这里是
    /// 每次核验都要跑的热路径。贪心只需保证**同一批里没有两条同源**，于是计数不会虚高；
    /// 它有可能少数出几个（当顺序不利时），方向是保守的，这正是要的方向。
    ///
    /// 台账里查不到的引用被跳过。它们不该出现——L2 的
    /// [`soca_contracts::Workspace::validate_candidates`] 已经把"引用不存在的证据"挡在
    /// 门外了——所以这里跳过而不是报错，是为了不在一个已被别处保证的前置条件上再制造一个
    /// 失败路径。
    pub fn independent_source_count(&self, refs: &[EvidenceRef]) -> usize {
        let mut kept: Vec<&EvidenceRecord> = Vec::new();
        for candidate in refs.iter().filter_map(|reference| self.get(reference)) {
            if kept
                .iter()
                .all(|existing| existing.is_independent_of(candidate))
            {
                kept.push(candidate);
            }
        }
        kept.len()
    }

    /// 一组引用里那些互相独立的记录本身。
    pub fn independent_sources(&self, refs: &[EvidenceRef]) -> Vec<&EvidenceRecord> {
        let mut kept: Vec<&EvidenceRecord> = Vec::new();
        for candidate in refs.iter().filter_map(|reference| self.get(reference)) {
            if kept
                .iter()
                .all(|existing| existing.is_independent_of(candidate))
            {
                kept.push(candidate);
            }
        }
        kept
    }

    /// 台账里出现过的全部观测值，去重后按字典序。
    pub fn known_values(&self) -> Vec<&str> {
        let mut values: Vec<&str> = self
            .records
            .iter()
            .map(|record| record.observed_value.as_str())
            .collect();
        values.sort_unstable();
        values.dedup();
        values
    }
}
