//! §4.1 L4 的调度器：决定"现在该不该跑、跑几轮、什么时候停"。
//!
//! §4.1 把调度器和状态机、规则检查、执行代理列在同一格，§9.1 那句是"**应用**决定'谁该工作'"。
//! 所以调度器**不是定时器**：它的全部内容是判断，而判断需要理由。三条它负责的判断：
//!
//! 1. **暂停优先于一切。** §12.1 的全局暂停是用户的一个动作，而用户按下它期望的是
//!    "现在停"。跑完手头这几轮再停，等于把一个安全动作变成一个礼貌建议。所以暂停
//!    **每一轮都查**，不是只在开头查一次。
//! 2. **资源压力同样优先。** §17"弹性"那一行是"人为降低可用内存、GPU OOM、磁盘忙时，
//!    **停止后台扩容**并保持取消/审批可响应"。[`Resources`] 承载前半句，与暂停**同列**
//!    但**报成两回事**——一个是用户按的，一个是机器到的。
//! 3. **没有进展就要退避。** 没有可推进的候选时继续跑，只会把审计账塞满一样的记录
//!    ——§12.3 说审计只放最小元数据，正是因为它假设每条记录都对应一次真实发生的事。
//! 4. **有界。** 一次调度不能占住任意长的时间。
//!
//! ## 它不拥有时钟
//!
//! 调度器不睡觉。每次推进之间把时钟拨一秒（[`Scheduler::SECONDS_PER_ROUND`]），而真正的
//! 等待是调用方的事。把等待塞进来就要求它读真实时间，而那样一来测试里就复现不了（§13）。
//!
//! 同理，**"取消"在这里就是不再调用**。单进程里"保持可响应"靠的不是一个可以被中断的循环，
//! 而是每一轮本身有界、以及调用方随时可以停手。

use serde::Serialize;
use soca_contracts::{ActionLevel, SelectionPolicy, WallClock};

use crate::error::CoreError;
use crate::resources::Resources;
use crate::subject::{AdvanceStep, LoopRound, RoundOutcome, Subject};

/// 一次调度的参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheduler {
    /// 一次调度最多跑几轮。
    pub max_rounds: u32,
    /// 连续几轮没有进展就停。
    pub idle_limit: u32,
}

impl Default for Scheduler {
    fn default() -> Self {
        // 16 轮、连续 3 轮无进展就停。两个数都是"宁可早点停"的方向：
        // 停早了只是少做一点，而停晚了会把审计账刷满一串一样的记录。
        Self {
            max_rounds: 16,
            idle_limit: 3,
        }
    }
}

impl Scheduler {
    /// 每次推进之间把时钟拨多少秒。
    pub const SECONDS_PER_ROUND: i64 = 1;

    /// 跑一段。
    ///
    /// `resources` 是这个调用点**看到的**资源状况（§17 的"弹性"）。由调用方给，而不是
    /// 调度器自己去读硬件：§9.1 说"**应用**决定'谁该工作'"，而"现在有多少资源"是硬件与
    /// 规划器（§10.1）的回答，不是调度器的。
    pub fn run(
        &self,
        subject: &mut Subject,
        resources: &Resources,
        policy: &SelectionPolicy,
        risk: ActionLevel,
        at: WallClock,
    ) -> Result<ScheduleReport, CoreError> {
        let mut rounds = 0u32;
        let mut idle = 0u32;
        let mut collected: Vec<LoopRound> = Vec::new();

        let outcome = loop {
            if let Some(reason) = subject.policy().pause_reason() {
                break ScheduleOutcome::Paused {
                    reason: reason.to_string(),
                    rounds,
                };
            }
            // 资源压力与暂停同列，而且**每一轮都查**——理由和暂停那一档一样：§17 要的是
            // "停止后台扩容并**保持取消/审批可响应**"，而"可响应"的前提是它没有先把机器压垮。
            //
            // 放在跑轮之前而不是之后：这一档要挡住的是"再要一份资源"，而跑完一轮再检查
            // 就已经要过了。
            if let Some(reason) = resources.stop_reason() {
                break ScheduleOutcome::ResourcePressure {
                    reason: reason.to_string(),
                    emergency: resources.is_emergency(),
                    rounds,
                };
            }
            if rounds >= self.max_rounds {
                break ScheduleOutcome::RoundsSpent { rounds };
            }

            let report = subject.run_round(
                policy,
                risk,
                at.plus_seconds(i64::from(rounds) * Self::SECONDS_PER_ROUND),
            )?;
            rounds = rounds.saturating_add(1);

            let stop = match &report.outcome {
                RoundOutcome::Finished { .. } => Some(ScheduleOutcome::Finished { rounds }),
                RoundOutcome::Idle => {
                    idle = idle.saturating_add(1);
                    None
                }
                RoundOutcome::NeedsInput { missing } => Some(ScheduleOutcome::NeedsInput {
                    rounds,
                    missing: missing.clone(),
                }),
                RoundOutcome::Advanced { step } => match step {
                    // 三件**真的改变了什么**的事。只有它们算"有进展"。
                    AdvanceStep::Observation { .. }
                    | AdvanceStep::Claim { .. }
                    | AdvanceStep::Action { .. } => {
                        idle = 0;
                        None
                    }
                    // 等人工批准是一条独立的走向：它等的是**人**，而缺信息等的是**世界**。
                    // 两者报成同一件事，操作员就分不出"该去看一眼批准"与"该去看一眼数据"。
                    AdvanceStep::NeedsApproval { reason, .. } => {
                        Some(ScheduleOutcome::NeedsApproval {
                            rounds,
                            reason: reason.clone(),
                        })
                    }
                    // 拒绝与"这条通路还没有"什么都没改变。把它们算作空转——否则一个每轮
                    // 都被拒的动作会把整个额度烧光，而账上只看得到一串一模一样的拒绝。
                    //
                    // 但这两种**不**取走候选：拒绝了不等于这件事以后都不该做。用户补一次
                    // 批准之后重试它恰恰是对的，而取走就没有重试了。
                    AdvanceStep::Refused { .. } | AdvanceStep::Unsupported { .. } => {
                        idle = idle.saturating_add(1);
                        None
                    }
                },
            };

            collected.push(report);
            if let Some(outcome) = stop {
                break outcome;
            }
            if idle >= self.idle_limit {
                break ScheduleOutcome::BackedOff {
                    rounds,
                    idle_rounds: idle,
                };
            }
        };

        Ok(ScheduleReport {
            outcome,
            rounds: collected,
        })
    }
}

