//! 拓扑迁移事务的执行（§6.2、§6.3；§17 的"拓扑数量"那一行）。
//!
//! §17 那一行是：
//!
//! > 同需求异资源输出不同叶/簇/协调数；**迁移有 epoch、fencing、状态恢复和回滚**，
//! > **主体身份不因压力被合并**。
//!
//! 前半句由 [`soca_core_topology`] 负责，它全套都在、也有测试。后半句的三样里有**两样也
//! 已经在了**，只是没人调：
//!
//! * **契约**：`soca_contracts::instance` 里有 `ScaleTransactionState` 的十档状态机、
//!   `SubjectRoute` 的世代、以及"新世代必须晚于旧世代"的校验。
//! * **存储**：`soca_storage` 里有 `open_scale_transaction`、`advance_scale_transaction`、
//!   以及 `commit_route` 的 CAS（"两个迁移同时提交会让旧 actor 继续持有可执行所有权"）。
//!
//! 缺的是**把它们按顺序跑一遍并处理失败**的东西——本模块。这是第四次"机制完整、线没接"，
//! 而这一次缺的是一个整层（`core-reconciler`）。
//!
//! ## 提交点，以及为什么回滚是有边界的
//!
//! ```text
//! PLANNED → RESERVED → DRAINING → SNAPSHOTTED → SHADOW_READY → ROUTE_COMMITTED → RETIRING → DONE
//!    └──────────┴───────────┴─────────────┴──────────────┘
//!                    RolledBack（只在提交点之前可达）
//!                                            ROUTE_COMMITTED 之后：只能 → RECOVERING → DONE
//! ```
//!
//! "提交点之后不能回滚"不是一条纪律，是 §6.3 的**类型约束**：`TopologyEpoch` 只能往上走
//! ——"需要回退时创建更大的世代，不能重新启用旧 fencing token"。所以提交之后回滚是
//! **做不到**的，而不是"不该做"；能做的只有补偿，而补偿会把事务推到 `DONE`，不是推回旧世代。
//!
//! ## 两段式，而不是一个大函数
//!
//! [`begin_scaling`] 走到 `DRAINING` 就交给调用方；调用方做完"把状态搬过去"这件事之后，
//! 再调 [`commit_scaling`]。分成两段是因为**中间那一步只有调用方会做**：本模块管顺序与
//! 世代的提交，而"搬哪些状态"是主体自己的事（对它来说就是降温再唤醒）。
//!
//! 写成"传一个回调进来"也能work，但那会让主体的一部分借用 `&mut Store`、另一部分借用
//! `&mut Subject`——而后者本身就握着前者。两段式把那个借用冲突消掉了，顺带让每一步都能被
//! 单独测试。

use serde::Serialize;
use soca_contracts::{
    BlobRef, ReservationId, ScaleTransaction, ScaleTransactionState, SubjectId, SubjectRoute,
    TopologyEpoch, TopologyPlan, TransactionId, WallClock,
};
use soca_storage::Store;

use crate::error::CoreError;

/// 一次迁移的结局（§6.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScaleRun {
    /// 跑完了。路由已经切到新世代。
    Committed {
        /// 事务标识。**三档都带上它**：出了事要能照着它去查事务表，
        /// 而从"哪一次迁移"到"事务表里的哪一行"之间不该有一步猜测。
        transaction_id: String,
        /// 切换前的世代。
        old_epoch: u64,
        /// 切换后的世代。
        new_epoch: u64,
    },
    /// 提交点**之前**失败，已经回滚。旧世代继续用。
    RolledBack {
        /// 事务标识。
        transaction_id: String,
        /// 为什么没成。给人读。
        reason: String,
        /// 走到哪一档才发现不行——它决定了要清理到什么地方。
        reached: &'static str,
    },
    /// 提交点**之后**失败。**回不去了**：只能补偿到 `DONE`。
    ///
    /// 这一档单独列出来，是因为它与上一档对操作员的意义完全不同：回滚了是"什么也没发生，
    /// 再来一次"；而这一档是"新世代已经在跑，去把它查清楚"。
    Recovering {
        /// 事务标识。
        transaction_id: String,
        /// 为什么没成。
        reason: String,
    },
}

impl ScaleRun {
    /// 是否真的切换了世代。
    pub fn committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}

