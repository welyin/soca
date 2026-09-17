//! 冷热单元与生命周期（§9.1、§9.2）。
//!
//! §9.1 的分层是"**应用决定'谁该工作'，OS 决定'页此刻在哪'**"。本模块负责前半句：
//!
//! | 层 | 内容 | 本模块的落点 |
//! |---|---|---|
//! | 热单元 | 当前任务状态、必要证据、活动邮箱 | [`UnitRegistry::hot`] |
//! | 冷单元 | SSD 上的事务元数据与内容对象，**不保留活线程** | `Store` 的 `units` 表 |
//! | 模型层 | 大权重与适配器 | 不在本模块（模型网关负责） |
//!
//! 三条 §9.2 的原文要求在这里成为可测行为：
//!
//! 1. "收到相关事件时…合并同单元的并发唤醒请求" —— [`UnitRegistry::request_wake`] 去重。
//! 2. "读取快照和游标后的事件，完成版本迁移与权限重验后进入 READY" ——
//!    [`UnitRegistry::wake`] 在没有通过 [`WakePolicy`] 时拒绝唤醒，而不是加载完再发现。
//! 3. "存在不明副作用时由持久在线动作账继续核对，不以卸载单元'解决'它" ——
//!    [`UnitRegistry::checkpoint`] 把未决动作**移交出去**并如实报告，而不是丢掉。

use std::collections::{BTreeMap, BTreeSet};

use soca_contracts::{UnitId, UnitSnapshot, UnitState, WallClock};
use soca_storage::{StorageError, Store, StoredEvent};

use crate::error::CoreError;

/// 一次唤醒最多带回多少条历史事件。
///
/// 唤醒不该把整个事件日志拉进内存。超过这个量的部分留给路由器按需续读（§10.4：感知流可以
/// 合并过时状态，但不可静默丢弃用户命令、授权撤回、动作回执或审计提交）。
pub const WAKE_CATCH_UP_LIMIT: usize = 256;

/// 唤醒必须通过的门槛（§9.2 的"权限重验"）。
///
/// `Default` 是**全部拒绝**：空集合意味着"当前没有任何能力策略或模型画像被认可"。
/// 这与 §12.2 的失败关闭一致——忘记配置的后果是拒绝唤醒，而不是悄悄放行。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WakePolicy {
    /// 当前仍然有效的能力策略引用。
    pub allowed_capabilities: BTreeSet<String>,
    /// 当前可用的模型画像引用。
    pub available_model_profiles: BTreeSet<String>,
}

impl WakePolicy {
    /// 把给定的策略与画像全部标记为有效。
    pub fn allowing(
        capabilities: impl IntoIterator<Item = impl Into<String>>,
        profiles: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            allowed_capabilities: capabilities.into_iter().map(Into::into).collect(),
            available_model_profiles: profiles.into_iter().map(Into::into).collect(),
        }
    }
}

/// 唤醒结果。
#[derive(Debug, Clone, PartialEq)]
pub enum WakeOutcome {
    /// 已进入 READY，并带回游标之后的事件。
    Ready {
        /// 唤醒后的快照。
        snapshot: Box<UnitSnapshot>,
        /// 单元休眠期间错过的事件。归属过滤留给路由器，因为单元快照里记的是任务域而不是任务。
        missed_events: Vec<StoredEvent>,
    },
    /// 单元已经是热的，本次没有做任何事。
    AlreadyHot {
        /// 当前热态快照。
        snapshot: Box<UnitSnapshot>,
    },
    /// 拒绝唤醒。原因需要进入审计账。
    Refused {
        /// 拒绝原因。
        reason: String,
    },
}

impl WakeOutcome {
    /// 是否真正完成了冷启动。
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

/// 降温结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointOutcome {
    /// 移交出去、由在线动作账继续核对的未决动作。
    pub handed_over: Vec<String>,
    /// 降温后的快照。
    pub snapshot: Box<UnitSnapshot>,
}

/// 热单元驻留表与唤醒预算。
#[derive(Debug)]
pub struct UnitRegistry {
    hot: BTreeMap<String, UnitSnapshot>,
    queued_wakes: BTreeSet<String>,
    wake_concurrency: usize,
}

impl UnitRegistry {
    /// 建立注册表。`wake_concurrency` 是同时在加载中的冷单元上限（§10.4 默认 2）。
    pub fn new(wake_concurrency: usize) -> Self {
        Self {
            hot: BTreeMap::new(),
            queued_wakes: BTreeSet::new(),
            wake_concurrency: wake_concurrency.max(1),
        }
    }

    /// 热单元数量。
    pub fn hot_count(&self) -> usize {
        self.hot.len()
    }

