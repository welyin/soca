//! 轨迹重放（§7.3："历史重放不重新执行外部动作，只读取已记录回执"）。
//!
//! 本模块的结构就是它对 §7.3 的承诺：
//!
//! ```text
//! Trajectory::load(store: &Store, task_id: &TaskId) -> Result<Trajectory, CoreError>
//! ```
//!
//! 参数里**没有执行代理、也没有模拟 OS**。重放拿不到任何能改变世界的句柄，所以"重放会不会
//! 重新执行外部动作"不是一个需要靠自觉遵守的约定，而是签名层面就不可能发生的事。
//! 与之对应的回归测试会重置模拟 OS 的尝试计数，然后断言重放前后计数不变。
//!
//! 重放不只是把记录读出来，还要判断这份记录**是否自洽**。§17 要求"公开报告失败、超时、
//! 拒绝和遗漏"，[`ReplayViolation`] 就是"遗漏"的机器可读形式。

use soca_contracts::{ActionReceipt, OutcomeVerified, TaskId};
use soca_storage::{
    ActionRecord, ActionState, Decision, PredictionRecord, Store, StoredEvent,
};

use crate::error::CoreError;

/// 一个动作在账上的完整轨迹。
#[derive(Debug, Clone, PartialEq)]
pub struct ActionTrajectory {
    /// 动作账记录。
    pub record: ActionRecord,
    /// 投递时刻。`None` 表示从未投递。
    pub dispatched_at: Option<soca_contracts::WallClock>,
    /// 动作前预测。
    pub prediction: Option<PredictionRecord>,
    /// 执行回执。
    pub receipt: Option<ActionReceipt>,
    /// 后置条件判定。
    pub outcome: Option<OutcomeVerified>,
}

impl ActionTrajectory {
    /// 这次动作是否可能已经改变了世界。
    pub fn may_have_changed_the_world(&self) -> bool {
        self.dispatched_at.is_some()
    }
}

/// 一次任务的全部记录。
#[derive(Debug, Clone, PartialEq)]
pub struct Trajectory {
    /// 任务标识。
    pub task_id: TaskId,
    /// 该任务下的观测事件，按提交序升序。
    pub observations: Vec<StoredEvent>,
    /// 该任务下的动作前预测。
    pub predictions: Vec<PredictionRecord>,
    /// 该任务下的动作，按创建顺序升序。
    pub actions: Vec<ActionTrajectory>,
}

/// 重放发现的自洽性问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayViolation {
    /// 放行的动作找不到动作前预测。
    AllowedActionWithoutPrediction {
        /// 动作标识。
        action_id: String,
    },
    /// 动作引用的预测属于另一个任务。
    PredictionTaskMismatch {
        /// 动作标识。
        action_id: String,
        /// 预测实际所属的任务。
        prediction_task: String,
    },
    /// 放行的动作，其预测没有失败条件，因此无法证伪。
    UnfalsifiablePrediction {
        /// 动作标识。
        action_id: String,
        /// 预测引用。
        prediction_ref: String,
    },
    /// 有回执，但账上没有任何投递记录。
    ExecutedWithoutDispatch {
        /// 动作标识。
        action_id: String,
    },
    /// 有后置条件判定，但没有回执。
    OutcomeWithoutReceipt {
        /// 动作标识。
        action_id: String,
    },
    /// 被拒绝的动作却留下了执行回执。
    DeniedActionHasReceipt {
        /// 动作标识。
        action_id: String,
    },
    /// 未知提交没有被结清，也没有回执。
    UnresolvedUnknownCommit {
        /// 动作标识。
        action_id: String,
    },
}

impl Trajectory {
    /// 从存储读出一份轨迹。
    ///
    /// 只读。返回 [`Trajectory`] 的过程不会写入任何表，也不会触碰环境。
    pub fn load(store: &Store, task_id: &TaskId, limit: usize) -> Result<Self, CoreError> {
        let observations = store.events_for_task(task_id, limit)?;
        let predictions = store.predictions_for_task(task_id)?;

        let records = store.actions_for_task(task_id, limit)?;
        let mut actions = Vec::with_capacity(records.len());
        for record in records {
            let dispatched_at = store.dispatched_at(&record.action_id)?;
            let receipt = store.receipt(&record.action_id)?;
            let outcome = store.outcome(&record.action_id)?;
            let prediction = store.prediction(&record.prediction_ref)?;
            actions.push(ActionTrajectory {
                record,
                dispatched_at,
                prediction,
                receipt,
                outcome,
            });
        }

        Ok(Self {
            task_id: task_id.clone(),
            observations,
            predictions,
            actions,
        })
    }

    /// 动作总数。
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }

    /// 检查轨迹是否自洽。空列表表示通过。
    pub fn violations(&self) -> Vec<ReplayViolation> {
        let mut found = Vec::new();

        for action in &self.actions {
            let action_id = action.record.action_id.clone();
            let allowed = action.record.decision == Decision::Allowed;

            if allowed {
                match &action.prediction {
                    None => {
                        found.push(ReplayViolation::AllowedActionWithoutPrediction {
                            action_id: action_id.clone(),
                        });
                    }
                    Some(record) => {
                        if record.task_id != self.task_id.to_string() {
                            found.push(ReplayViolation::PredictionTaskMismatch {
                                action_id: action_id.clone(),
                                prediction_task: record.task_id.clone(),
                            });
                        }
                        if record.prediction.failure_conditions.is_empty() {
                            found.push(ReplayViolation::UnfalsifiablePrediction {
                                action_id: action_id.clone(),
                                prediction_ref: record.prediction.prediction_ref.to_string(),
                            });
                        }
                    }
                }
            }

            if action.receipt.is_some() && action.dispatched_at.is_none() {
                found.push(ReplayViolation::ExecutedWithoutDispatch {
                    action_id: action_id.clone(),
                });
            }

            if action.outcome.is_some() && action.receipt.is_none() {
                found.push(ReplayViolation::OutcomeWithoutReceipt {
                    action_id: action_id.clone(),
                });
            }

            if !allowed && action.receipt.is_some() {
                found.push(ReplayViolation::DeniedActionHasReceipt {
                    action_id: action_id.clone(),
                });
            }

            // 结果未知且没有回执：这是允许的状态（等待恢复任务核对），但它必须仍然是
            // UNKNOWN_COMMIT，不能被当成已完成。
            if action.record.state == ActionState::UnknownCommit && action.receipt.is_some() {
                found.push(ReplayViolation::UnresolvedUnknownCommit { action_id });
            }
        }

        found
    }

    /// 轨迹是否通过全部自洽性检查。
    pub fn is_consistent(&self) -> bool {
        self.violations().is_empty()
    }

    /// 该任务是否曾经产生过副作用。
    pub fn touched_the_world(&self) -> bool {
        self.actions
            .iter()
            .any(ActionTrajectory::may_have_changed_the_world)
    }
}
