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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use soca_contracts::{PlanState, TopologyPlan, WallClock};

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

// ---------------------------------------------------------------------------
// 峰值账
// ---------------------------------------------------------------------------

/// 一个计量：当前值、峰值，以及峰值出现的时刻。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    /// 当前值。
    pub current: u64,
    /// 自上次 [`ResourceLedger::reset_peaks`] 以来的峰值。
    pub peak: u64,
    /// 峰值出现的时刻。`None` 表示这个计量从来还没有被记过。
    ///
    /// 记时刻不只是为了好看：一次长跑里"峰值出现在第 3 分钟还是第 11 小时"决定了它是
    /// 启动抖动还是泄漏，而两者要做的事完全不同。
    pub peak_at: Option<WallClock>,
}

/// 一台**峰值**账（§17 的"记录峰值……不只看平均值"）。
///
/// §17 那一行的后半句是：
///
/// > 记录**峰值**私有提交及工作集，**不只看平均值**。
///
/// ## 为什么不能事后从采样里算
///
/// 峰值不能由一串采样推出来——除非把所有采样都留着，而"留所有采样"正是平均值的邻居，
/// 也正是长跑里最先撑不住的那一部分。所以它必须在**值变化的那一刻**记下来，
/// 而这决定了它的形状：一个随写随更新的账，而不是一个查询函数。
///
/// ## "不只看平均值"不是一句废话
///
/// 一次跑了一小时、平均占用 200 MiB、峰值 2 GiB 的运行**会被 OOM 打掉**，而它的平均值
/// 很好看。§17 要的是那一行能回答"这台机器被压到过什么程度"，平均值答不了它。
///
/// ## 它记的不是 OS 的数字
///
/// 本版不读真实硬件（§19 末段），所以记的是**进程自己知道的那部分**：内容仓的字节、
/// 事件与审计的条数、内存里的池子大小。它们是"私有提交"里可归因的那部分——
/// 而可归因的那部分才是能拿去做决策的。真正的 OS 数字（`GetPerformanceInfo`）该由
/// 适配器提供，本版没有；这一点写在这里，免得读的人以为这里记的是工作集。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLedger {
    metrics: BTreeMap<String, Metric>,
}

impl ResourceLedger {
    /// 记一次当前值。
    ///
    /// 比峰值高就更新峰值，否则只更新当前值。**当前值下降不会带走峰值**——那正是这台账
    /// 存在的全部意义。
    pub fn record(&mut self, name: &str, value: u64, at: WallClock) {
        match self.metrics.get_mut(name) {
            None => {
                self.metrics.insert(
                    name.to_string(),
                    Metric {
                        current: value,
                        peak: value,
                        peak_at: Some(at),
                    },
                );
            }
            Some(metric) => {
                metric.current = value;
                if value > metric.peak {
                    metric.peak = value;
                    metric.peak_at = Some(at);
                }
            }
        }
    }

    /// 某个计量。没有记过时返回 `None`。
    pub fn metric(&self, name: &str) -> Option<&Metric> {
        self.metrics.get(name)
    }

    /// 全部计量，按名字排序（`BTreeMap`，所以同一份状态永远序列化成同一串字节）。
    pub fn metrics(&self) -> &BTreeMap<String, Metric> {
        &self.metrics
    }

    /// 当前值之和。
    pub fn total_current(&self) -> u64 {
        self.metrics.values().map(|metric| metric.current).sum()
    }

    /// 峰值之和。它是"这台机器被压到过的最坏情形"的一个粗略上界——
    /// 各值的峰值未必同时出现，所以是**上界**而不是实际同时占用。
    pub fn total_peak(&self) -> u64 {
        self.metrics.values().map(|metric| metric.peak).sum()
    }

    /// 开一段新的计量窗口：**把峰值压到当前值**。
    ///
    /// 不是压到 0。压到 0 会让新窗口的峰值从"当时已经是多少"往下算起，
    /// 于是它报出的峰值比实际低——而一个偏低的峰值比没有峰值更糟，因为它看起来是个答案。
    ///
    /// §17 要求"先 8 小时后 24 小时试运行"，那就是两段：第一段的峰值不该污染第二段。
    pub fn reset_peaks(&mut self, at: WallClock) {
        for metric in self.metrics.values_mut() {
            metric.peak = metric.current;
            metric.peak_at = Some(at);
        }
    }

    /// 峰值比当前值高出多少。没有涨过的计量是 0。
    ///
    /// 它是"这个计量有没有被撑到过"的最短回答，适合给界面用。
    pub fn headroom(&self, name: &str) -> u64 {
        self.metric(name)
            .map_or(0, |metric| metric.peak.saturating_sub(metric.current))
    }
}
