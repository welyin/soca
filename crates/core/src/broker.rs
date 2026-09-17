//! 独立权限与执行代理（§12.2）。
//!
//! 本模块是副作用的**唯一出口**。§12.1 的默认不是"能访问底层就能执行所有底层动作"，
//! 因此执行代理的入口签名刻意收成一条：
//!
//! ```text
//! submit(trigger: &DataState, ...)
//! ```
//!
//! 它不接收 [`soca_contracts::ActionIntent`] 就直接执行，而是要求调用方先拿出一个
//! [`soca_contracts::ExecutionPermit`] 并包成 `DataState`。传入其余六类数据状态会得到
//! [`BrokerError::NotAnExecutionPermit`]——也就是说，§7.2 的"只有执行许可可以触发副作用"
//! 在这里不是一个需要自觉遵守的约定，而是唯一能走通的那条路。
//!
//! 三条本模块落实的规则：
//!
//! * **许可绑定具体动作**：`tool_id`、`object_scope`、参数摘要、风险等级、TTL、次数，
//!   任何一项不符即拒绝（由 [`ExecutionPermit::authorizes`] 判定）。
//! * **失败关闭**：代理不可用、许可过期、参数被改，默认拒绝而不是放行（§12.2 末段）。
//! * **结果未知不等于失败**：断电返回 [`BrokerOutcome::Interrupted`]，调用方不得重发。

use soca_contracts::{
    ActionIntent, ActionReceipt, CommitStatus, DataState, DataStateKind, ExecutionPermit, WallClock,
};

use crate::os::{AttemptOutcome, SimulatedOs};

/// 执行代理的判定结果。
#[derive(Debug, Clone, PartialEq)]
pub enum BrokerOutcome {
    /// 已执行并拿到回执。回执不等于后置条件已验证（§7.2）。
    Receipted(Box<ActionReceipt>),
    /// 副作用已发生，但结果未知。调用方**不得**重发，必须走恢复流程核对目标状态。
    Interrupted {
        /// 动作标识。
        action_id: String,
    },
    /// 未执行。拒绝原因需要进入审计账（§6.8）。
    Refused {
        /// 拒绝原因。
        reason: String,
    },
}

impl BrokerOutcome {
    /// 是否真的产生了副作用。
    pub fn did_apply(&self) -> bool {
        matches!(self, Self::Receipted(_) | Self::Interrupted { .. })
    }
}

/// 执行代理的编程错误。
///
/// 许可校验失败不算错误——那是一条正常的拒绝路径，走 [`BrokerOutcome::Refused`] 并留痕。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    /// 传入的数据状态不是执行许可。
    #[error("只有 ExecutionPermit 可以触发副作用，传入的是 {actual:?}（§7.2）")]
    NotAnExecutionPermit {
        /// 实际传入的状态种类。
        actual: DataStateKind,
    },
}

/// 执行代理。
#[derive(Debug)]
pub struct ActionBroker {
    os: SimulatedOs,
    /// `Some(reason)` 表示代理不可用。§12.2 要求此时默认拒绝新副作用。
    unavailable: Option<String>,
}

impl ActionBroker {
    /// 用一个模拟环境构造代理。
    pub fn new(os: SimulatedOs) -> Self {
        Self {
            os,
            unavailable: None,
        }
    }

    /// 借用底层环境。
    pub fn os(&self) -> &SimulatedOs {
        &self.os
    }

    /// 可变借用底层环境。给传感器侧读取世界用，不给执行用。
    pub fn os_mut(&mut self) -> &mut SimulatedOs {
        &mut self.os
    }

    /// 让代理进入不可用状态。对应 §12.2 的"Broker 故障、策略不可读、审计写失败、磁盘满
    /// 或审批过期"。
    pub fn set_unavailable(&mut self, reason: impl Into<String>) {
        self.unavailable = Some(reason.into());
    }

    /// 恢复可用。
    pub fn set_available(&mut self) {
        self.unavailable = None;
    }

    /// 代理当前是否可用。
    pub fn is_available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// 提交一次动作。
    ///
    /// 代理不持有存储句柄：它既不依赖数据库，也不会在拒绝时留下半截写入。
    ///
    /// 这里做的是 [`ExecutionPermit::revalidate`] 而不是 `authorizes`：许可的次数已经在
    /// 受理时原子扣减，交接环节再判一次会把同一个动作算两次。交接需要拦的是另外两件事——
    /// 许可是否已经过期、参数是否在受理之后被改过。
    pub fn submit(
        &mut self,
        trigger: &DataState,
        intent: &ActionIntent,
        at: WallClock,
    ) -> Result<BrokerOutcome, BrokerError> {
        // 1. 失败关闭优先于一切。
        if let Some(reason) = &self.unavailable {
            return Ok(BrokerOutcome::Refused {
                reason: format!("执行代理不可用：{reason}"),
            });
        }

        // 2. 只有执行许可能往下走。其余六类数据状态在这里被挡住。
        let DataState::ExecutionPermit(permit) = trigger else {
            return Err(BrokerError::NotAnExecutionPermit {
                actual: trigger.kind(),
            });
        };

        // 3. 许可必须仍然绑定这次具体动作，且未过期。
        if let Err(reason) = permit.revalidate(intent, at) {
            return Ok(BrokerOutcome::Refused {
                reason: reason.to_string(),
            });
        }

        // 4. 执行。
        Ok(match self.os.execute(intent) {
            AttemptOutcome::Applied { version } => {
                BrokerOutcome::Receipted(Box::new(receipt(
                    intent,
                    permit,
                    CommitStatus::Completed,
                    at,
                    "副作用已应用",
                    Some(version),
                )))
            }
            AttemptOutcome::AlreadyApplied { version } => BrokerOutcome::Receipted(Box::new(
                receipt(
                    intent,
                    permit,
                    CommitStatus::Completed,
                    at,
                    "该动作此前已应用，本次未重复应用",
                    Some(version),
                ),
            )),
            AttemptOutcome::Failed { reason } => BrokerOutcome::Receipted(Box::new(receipt(
                intent,
                permit,
                CommitStatus::Failed,
                at,
                &reason,
                None,
            ))),
            // 关键：断电不产生回执。副作用是否发生只有核对目标状态才知道。
            AttemptOutcome::Interrupted { .. } => BrokerOutcome::Interrupted {
                action_id: intent.action_id.to_string(),
            },
        })
    }
}

fn receipt(
    intent: &ActionIntent,
    permit: &ExecutionPermit,
    status: CommitStatus,
    at: WallClock,
    detail: &str,
    observed_target_version: Option<String>,
) -> ActionReceipt {
    ActionReceipt {
        action_id: intent.action_id.clone(),
        permit_id: permit.permit_id.clone(),
        status,
        recorded_at: at,
        detail: detail.to_string(),
        observed_target_version,
    }
}