    /// 按标识列出热单元。
    pub fn hot_ids(&self) -> Vec<&str> {
        self.hot.keys().map(String::as_str).collect()
    }

    /// 某单元是否是热的。
    pub fn is_hot(&self, unit_id: &UnitId) -> bool {
        self.hot.contains_key(unit_id.as_str())
    }

    /// 取得热态快照。
    pub fn hot(&self, unit_id: &UnitId) -> Option<&UnitSnapshot> {
        self.hot.get(unit_id.as_str())
    }

    /// 登记一次唤醒请求。
    ///
    /// 返回 `false` 表示同一单元的请求已经被合并（§9.2："合并同单元的并发唤醒请求"）。
    /// 这一步有意不做加载：加载要走预算，预算由 [`UnitRegistry::take_wake_batch`] 发。
    pub fn request_wake(&mut self, unit_id: &UnitId) -> bool {
        self.queued_wakes.insert(unit_id.to_string())
    }

    /// 待唤醒的单元，按标识排序。
    pub fn queued_wakes(&self) -> Vec<&str> {
        self.queued_wakes.iter().map(String::as_str).collect()
    }

    /// 取出本批允许加载的单元，数量不超过唤醒并发上限。
    pub fn take_wake_batch(&mut self) -> Vec<UnitId> {
        let batch: Vec<String> = self
            .queued_wakes
            .iter()
            .take(self.wake_concurrency)
            .cloned()
            .collect();
        for id in &batch {
            self.queued_wakes.remove(id);
        }
        batch
            .into_iter()
            .filter_map(|id| UnitId::new(id).ok())
            .collect()
    }

    /// 唤醒一个冷单元。
    pub fn wake(
        &mut self,
        store: &mut Store,
        unit_id: &UnitId,
        policy: &WakePolicy,
        at: WallClock,
    ) -> Result<WakeOutcome, CoreError> {
        if let Some(hot) = self.hot.get(unit_id.as_str()) {
            return Ok(WakeOutcome::AlreadyHot {
                snapshot: Box::new(hot.clone()),
            });
        }

        let Some(mut snapshot) = store.unit(unit_id)? else {
            return Err(StorageError::UnitNotFound {
                unit_id: unit_id.to_string(),
            }
            .into());
        };

        // 库里的状态不是 COLD，但热表里也没有它：上一次运行的残留。
        // 直接覆盖成 READY 会把"上一次的副作用是否结清"这个问题抹掉。
        if snapshot.state != UnitState::Cold {
            return Ok(WakeOutcome::Refused {
                reason: format!(
                    "目录中的状态是 {}，热表里却没有该单元；需先结清上一次运行，不能直接覆盖",
                    snapshot.state.as_str()
                ),
            });
        }

        // 权限重验。放在加载之前：§9.2 要求"完成版本迁移与权限重验后进入 READY"，
        // 而拒绝一个不该被唤醒的单元，比加载完再拒绝便宜得多。
        if !policy
            .allowed_capabilities
            .contains(snapshot.capability_policy_ref.as_str())
        {
            return Ok(WakeOutcome::Refused {
                reason: format!(
                    "能力策略 {} 已失效，拒绝唤醒",
                    snapshot.capability_policy_ref
                ),
            });
        }
        if !policy
            .available_model_profiles
            .contains(snapshot.model_profile_ref.as_str())
        {
            return Ok(WakeOutcome::Refused {
                reason: format!(
                    "模型画像 {} 当前不可用，拒绝唤醒",
                    snapshot.model_profile_ref
                ),
            });
        }

        // 版本迁移检查：快照 schema 与本程序不一致时 validate 会拒绝。
        snapshot.validate()?;

        snapshot.transition_to(UnitState::Loading)?;
        store.save_unit(&snapshot, at)?;

        // 游标之后的事件才是这个单元错过的世界。
        let missed_events =
            store.read_events_after(snapshot.last_applied_sequence as i64, WAKE_CATCH_UP_LIMIT)?;

        snapshot.transition_to(UnitState::Ready)?;
        store.save_unit(&snapshot, at)?;

        self.hot.insert(snapshot.unit_id.to_string(), snapshot.clone());
        self.queued_wakes.remove(unit_id.as_str());

        Ok(WakeOutcome::Ready {
            snapshot: Box::new(snapshot),
            missed_events,
        })
    }