/// 一段调度的完整记录：结论 + 逐轮。
///
/// 结论回答"为什么停"，逐轮回答"刚才发生了什么"。**两个都要**：只给结论，界面
/// 就无从知道那几轮到底做了什么；只给逐轮，调用方就得自己重新推导"为什么停"，
/// 而那个推导会和调度器里的判断分叉——两套判断里总有一套是错的。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScheduleReport {
    /// 为什么停。
    pub outcome: ScheduleOutcome,
    /// 这一段的每一轮。
    pub rounds: Vec<LoopRound>,
}

/// 一次调度的结论（§6 第 9 步的四种走向，落到调度这一层）。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleOutcome {
    /// 跑满了给定的轮数。
    RoundsSpent {
        /// 实际跑了几轮。
        rounds: u32,
    },
    /// 跑着跑着没有可推进的了。§6 第 9 步的"结束"。
    Finished {
        /// 实际跑了几轮。
        rounds: u32,
    },
    /// 连续没有进展，退避停下。
    BackedOff {
        /// 实际跑了几轮。
        rounds: u32,
        /// 末尾连着几轮没有进展。
        idle_rounds: u32,
    },
    /// 全局暂停中（§12.1）。
    Paused {
        /// 暂停的理由。
        reason: String,
        /// 停下来之前跑了几轮。
        rounds: u32,
    },
    /// 资源压力下停下（§17 的"弹性"）。
    ///
    /// 与 [`ScheduleOutcome::Paused`] 分开：那一档是**用户按的**，这一档是**机器到的**。
    /// 合成一个取值的话，操作员看到"停了"会去问"谁按的暂停"——而这一档的答案是
    /// "没人按，是它自己到了边上"，该做的事完全不同。
    ResourcePressure {
        /// 压力来源（§5 的 `PlanReason`，或一次紧急事件的来源）。
        reason: String,
        /// 是不是一次压力事件（而不是计划上的暂停）。§5.3 的"不能等 10 秒伸缩滞后
        /// 才处理实际 OOM"说的就是这一个比特。
        emergency: bool,
        /// 停下来之前跑了几轮。
        rounds: u32,
    },
    /// 需要外部输入（§6 第 9 步的"请求澄清"）。
    NeedsInput {
        /// 实际跑了几轮。
        rounds: u32,
        /// 缺什么。
        missing: Vec<String>,
    },
    /// 停下来等人工批准（§12.1）。
    NeedsApproval {
        /// 实际跑了几轮。
        rounds: u32,
        /// 为什么还不能放行。
        reason: String,
    },
}

impl ScheduleOutcome {
    /// 这一段跑了几轮。
    pub fn rounds(&self) -> u32 {
        match self {
            Self::RoundsSpent { rounds }
            | Self::Finished { rounds }
            | Self::BackedOff { rounds, .. }
            | Self::Paused { rounds, .. }
            | Self::ResourcePressure { rounds, .. }
            | Self::NeedsInput { rounds, .. }
            | Self::NeedsApproval { rounds, .. } => *rounds,
        }
    }

    /// 还该不该继续。调用方自己驱动循环时用这个判断。
    pub fn wants_more(&self) -> bool {
        matches!(self, Self::RoundsSpent { .. })
    }
}
