//! 保留期与删除（§12.3）。
//!
//! §12.3 把删除写成**两步**，而这句要求此前只有前半句有人执行：
//!
//! > 先写 tombstone 使查询立即不可见，**再异步清理**，并给用户完成状态。
//!
//! 数据结构早就够了：`MemoryEntry::valid_until` 按类别算好了保留期（[`MemoryKind::
//! default_retention_days`]），`Store::expired_memories` 能找出该清理的，
//! `Store::tombstone` / `Store::purge_tombstoned` / `Store::prune_audit_before` 也都写好了。
//! **没有任何东西调用它们。** 于是 `valid_until` 只是一个写在那里、永远不生效的数字。
//!
//! 本模块是那个驱动，并且刻意**不把两步合并**：合并之后，"已经隐藏"与"已经清理"这两个不同
//! 的状态就再也无法分别报告，而用户看到的"删除完成"指的应当是前者——后者可能要慢得多，
//! 而且不承诺在某个时刻之前完成。
//!
//! ## 范围：§12.3 那张表逐行的现状
//!
//! 那张表列的是六类**数据**，而 L5 记忆只是其中一行。逐行说明本模块管到哪：
//!
//! | §12.3 的行 | 现状 |
//! |---|---|
//! | 对话与转写（初值 7 天） | **没有生产者。** 环路只写 [`MemoryKind::Fact`]（不设过期）。而且 [`MemoryEntry`] 要求至少一条证据引用，用户的一句话并不产生证据——"对话内容存在哪里"这个问题得先想清楚，答案未必是记忆表。 |
//! | 长期偏好/语义事实（不自动过期） | 环路在写的就是它；`expire_retained` 确实不会碰它（有测试钉住）。 |
//! | 操作审计（初值 30 天） | 由 [`purge_retained`] 按 [`RetentionPolicy::audit_retention_days`] 裁剪。**这一行是完整的。** |
//! | 授权文件派生索引（撤销后立即不可检索） | [`Store::tombstone_by_evidence`] 实现了证据撤回那一侧的传播，等的是"权限撤回"的入口——那个入口还没有。 |
//! | 原始摄像头/屏幕帧、原始音频（不持久化） | 设计上就不落盘，因此没有保留期可执行；采集路径也还没有。 |
//!
//! 用户显式删除（[`forget`]，§12.3 的"用户可随时删除"）是**完整且今天就能用**的：
//! 它管的是任意一类记忆。
//!
//! **没有实现的是向派生内容的传播**。§12.3 要求删除"传播到快照、**摘要**、向量索引、
//! 模型会话缓存和备份保留计划"，而这需要一个"这条摘要引自哪几条记忆"的链接——当前系统里
//! 没有任何代码写过派生记忆，所以那套机制建起来会是一条两头都悬在空中的管子。
//! 等有东西真的开始产出摘要，它要解决的第一个问题就是这个链接。

use soca_contracts::{MemoryId, MemoryKind, WallClock};
use soca_storage::{audit::AuditCategory, StorageError, Store};

use crate::error::CoreError;

/// 保留期策略（§12.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// 审计账保留天数。§12.3 的初值是 30 天。
    pub audit_retention_days: i64,
    /// 隐藏之后是否**立刻**清理。
    ///
    /// 默认 `true` 只是因为省事；需要保留一个可恢复窗口的场合应当传 `false`，先把"看不见了"
    /// 这个状态交给用户，清理留给下一次。§12.3 的原文用的是"**异步**清理"，所以 `false`
    /// 不是降级，而是那条要求的字面意思。
    pub purge: bool,
    /// 是否裁掉超期的审计账。
    ///
    /// 单独一个开关，因为它与记忆保留的后果不同：审计是**追责**依据，§12.3 给了它 30 天，
    /// 但裁剪它会让"当初为什么这么做"永久无法回答。默认 `true`，但调用方应当知道自己在关什么。
    pub prune_audit: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            audit_retention_days: 30,
            purge: true,
            prune_audit: true,
        }
    }
}

/// 一条被隐藏的记忆。
///
/// **刻意不带命题原文。** 报告会进界面、进日志，而 §12.3 明确"不保存密码、完整 prompt 或
/// 无限个人内容"——从保留期里清掉一条记忆，却把它的内容抄进报告，等于绕了一圈又存了一份。
/// 标识与类别足够回答"哪一条、为什么"。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TombstonedMemory {
    /// 条目标识。
    pub memory_id: String,
    /// 类别。
    pub kind: &'static str,
    /// 为什么被隐藏。
    pub reason: &'static str,
}

/// 一次保留期执行的报告（§12.3 的"给用户完成状态"）。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RetentionReport {
    /// 本次隐藏的条目。
    pub tombstoned: Vec<TombstonedMemory>,
    /// 本次清理掉的条目数。
    pub purged: usize,
    /// 本次裁掉的审计条数。
    pub audit_pruned: usize,
    /// 当前仍然等着清理的条目数。
    ///
    /// 有了它，界面才能说清"删除已完成"与"删除还在排队"的区别——而不是把两者都显示成
    /// 一个勾。
    pub awaiting_purge: usize,
}

impl RetentionReport {
    /// 把另一次执行的报告并进来。
    pub fn merge(&mut self, other: RetentionReport) {
        self.tombstoned.extend(other.tombstoned);
        self.purged = self.purged.saturating_add(other.purged);
        self.audit_pruned = self.audit_pruned.saturating_add(other.audit_pruned);
        self.awaiting_purge = other.awaiting_purge;
    }