    /// 把热单元迁到指定状态。
    ///
    /// 迁移合法性由契约层的状态机判定（§9.2 的状态图），这里只负责落库与热表同步。
    pub fn set_state(
        &mut self,
        store: &mut Store,
        unit_id: &UnitId,
        next: UnitState,
        at: WallClock,
    ) -> Result<(), CoreError> {
        let Some(snapshot) = self.hot.get_mut(unit_id.as_str()) else {
            return Err(StorageError::UnitNotFound {
                unit_id: unit_id.to_string(),
            }
            .into());
        };
        snapshot.transition_to(next)?;
        let persisted = snapshot.clone();
        store.save_unit(&persisted, at)?;
        Ok(())
    }

    /// 推进单元的事件游标。
    ///
    /// 游标只能向前。§7.3 的"可重放状态转移"要求重放不会把单元的记忆往回拨；一旦允许回退，
    /// 冷却期间的重复处理就再也无法判定。
    pub fn advance_cursor(
        &mut self,
        store: &mut Store,
        unit_id: &UnitId,
        sequence: u64,
        at: WallClock,
    ) -> Result<(), CoreError> {
        let Some(snapshot) = self.hot.get_mut(unit_id.as_str()) else {
            return Err(StorageError::UnitNotFound {
                unit_id: unit_id.to_string(),
            }
            .into());
        };
        if sequence < snapshot.last_applied_sequence {
            return Err(CoreError::CursorWentBackwards {
                unit_id: unit_id.to_string(),
                current: snapshot.last_applied_sequence,
                attempted: sequence,
            });
        }
        snapshot.last_applied_sequence = sequence;
        let persisted = snapshot.clone();
        store.save_unit(&persisted, at)?;
        Ok(())
    }

    /// 记下一个尚未了结的动作。
    ///
    /// §9.2 的降温步骤是"将 pending 动作、游标、状态和 outbox 事务提交"。因此单元把动作交给
    /// 执行代理之后，必须把它记进自己的快照。否则 [`UnitRegistry::checkpoint`] 的闸门永远为
    /// 空，"存在不明副作用时不能靠卸载单元解决"就成了一句空话。
    ///
    /// 重复登记同一动作是幂等的：快照不允许重复引用（§7.2 的证据去重原则同样适用于动作）。
    pub fn note_pending_action(
        &mut self,
        store: &mut Store,
        unit_id: &UnitId,
        action_id: &soca_contracts::ActionId,
        at: WallClock,
    ) -> Result<(), CoreError> {
        let Some(snapshot) = self.hot.get_mut(unit_id.as_str()) else {
            return Err(StorageError::UnitNotFound {
                unit_id: unit_id.to_string(),
            }
            .into());
        };
        if !snapshot.pending_action_ids.contains(action_id) {
            snapshot.pending_action_ids.push(action_id.clone());
        }
        let persisted = snapshot.clone();
        store.save_unit(&persisted, at)?;
        Ok(())
    }

    /// 了结一个动作，把它从待处理集合里去掉。
    pub fn resolve_pending_action(
        &mut self,
        store: &mut Store,
        unit_id: &UnitId,
        action_id: &soca_contracts::ActionId,
        at: WallClock,
    ) -> Result<(), CoreError> {
        let Some(snapshot) = self.hot.get_mut(unit_id.as_str()) else {
            return Err(StorageError::UnitNotFound {
                unit_id: unit_id.to_string(),
            }
            .into());
        };
        snapshot.pending_action_ids.retain(|id| id != action_id);
        let persisted = snapshot.clone();
        store.save_unit(&persisted, at)?;
        Ok(())
    }

    /// 降温一个单元。
    ///
    /// 未决动作会被**移交**出去并如实报告（§9.2），而不是被丢掉。移交的对象是持久在线动作账
    /// ——即 `Store` 里的 `actions` 表；单元降温之后，核对这些动作的责任落在恢复流程上。
    pub fn checkpoint(
        &mut self,
        store: &mut Store,
        unit_id: &UnitId,
        at: WallClock,
    ) -> Result<CheckpointOutcome, CoreError> {
        let Some(mut snapshot) = self.hot.get(unit_id.as_str()).cloned() else {
            return Err(CoreError::UnitNotHot {
                unit_id: unit_id.to_string(),
            });
        };

        snapshot.transition_to(UnitState::Checkpointing)?;
        let handed_over: Vec<String> = snapshot
            .drain_pending_for_action_ledger()
            .iter()
            .map(ToString::to_string)
            .collect();
        // 到了这一步 pending 已经清空，COLD 迁移不会再被闸门挡住。
        snapshot.transition_to(UnitState::Cold)?;

        store.save_unit(&snapshot, at)?;
        self.hot.remove(unit_id.as_str());

        Ok(CheckpointOutcome {
            handed_over,
            snapshot: Box::new(snapshot),
        })
    }
}