/// 开一次迁移，并推进到 `DRAINING`（§6.2 的"已停止新租约，持久邮箱仍收件"）。
///
/// 走到 `DRAINING` 就停下：再往前一步是"快照已单事务写入"，而那件事**只有调用方做得了**
/// ——它才知道要写什么。
///
/// 三道闸在动手之前依次过一遍，因为它们各自都能让"跑一半"变成不必要的：
///
/// 1. **主体身份不许改**（§17 的"主体身份不因压力被合并"）。计划里的主体数必须与现在一致——
///    压力下的自动合并会把两个主体的记忆域并成一个，而那是一次**不可逆**的信息损失。
/// 2. **计划必须可行**。`PAUSED` 的计划意味着这台机器现在承载不了那个档位；照它迁移
///    等于把一次"还没准备好"变成一次"已经在跑"。这一档归入回滚。
/// 3. **新世代必须是真的新的**。它由当前路由派生，而不是由调用方指定——让调用方指定的话，
///    "回退"就有了一条路：填一个更小的数。而那正是 §6.3 明令不许的。
pub fn begin_scaling(
    store: &mut Store,
    subject_id: &SubjectId,
    plan: &TopologyPlan,
    at: WallClock,
) -> Result<ScaleTransaction, CoreError> {
    // 三道闸**在登记任何东西之前**过一遍。一次被拒的迁移不该留下痕迹——
    // 留下一条路由或一行事务记录，会让"这次到底有没有试图迁移过"多一个要解释的东西，
    // 而它的答案（"试图过，但没开始"）本该什么都不留下。
    //
    // 一、主体身份。
    if plan.subjects != 1 {
        return Err(CoreError::ScaleRefused {
            reason: format!(
                "计划里有 {} 个主体，而迁移不许改变主体数（§17：主体身份不因压力被合并），\
                 当前是 1 个",
                plan.subjects
            ),
        });
    }

    // 二、计划得是可行的。
    if plan.state != soca_contracts::PlanState::Running {
        return Err(CoreError::ScaleRefused {
            reason: format!(
                "计划是暂停的（{}）；照一份暂停的计划迁移，等于把「还没准备好」\
                 变成「已经在跑」",
                plan.reason.as_str()
            ),
        });
    }

    // 三、世代由当前路由派生。**不是由调用方指定的**——让调用方指定的话，"回退"就有了
    // 一条路：填一个更小的数。而那正是 §6.3 明令不许的。
    let route = store.ensure_route(subject_id, &graph_ref(plan), at)?;
    let new_epoch = route.current_epoch.next()?;

    let transaction = ScaleTransaction {
        transaction_id: TransactionId::new(format!(
            "transaction:{}:{}",
            subject_id, new_epoch
        ))?,
        subject_id: subject_id.clone(),
        old_epoch: route.current_epoch,
        new_epoch,
        state: ScaleTransactionState::Planned,
        plan: *plan,
        resource_reservation_id: ReservationId::new(format!("reservation:{new_epoch}"))?,
        deadline: at.plus_seconds(SCALE_DEADLINE_SECONDS),
    };
    store.open_scale_transaction(&transaction, at)?;

    // `RESERVED` 是 §6.2 的"峰值资源已预留。失败不启动迁移"。
    //
    // 本版没有资源预约表（`ReservationId` 只登记在事务里），所以这一步的实质是**再判一次
    // 计划可行**——在单机单主体里，"预留"与"算得出放得下"是同一件事。这一点写在这里，
    // 免得读的人以为背后有一个资源代理。
    let mut transaction = transaction;
    advance(store, &mut transaction, ScaleTransactionState::Reserved, at)?;
    advance(store, &mut transaction, ScaleTransactionState::Draining, at)?;
    Ok(transaction)
}

/// 调用方搬完状态之后，把它提交掉（§6.2 的 `SNAPSHOTTED` → … → `DONE`）。
///
/// `state_moved` 是调用方对"**快照已落库、影子已恢复**"这两档的断言。**它不由本模块去
/// 验证**——本模块看不见单元的内部状态。这个参数的作用是让那一步在**调用处**留下名字，
/// 而不是让它悄悄滑过去。
///
/// 一处诚实的缺口：§6.2 的 `SHADOW_READY` 是"影子已恢复，**只读**"，而本版恢复出来的
/// 单元是可以写的——"只读"要一层额外的闸，而那一层还没有。所以这一档在本版里的实际含义
/// 是"影子起来了"，不含只读。
pub fn commit_scaling(
    store: &mut Store,
    transaction: &ScaleTransaction,
    state_moved: bool,
    at: WallClock,
) -> Result<ScaleRun, CoreError> {
    if !state_moved {
        return rollback_scaling(store, transaction, "状态没有搬过去", at);
    }

    // 事务在手里**始终可变**，不在各步之间传来传去：传值的那一版在错误路径上会把它
    // 移到 `advance` 里去，于是"失败了要拿它去回滚"这一步就用不上它了。
    let mut transaction = transaction.clone();

    // 提交点之前的最后两档。任何一步失败都能回滚。
    for next in [
        ScaleTransactionState::Snapshotted,
        ScaleTransactionState::ShadowReady,
    ] {
        if let Err(error) = advance(store, &mut transaction, next, at) {
            return rollback_scaling(store, &transaction, &error.to_string(), at);
        }
    }

    // —— 提交点 ——
    //
    // 路由切换用 CAS，`expected` 是事务里记的旧世代。**不做"以新压旧"的宽容处理**：
    // 两个迁移同时提交会让旧 actor 继续持有可执行所有权（§6.2）。
    //
    // 这一行之后就没有回头路了：`new_epoch` 已经生效，而世代只能往上走。
    if let Err(error) = store.commit_route(
        &transaction.subject_id,
        transaction.old_epoch,
        transaction.new_epoch,
        &graph_ref(&transaction.plan),
        at,
    ) {
        // CAS 失败有两种：另一个迁移先提交了（世代对不上），或者路由不见了。
        // 两者都发生在**提交点之前**——路由没换，我们还站在旧世代上。
        return rollback_scaling(store, &transaction, &error.to_string(), at);
    }

    for next in [
        ScaleTransactionState::RouteCommitted,
        ScaleTransactionState::Retiring,
        ScaleTransactionState::Done,
    ] {
        if let Err(error) = advance(store, &mut transaction, next, at) {
            // 路由**已经切了**，而事务表没跟上。这是提交点之后的失败：回不去，
            // 只能补到 `DONE`。
            return recover(store, &transaction, &error.to_string(), at);
        }
    }

    Ok(ScaleRun::Committed {
        transaction_id: transaction.transaction_id.to_string(),
        old_epoch: transaction.old_epoch.get(),
        new_epoch: transaction.new_epoch.get(),
    })
}

