//! 资源状况：调度器要不要因此停下（§10.1、§17 的"弹性"）。
//!
//! §17 那一行是：
//!
//! > 人为降低可用内存、GPU OOM、磁盘忙时，**停止后台扩容**并保持**取消/审批可响应**；
//! > 记录**峰值**私有提交及工作集，**不只看平均值**。
//!
//! 这一格承载的是前半句。而在单主体运行时里，"停止后台扩容"的形式就是**停下来**：
//! 拓扑计划说没有可承载的档位时，继续跑那一轮等于在一台已经超预算的机器上再要一份资源。
//!
//! ## 为什么把规划器接进来，而不是让调用方传一个布尔
//!
//! [`soca_core_topology`] 里有完整的可行性判断、伸缩滞后与压力紧急通道（§5、ENG-03），
//! 但在这一项之前它**没有任何调用方**——36 处引用全在它自己和自己的测试里。
//! 让调用方自己判断"现在有没有压力"，等于把那些判断复制一份到每个调用点，
//! 而两份判断迟早在某次改动里分叉；分叉的方向是某一处忘了查，于是系统在超预算的机器上继续跑。
//!
//! ## `Unknown` 为什么按"可以跑"处理
//!
//! 这是一个刻意的默认，方向与 §12.2 的失败关闭相反，所以写清楚：权限的默认必须朝拒绝，
//! 因为放行一次不该放行的动作是不可逆的；而调度器的默认朝放行，因为"没给包络"是一种
//! **配置缺失**，不是一种压力。让缺配置表现为"永远不跑"的话，一个忘了接线的新调用点
//! 第一次运行时会看起来像挂了——而那种故障最难归因。
//!
//! 它的代价是实的：调用方忘了接包络 = 压力保护不生效。所以 [`Resources::Unknown`] 是一个
//! **显式取值**，会出现在报告与界面里，而不是被悄悄当成"没有压力"。

use serde::{Deserialize, Serialize};
use soca_contracts::{PlanState, TopologyPlan};

/// 调度器看到的资源状况（§10.1、§17 的"弹性"）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resources {
    /// 调用方没有给资源包络。
    Unknown,
    /// 包络够用，计划是 `Running`。
    Running,
    /// 计划被暂停：**停止后台工作**。
    Paused {
        /// 计划给出的原因（§5 的 `PlanReason`）。
        reason: String,
    },
    /// 压力事件：**不等伸缩滞后**（§5.3 的"不能等 10 秒伸缩滞后才处理实际 OOM"）。
    ///
    /// 与 [`Resources::Paused`] 都是"停"，分开是因为**停下来之后该做什么不一样**：
    /// 计划暂停是"这台机器现在承载不了这个档位"，压力紧急是"刚刚出事了"。
    /// 报成同一件事的话，操作员分不出"该等一等"与"该去看一眼那台机器"。
    Emergency {
        /// 压力事件的来源。
        reason: String,
    },
}

impl Resources {
    /// 从一份拓扑计划读出来（§10.1）。
    pub fn from_plan(plan: &TopologyPlan) -> Self {
        match plan.state {
            PlanState::Running => Self::Running,
            PlanState::Paused => Self::Paused {
                reason: plan.reason.as_str().to_string(),
            },
        }
    }

    /// 现在能不能跑。
    pub fn is_runable(&self) -> bool {
        matches!(self, Self::Unknown | Self::Running)
    }

    /// 停下来的理由。`None` 表示能跑。
    pub fn stop_reason(&self) -> Option<&str> {
        match self {
            Self::Unknown | Self::Running => None,
            Self::Paused { reason } | Self::Emergency { reason } => Some(reason),
        }
    }

    /// 是不是一次压力事件（而不是计划上的暂停）。
    pub fn is_emergency(&self) -> bool {
        matches!(self, Self::Emergency { .. })
    }
}

impl Default for Resources {
    /// [`Resources::Unknown`]。理由见模块文档。
    fn default() -> Self {
        Self::Unknown
    }
}
