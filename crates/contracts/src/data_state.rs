//! §7.2 的七类数据状态。
//!
//! 本模块把"数据状态不偷换"变成一条可判定规则：**七类状态里只有 [`ExecutionPermit`]
//! 允许触发 OS 副作用**。其余六类在 [`DataState::may_trigger_side_effect`] 上恒为 `false`。
//!
//! 另有一条容易被绕过的规则也在这里落地：LLM 输出看起来像系统消息，也不能升级信任等级
//! （§7.2）。因此 [`ActionIntent`] 的唯一构造入口要求一个真实的 [`PredictionRef`]，
//! 而不是一个"模型说可以执行"的布尔值。

use serde::{Deserialize, Serialize};

use crate::belief::{Hypothesis, Observation, Prediction};
use crate::{
    ActionIntent, ActionReceipt, ExecutionPermit, OutcomeVerified,
};

/// 数据状态种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataStateKind {
    /// 观测。
    Observation,
    /// 假设。
    Hypothesis,
    /// 预测。
    Prediction,
    /// 动作意图。
    ActionIntent,
    /// 执行许可。
    ExecutionPermit,
    /// 执行回执。
    ActionReceipt,
    /// 后置条件判定。
    OutcomeVerified,
}

/// 数据状态。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state_kind", rename_all = "snake_case")]
pub enum DataState {
    /// 某来源在某时刻的观测。不能直接触发副作用。
    Observation(Observation),
    /// 单元或模型的解释，带支持、反对与未知项。不能直接触发副作用。
    Hypothesis(Hypothesis),
    /// 动作前可检查的预期结果。不能直接触发副作用。
    Prediction(Prediction),
    /// 动作意图。不能直接触发副作用。
    ActionIntent(ActionIntent),
    /// 执行许可。**唯一**可以被受限执行代理消费的状态。
    ExecutionPermit(ExecutionPermit),
    /// 执行回执。不等于后置条件验证成功，不能直接触发副作用。
    ActionReceipt(ActionReceipt),
    /// 后置条件判定。驱动下一轮，不自动追加新权限，不能直接触发副作用。
    OutcomeVerified(OutcomeVerified),
}

impl DataState {
    /// 种类。
    pub fn kind(&self) -> DataStateKind {
        match self {
            Self::Observation(_) => DataStateKind::Observation,
            Self::Hypothesis(_) => DataStateKind::Hypothesis,
            Self::Prediction(_) => DataStateKind::Prediction,
            Self::ActionIntent(_) => DataStateKind::ActionIntent,
            Self::ExecutionPermit(_) => DataStateKind::ExecutionPermit,
            Self::ActionReceipt(_) => DataStateKind::ActionReceipt,
            Self::OutcomeVerified(_) => DataStateKind::OutcomeVerified,
        }
    }

    /// 是否允许触发 OS 副作用。
    ///
    /// 只有 [`DataStateKind::ExecutionPermit`] 返回 `true`。
    pub fn may_trigger_side_effect(&self) -> bool {
        matches!(self, Self::ExecutionPermit(_))
    }

    /// 是否是原始观测。
    ///
    /// 只有原始观测（以及它派生出的、带 `derived_from` 指向的观测）可以作为证据被引用；
    /// 假设、预测、意图都不是证据。
    pub fn is_evidence(&self) -> bool {
        matches!(self, Self::Observation(_))
    }

    /// 执行代理消费许可后的交接：把许可换成"已提交"的回执。
    ///
    /// 注意这里只生成回执，不生成 [`OutcomeVerified`]。后置条件必须由新的观测另行判定。
    pub fn into_receipt(self, receipt: ActionReceipt) -> Result<ActionReceipt, &'static str> {
        match self {
            Self::ExecutionPermit(permit) => {
                if permit.permit_id != receipt.permit_id {
                    return Err("回执引用的许可与被执行许可不一致");
                }
                Ok(receipt)
            }
            _ => Err("只有 ExecutionPermit 可以被消费为回执"),
        }
    }

    /// 校验状态自身。
    pub fn validate(&self) -> Result<(), crate::ContractError> {
        match self {
            Self::Hypothesis(hypothesis) => hypothesis.validate(),
            Self::Prediction(prediction) => {
                if let Some(probability) = &prediction.uncertainty.probability {
                    probability.validate()?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