/// 提交点之前失败：**回滚**。旧世代继续用，什么也没换。
pub fn rollback_scaling(
    store: &mut Store,
    transaction: &ScaleTransaction,
    reason: &str,
    at: WallClock,
) -> Result<ScaleRun, CoreError> {
    // 从**库里**读它现在停在哪一档，而不是用手上那份内存副本：两者不一致时，
    // 该信的是库里那份，而报告要说的是"实际走到了哪"。
    let stopped_at = store
        .scale_transaction(&transaction.transaction_id)?
        .map_or(transaction.state, |current| current.state);
    store.advance_scale_transaction(
        &transaction.transaction_id,
        ScaleTransactionState::RolledBack,
        at,
    )?;
    Ok(ScaleRun::RolledBack {
        transaction_id: transaction.transaction_id.to_string(),
        reason: reason.to_string(),
        reached: stopped_at.as_str(),
    })
}

/// 提交点之后失败：**补偿**。不能回滚，只能把事务推到 `DONE` 并如实报出来。
///
/// 报成 `Recovering` 而不是 `Committed`，因为调用方要做的事完全不同：前者是"去查清楚
/// 新世代在跑什么"，后者是"继续"。合成一件事，会让一次半途而废看起来像一次成功。
fn recover(
    store: &mut Store,
    transaction: &ScaleTransaction,
    reason: &str,
    at: WallClock,
) -> Result<ScaleRun, CoreError> {
    store.advance_scale_transaction(
        &transaction.transaction_id,
        ScaleTransactionState::Recovering,
        at,
    )?;
    store.advance_scale_transaction(
        &transaction.transaction_id,
        ScaleTransactionState::Done,
        at,
    )?;
    Ok(ScaleRun::Recovering {
        transaction_id: transaction.transaction_id.to_string(),
        reason: reason.to_string(),
    })
}

/// 当前路由。没有登记过时返回 `None`。
pub fn current_route(
    store: &Store,
    subject_id: &SubjectId,
) -> Result<Option<SubjectRoute>, CoreError> {
    Ok(store.route(subject_id)?)
}

/// 计划对应的拓扑图引用。
///
/// 由计划的**内容**派生，而不是随机生成：同一份计划永远得到同一个引用，
/// 于是"这两个世代用的是不是同一张图"是一个能直接比的问题。
fn graph_ref(plan: &TopologyPlan) -> BlobRef {
    use soca_contracts::Sha256Hex;

    // 序列化**整份计划**再取摘要，而不是挑几个字段拼一拼：挑字段的话，将来给 `TopologyPlan`
    // 加一个影响拓扑的字段而忘了加进这段拼接，会得到**两种图、同一个引用**——
    // 而那正是"这两个世代用的是不是同一张图"要回答的问题。
    let encoded = serde_json::to_vec(plan).unwrap_or_default();
    let digest = Sha256Hex::of_bytes(&encoded);
    let hex = digest.as_str();
    let head = &hex[..32.min(hex.len())];
    BlobRef::new(format!("blob:topology-{head}")).expect("摘要定长，必然合法")
}

/// 推进一档。内存里的副本跟着一起走，好让失败路径上还能拿它去回滚。
fn advance(
    store: &mut Store,
    transaction: &mut ScaleTransaction,
    next: ScaleTransactionState,
    at: WallClock,
) -> Result<(), CoreError> {
    store.advance_scale_transaction(&transaction.transaction_id, next, at)?;
    transaction.state = next;
    Ok(())
}

/// 一次迁移事务的存活上限（秒）。
///
/// §6.2 说"到期**不是**自动杀死有未决副作用的 Broker"——所以这个期限不触发任何杀进程的
/// 动作，它只是一个"该有人来看一眼了"的标记。把它当超时用，会让一次迁移在半路上被砍掉，
/// 而那时它可能已经改过世界了。
const SCALE_DEADLINE_SECONDS: i64 = 300;

/// 把一个世代写成一句人能读的话。
pub fn describe_epoch(epoch: TopologyEpoch) -> String {
    format!("拓扑世代 {epoch}")
}
