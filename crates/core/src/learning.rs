//! 从实际记录里收集保留任务集（§13.2）。
//!
//! §13.2 要求候选策略"经**回放与保留任务集检验**后版本化启用"。准入闸本身在契约层
//! （[`soca_contracts::learning`]）——那里只有规则，没有存储。本模块负责的是它的**输入**：
//! 从记录里挑出那些"事后已经知道答案"的结论。
//!
//! 一条结论的对错在记录里长什么样，是三件不同的事：
//!
//! | 记录里的事实 | 是不是"我们错了" |
//! |---|---|
//! | 用户按了「记错了」（`tombstone_reason = user_correction`） | **是**。这是本版唯一一个明确的"这条是错的"信号 |
//! | 用户主动删掉（`user_requested`） | 不是。他可能只是不想留着 |
//! | 保留期到期（`retention_expired`） | 不是 |
//! | 它引用的证据被撤回（`capability_revoked`） | 不是。结论没被证伪，是它的依据不能用了 |
//!
//! 最后一行值得多说一句：把"证据被撤回"当成"结论错了"很容易，因为两者都让结论消失。
//! 但那会把系统推向一个很糟的行为——**用户撤回一次权限，系统就以为自己做错了一件事，
//! 于是把门槛提上去**。撤回权限是用户的常规操作，不是一次纠错。
//!
//! 于是保留任务集里的"错案"只有一个来源：用户说过它错。这也是为什么这个模块是
//! [`crate::subject::Subject::correct`] 的**下游**——那条通路不建起来，学习就没有教材。
//!
//! 一条**寿命**上的限制，写在这里以免被当成没有：`purge_tombstoned` 会把已经删除的记忆行
//! 物理删掉（§12.3 的第二步），所以错案只在"删除之后、清理之前"这段窗口里读得到。
//! 那是 §12.3 的取舍（删除要真的删掉），不是这里的疏忽——但它意味着**学习要在清理之前发生**。

use soca_contracts::{HoldoutSet, RecordedConclusion, SubjectId, WallClock};
use soca_storage::Store;

use crate::error::CoreError;

/// 用户说"记错了"时写下的删除原因。
///
/// 与 [`crate::subject::Subject::correct`] 里那两处字面量共用同一个常量：写成两个字符串的话，
/// 有一天其中一个改了而另一个没改，**学习就再也读不到教材**——而那种失败是静默的，
/// 表现只是"系统一直学不到东西"。
pub const CORRECTION_REASON: &str = "user_correction";

/// 保留任务集里最多收多少条错案。
///
/// 有界不是怕内存（错案本来就少），是因为**它要参与比对**：一份几千条的历史会让检验本身
/// 变成一次可疑的操作，而"多久以前的事还算数"没有好答案。取一个能装下"最近做错的那些"的量。
pub const MAX_RECORDED_MISTAKES: usize = 256;

/// 从记录里收集保留任务集（§13.2）。
///
/// 只读。它不写任何表，也不改任何结论——§13.2 要的"不覆盖原证据"在这里是字面意义上的：
/// 收集出来的条目引用着记忆标识，从不改写它们。
pub fn holdout_from(
    store: &Store,
    owner: &SubjectId,
    at: WallClock,
) -> Result<HoldoutSet, CoreError> {
    let known_right = store
        .recall(owner, None, at)?
        .into_iter()
        .map(|entry| RecordedConclusion {
            memory_id: entry.memory_id.to_string(),
            evidence_count: entry.evidence_refs.len(),
            why: "仍然可见、没有被更正过".to_string(),
        })
        .collect();

    let known_wrong = store
        .tombstoned_memories(owner, MAX_RECORDED_MISTAKES)?
        .into_iter()
        .filter(|entry| entry.tombstone_reason.as_deref() == Some(CORRECTION_REASON))
        .map(|entry| RecordedConclusion {
            memory_id: entry.memory_id.to_string(),
            evidence_count: entry.evidence_refs.len(),
            why: "用户按了「记错了」".to_string(),
        })
        .collect();

    Ok(HoldoutSet {
        known_right,
        known_wrong,
    })
}
