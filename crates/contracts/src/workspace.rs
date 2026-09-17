//! L2 工作空间：能力簇小黑板（§4.1、§6 第 4 步）。
//!
//! §4.1 对 L2 只给了两条约束，但它们都不是装饰：
//!
//! > L2工作空间 | 能力簇小黑板＋主体有限全局黑板 | 各组合边界有小空间，不无限复制
//!
//! > 黑板有界，证据不能被执行层改写
//!
//! 本模块把这两条做成结构上无法违反的东西：
//!
//! 1. **有界**。主题数与总字节数都有编译期常量上限，超出即拒绝写入。调用方不能"先写进去
//!    再说"——§4.1 L2 说的"不无限复制"就落在这里。
//! 2. **证据只增不减**。[`Workspace`] 没有任何删除或替换证据的方法，证据只能随笔记一起
//!    追加。执行层（L4 的调度与执行代理）手里拿不到一个能改写证据的句柄，因为不存在这样
//!    的方法。这比"约定执行层不要改"要强：约定会被绕过，缺失的方法不会。
//!
//! 顺带承担 §6 第 4 步的职责：
//!
//! > L2验证候选结构与证据存在性
//!
//! [`Workspace::validate_candidates`] 就是这句话。它让 [`crate::EvidenceRef`] 从一个"长得
//! 像引用的字符串"变成"必须在黑板上确实存在的东西"——否则模型或实现方可以随手编一个
//! `obs:xxx` 当证据用，而没人能发现。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ActionId, CandidateSet, ContractError, EvidenceRef, UnitId, Verdict, WallClock};

/// 黑板允许的最大主题数。
pub const MAX_WORKSPACE_TOPICS: usize = 64;

/// 黑板允许的最大字节数，按笔记的序列化长度累计。
pub const MAX_WORKSPACE_BYTES: usize = 256 * 1024;

/// 一条写在黑板上的笔记。
///
/// 每个变体都**必须携带证据**。这是本模块最重要的一条不变量：既然证据只能随笔记进入黑板，
/// 而笔记又必须带证据，那么"黑板上有一条不带证据的结论"这件事就不可能发生。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceNote {
    /// 子单元的一条发现。
    Finding {
        /// 命题。
        statement: String,
        /// 支持它的证据。必须非空。
        evidence_refs: Vec<EvidenceRef>,
    },
    /// 一次动作的结果判定（§6 第 7–8 步）。
    Outcome {
        /// 对应的动作。
        action_id: ActionId,
        /// 判定结果。
        verdict: Verdict,
        /// 支撑判定的观测。必须非空。
        observation_refs: Vec<EvidenceRef>,
    },
}

impl WorkspaceNote {
    /// 这条笔记携带的证据。
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        match self {
            Self::Finding { evidence_refs, .. } => evidence_refs,
            Self::Outcome { observation_refs, .. } => observation_refs,
        }
    }

    /// 可读摘要，用于审计与界面。
    pub fn summary(&self) -> String {
        match self {
            Self::Finding { statement, .. } => statement.clone(),
            Self::Outcome {
                action_id, verdict, ..
            } => format!("{action_id} -> {verdict:?}"),
        }
    }
}

/// 一个主题下累积的笔记。
///
/// 笔记是**追加**的，不是覆盖的。同一主题被再次写入时旧笔记仍然保留：§4.2 要求父单元不能
/// 用新的结论把旧的子结果悄悄挤掉，历史必须留在原处供审计与重放读取。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEntry {
    /// 主题名。
    pub topic: String,
    /// 按写入顺序排列的笔记。
    pub notes: Vec<WorkspaceNote>,
    /// 最后一次写入者。
    pub posted_by: UnitId,
    /// 本主题被写入的次数。
    pub revision: u64,
    /// 最后一次写入时刻。
    pub last_posted_at: WallClock,
}

/// L2 工作空间。
///
/// 一个能力簇一个实例（§4.1："各组合边界有小空间，不无限复制"）；主体另有一个有限的全局
/// 黑板，二者不共享实例。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    entries: BTreeMap<String, WorkspaceEntry>,
    /// 证据集合。只增不减，且只能由 [`Workspace::post`] 追加。
    evidence: Vec<EvidenceRef>,
    posts: u64,
    bytes: usize,
}

impl Workspace {
    /// 一块空黑板。
    pub fn new() -> Self {
        Self::default()
    }