    /// 本次是否什么都没做。
    pub fn is_empty(&self) -> bool {
        self.tombstoned.is_empty() && self.purged == 0 && self.audit_pruned == 0
    }
}

/// §12.3 的第一步：让超过保留期的记忆**立即不可见**。
///
/// 到期**不等于**已经删除（§12.3 明确要求删除走 tombstone 流程），所以本函数只回答
/// "哪些该隐藏了"，隐藏之后它们就从所有检索里消失了，而内容还在原处等着下一步。
///
/// 它是**存储级**的，不按所有者分工：§12.3 的保留期是一条系统策略，而按所有者各清各的，
/// 会让一个不再活跃的所有者的过期数据永远留着——而那恰好是最该被清掉的那一类。
pub fn expire_retained(store: &mut Store, at: WallClock) -> Result<RetentionReport, CoreError> {
    let expired = store.expired_memories(at)?;
    let mut tombstoned = Vec::new();

    for entry in &expired {
        store.tombstone(&entry.memory_id, "retention_expired", at)?;
        tombstoned.push(TombstonedMemory {
            memory_id: entry.memory_id.to_string(),
            kind: entry.kind.as_str(),
            reason: "retention_expired",
        });
    }

    if !tombstoned.is_empty() {
        // 审计只记"做了什么、几条"，不记内容——同一条理由：审计账不是存放个人内容的地方。
        store.audit(
            at,
            AuditCategory::RetentionEnforced,
            "memory",
            "expired",
            &format!("保留期到期，已隐藏 {} 条记忆", tombstoned.len()),
        )?;
    }

    Ok(RetentionReport {
        tombstoned,
        purged: 0,
        audit_pruned: 0,
        awaiting_purge: store.tombstoned_memory_count()?,
    })
}

/// §12.3 的第二步：把已经不可见的内容真正清掉，并按策略裁掉超期的审计账。
///
/// 清理**不区分**是谁隐藏的：保留期到期的、用户手删的、证据撤回传导过来的，都在这时候
/// 一起走。给它们分别安排清理时机没有好处——它们都已经不可见了，而"什么时候清干净"
/// 对用户而言是同一个问题。
pub fn purge_retained(
    store: &mut Store,
    policy: &RetentionPolicy,
    at: WallClock,
) -> Result<RetentionReport, CoreError> {
    let purged = store.purge_tombstoned()?;

    let audit_pruned = if policy.prune_audit {
        let cutoff = at.plus_seconds(-policy.audit_retention_days.saturating_mul(86_400));
        store.prune_audit_before(cutoff)?
    } else {
        0
    };

    if purged > 0 || audit_pruned > 0 {
        store.audit(
            at,
            AuditCategory::RetentionEnforced,
            "memory",
            "purged",
            &format!("清理 {purged} 条已隐藏记忆、{audit_pruned} 条超期审计"),
        )?;
    }

    Ok(RetentionReport {
        tombstoned: Vec::new(),
        purged,
        audit_pruned,
        awaiting_purge: store.tombstoned_memory_count()?,
    })
}

/// §12.3 的用户入口："用户可随时删除"。
///
/// 只做**隐藏**那一步。清理走 [`purge_retained`]：用户点"删除"之后应当立刻看到"已经删除"，
/// 而把物理清理也塞进这个调用，会让界面卡在一次可能很慢的传播上——§12.3 要的是
/// "**异步**清理，并给用户完成状态"，两件事本来就该分开。
///
/// 三种情况分得很清楚，因为把它们合成一种都会给出错误的完成状态：
///
/// * **还在等清理**的条目重复删除 → 幂等成功，报告里 `tombstoned` 为空。用户看到失败提示
///   会以为没删掉，然后点第二次——而此时报错只会把他困在一个"删不掉"的界面上。
/// * **从未存在**的标识 → `MemoryNotFound`。用户可能填错了名字，静默成功会让他以为
///   删掉了某个东西。
/// * **已经清理掉**的条目 → 也是 `MemoryNotFound`，因为它连审计入口都读不到了。系统无从
///   判断它被删过还是从来不存在，而把两种情况都说成"已删除"，是在替一个它不掌握的事实作证。
pub fn forget(
    store: &mut Store,
    memory_id: &MemoryId,
    reason: &str,
    at: WallClock,
) -> Result<RetentionReport, CoreError> {
    let Some(entry) = store.memory_including_hidden(memory_id)? else {
        return Err(StorageError::MemoryNotFound {
            memory_id: memory_id.to_string(),
        }
        .into());
    };

    let mut tombstoned = Vec::new();
    if entry.status.is_visible() {
        store.tombstone(memory_id, reason, at)?;
        tombstoned.push(TombstonedMemory {
            memory_id: memory_id.to_string(),
            kind: entry.kind.as_str(),
            reason: "user_requested",
        });
        store.audit(
            at,
            AuditCategory::RetentionEnforced,
            memory_id.as_str(),
            "forgotten",
            "用户请求删除",
        )?;
    }

    Ok(RetentionReport {
        tombstoned,
        purged: 0,
        audit_pruned: 0,
        awaiting_purge: store.tombstoned_memory_count()?,
    })
}

/// 每个类别当前的保留期，供界面展示"这条什么时候会自动消失"。
pub fn default_retention_days(kind: MemoryKind) -> Option<i64> {
    kind.default_retention_days()
}