    /// 写一条笔记。
    ///
    /// 逐条拒绝：
    /// * 笔记不带证据——没有证据的结论不进黑板（§7.2：真相来源是可核验证据）；
    /// * 主题数或总字节数超过上限（§4.1 L2：黑板有界）。
    pub fn post(
        &mut self,
        topic: impl Into<String>,
        note: WorkspaceNote,
        posted_by: &UnitId,
        at: WallClock,
    ) -> Result<(), ContractError> {
        if note.evidence_refs().is_empty() {
            return Err(ContractError::MissingRefs {
                field: "workspace_note.evidence_refs",
            });
        }

        let topic = topic.into();
        let payload = serde_json::to_vec(&note).map_err(|_| ContractError::EncodingFailed {
            field: "workspace_note",
        })?;
        let added = payload.len().saturating_add(topic.len());

        // 同一主题的重复写入不新增主题，只增长字节。
        let is_new_topic = !self.entries.contains_key(&topic);
        if is_new_topic && self.entries.len() >= MAX_WORKSPACE_TOPICS {
            return Err(ContractError::WorkspaceTopicLimitExceeded {
                limit: MAX_WORKSPACE_TOPICS,
                actual: self.entries.len() + 1,
            });
        }
        let projected = self.bytes.saturating_add(added);
        if projected > MAX_WORKSPACE_BYTES {
            return Err(ContractError::WorkspaceByteLimitExceeded {
                limit: MAX_WORKSPACE_BYTES,
                actual: projected,
            });
        }

        // 证据先追加后写笔记：两条都只在同一个方法里发生，因此不存在"笔记进了黑板但证据
        // 没进"的中间态。
        for reference in note.evidence_refs() {
            if !self.evidence.contains(reference) {
                self.evidence.push(reference.clone());
            }
        }

        self.bytes = projected;
        self.posts = self.posts.saturating_add(1);

        match self.entries.get_mut(&topic) {
            Some(entry) => {
                entry.notes.push(note);
                entry.posted_by = posted_by.clone();
                entry.revision = entry.revision.saturating_add(1);
                entry.last_posted_at = at;
            }
            None => {
                self.entries.insert(
                    topic.clone(),
                    WorkspaceEntry {
                        topic,
                        notes: vec![note],
                        posted_by: posted_by.clone(),
                        revision: 1,
                        last_posted_at: at,
                    },
                );
            }
        }
        Ok(())
    }

    /// 读一个主题。
    pub fn entry(&self, topic: &str) -> Option<&WorkspaceEntry> {
        self.entries.get(topic)
    }

    /// 全部主题，按字典序。
    pub fn topics(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// 黑板上的全部证据。**只读**：本类型不提供任何修改它的方法。
    pub fn evidence(&self) -> &[EvidenceRef] {
        &self.evidence
    }

    /// 某条证据是否确实在黑板上。
    pub fn has_evidence(&self, reference: &EvidenceRef) -> bool {
        self.evidence.contains(reference)
    }

    /// 主题数。
    pub fn topic_count(&self) -> usize {
        self.entries.len()
    }

    /// 笔记总数。
    pub fn note_count(&self) -> usize {
        self.entries.values().map(|entry| entry.notes.len()).sum()
    }

    /// 累计写入次数。
    pub fn posts(&self) -> u64 {
        self.posts
    }

    /// 当前占用的字节数。
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// 黑板是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// §6 第 4 步：**L2 验证候选结构与证据存在性**。
    ///
    /// 两件事：
    /// 1. 候选集合自身的结构规则（§4.2 的冲突形式、证据非空、黑板有界）先过一遍；
    /// 2. 集合引用到的每一条证据，都必须在黑板上确实存在。
    ///
    /// 第 2 条是这套设计里"证据"一词有分量的原因。没有它，`EvidenceRef` 只是一个前缀正确
    /// 的字符串，任何实现方（或模型）都能编造 `obs:whatever` 并通过全部校验。
    pub fn validate_candidates(&self, set: &CandidateSet) -> Result<(), ContractError> {
        set.validate()?;
        for reference in set.evidence_refs() {
            if !self.has_evidence(reference) {
                return Err(ContractError::EvidenceNotOnWorkspace {
                    evidence_ref: reference.to_string(),
                });
            }
        }
        Ok(())
    }
}
