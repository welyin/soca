//! 拓扑数量规划与伸缩滞后（实施规格 §5、工单 ENG-03）。
//!
//! 本 crate 是 `design/soca_reference/topology.py` 的行为移植。它**没有任何副作用**：
//! 不读硬件、不开进程、不写存储、不读时钟。资源包络与需求由调用方给出，因此同一份输入
//! 必然得到同一份计划——这正是 §15 的 SC-01 能在五种包络上复算的前提。
//!
//! 与参考实现的一致性由两组测试保证：一组是参考 `test_topology.py` 的 13 项等价移植，
//! 另一组是规格 §5.2 的五档计划表。两者都直接写死期望值，而不是用本实现自己算出来再对照。
//!
//! 本 crate 不负责把计划变成现实。`PLANNED` 与 `DONE` 是两件事：迁移事务、fencing 与
//! epoch 切换属于 `core-reconciler`（ENG-11）。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use soca_contracts::{
    ModelBackend, ModelReservation, PlanReason, PlanState, PlannerPolicy, ResourceEnvelope,
    ScaleAction, ScaleDecision, ScaleReason, TopologyError, TopologyPlan,
    CLUSTERS_PER_COORDINATOR, HOT_ACTORS_PER_WORKER, HOT_LEAF_DIVISOR, LEAVES_PER_CLUSTER,
    LEAF_PROFILES, MAX_CLUSTERS_BEFORE_COORDINATOR,
};

/// 最小拓扑档位。需求为 0 时也至少给这一档，而不是给零个叶。
pub const MINIMUM_LEAF_PROFILE: u32 = 8;

/// 按档位算出一份候选计划。
///
/// 本函数不判断可行性：它只回答"如果按这个档位配置，需要多少资源"。可行性由
/// [`plan_topology`] 判断。
pub fn candidate_plan(
    leaves: u32,
    envelope: &ResourceEnvelope,
    model: &ModelReservation,
    policy: &PlannerPolicy,
) -> Result<TopologyPlan, TopologyError> {
    if !LEAF_PROFILES.contains(&leaves) {
        return Err(TopologyError::UnsupportedLeafProfile {
            actual: leaves,
            allowed: LEAF_PROFILES.to_vec(),
        });
    }

    let clusters = leaves.div_ceil(LEAVES_PER_CLUSTER);
    let coordinators = if clusters > MAX_CLUSTERS_BEFORE_COORDINATOR {
        clusters.div_ceil(CLUSTERS_PER_COORDINATOR)
    } else {
        0
    };
    let hot_leaves = leaves / HOT_LEAF_DIVISOR;
    let workers = hot_leaves.div_ceil(HOT_ACTORS_PER_WORKER);
    // 每个簇、每个协调器各有一份控制状态，主体本身也算一份。
    let controllers = clusters + coordinators + 1;

    let catalog_mib = (u64::from(leaves) * policy.catalog_kib_per_leaf).div_ceil(1024);
    let required_mib = policy.core_mib
        + policy.cache_mib
        + model.ram_mib
        + catalog_mib
        + u64::from(hot_leaves) * policy.hot_state_mib_per_leaf
        + u64::from(controllers) * policy.controller_mib
        + u64::from(workers) * policy.worker_peak_mib;

    Ok(TopologyPlan {
        state: PlanState::Running,
        leaves,
        clusters,
        coordinators,
        subjects: 1,
        hot_leaves,
        workers,
        llm_parallel_calls: workers.min(model.max_parallel_calls),
        ram_required_mib: required_mib,
        ram_limit_mib: envelope.ram_limit_mib,
        reason: PlanReason::Admitted,
    })
}

/// 按当前包络与需求算出拓扑计划。
///
/// 硬件决定**可承载上限**，任务需求决定**实际申请多少槽**。因此需求为 0 时只给最小档位，
/// 而不是把可用硬件填满（§2：不因空闲 RAM 多就生成无任务角色）。
pub fn plan_topology(
    envelope: &ResourceEnvelope,
    model: &ModelReservation,
    desired_leaf_slots: u64,
    policy: &PlannerPolicy,
) -> Result<TopologyPlan, TopologyError> {
    // 先校验输入。绕过构造函数直接拼结构体也会在这里被拦住。
    envelope.validate()?;
    model.validate()?;
    policy.validate()?;

    // 暂停计划仍然保留 1 个主体：资源不足时暂停工作，而不是删除主体（§3.1 R2）。
    let paused = |reason: PlanReason| TopologyPlan {
        state: PlanState::Paused,
        leaves: 0,
        clusters: 0,
        coordinators: 0,
        subjects: 1,
        hot_leaves: 0,
        workers: 0,
        llm_parallel_calls: 0,
        ram_required_mib: policy.core_mib,
        ram_limit_mib: envelope.ram_limit_mib,
        reason,
    };

    if envelope.ram_limit_mib < policy.core_mib {
        return Ok(paused(PlanReason::ControlReserveUnavailable));
    }
    if envelope.telemetry_age_seconds > policy.max_telemetry_age_seconds {
        // 遥测失效时禁止扩容。产品仍保留旧快照与控制通道，由协调器排空认知工作。
        return Ok(paused(PlanReason::StaleTelemetry));
    }
    if model.backend == ModelBackend::Remote && !model.remote_authorized {
        // 不因本地资源不足而自动把私人上下文发到云端（§4.3、§8）。
        return Ok(paused(PlanReason::RemoteNotAuthorized));
    }
    if model.vram_mib > envelope.gpu_allocatable_mib {
        return Ok(paused(PlanReason::ModelVramUnavailable));
    }

    let requested = desired_leaf_slots
        .max(u64::from(MINIMUM_LEAF_PROFILE))
        .min(u64::from(policy.max_leaves));
    let Some(ceiling) = LEAF_PROFILES
        .iter()
        .copied()
        .find(|profile| u64::from(*profile) >= requested)
    else {
        return Err(TopologyError::InvalidDemand {
            limit: u64::from(policy.max_leaves),
        });
    };

    let cpu_ceiling = envelope.cpu_slots.min(policy.max_workers);
    let mut feasible: Option<TopologyPlan> = None;
    for leaves in LEAF_PROFILES {
        if leaves > ceiling || leaves > policy.max_leaves {
            break;
        }
        let candidate = candidate_plan(leaves, envelope, model, policy)?;
        if candidate.ram_required_mib <= envelope.ram_limit_mib && candidate.workers <= cpu_ceiling {
            feasible = Some(candidate);
        }
    }

    // 取不超过需求档位的最大可行项，而不是能放下就尽量放大。
    Ok(feasible.unwrap_or_else(|| paused(PlanReason::MinimumTopologyUnavailable)))
}

/// 伸缩滞后门控。
///
/// 它只**建议**，不改变拓扑：§5.3 明确"只有迁移事务提交后的 `acknowledge()` 更新冷却，
/// 不把'提出计划'当作'已完成扩容'"。
#[derive(Debug, Clone)]
pub struct ScaleGate {
    upscale_hold: f64,
    downscale_hold: f64,
    cooldown: f64,
    pending_target: Option<u32>,
    pending_since: Option<f64>,
    last_applied: f64,
    last_observed: f64,
}

impl Default for ScaleGate {
    fn default() -> Self {
        Self {
            upscale_hold: 60.0,
            downscale_hold: 10.0,
            cooldown: 120.0,
            pending_target: None,
            pending_since: None,
            last_applied: f64::NEG_INFINITY,
            last_observed: f64::NEG_INFINITY,
        }
    }
}

impl ScaleGate {
    /// 用给定的滞后时限构造。
    pub fn new(
        upscale_hold: f64,
        downscale_hold: f64,
        cooldown: f64,
    ) -> Result<Self, TopologyError> {
        for value in [upscale_hold, downscale_hold, cooldown] {
            if !value.is_finite() || value < 0.0 {
                return Err(TopologyError::InvalidHysteresis {
                    reason: "滞后时限必须有限且非负",
                });
            }
        }
        Ok(Self {
            upscale_hold,
            downscale_hold,
            cooldown,
            ..Self::default()
        })
    }

    /// 上一次被接纳的观测时刻。
    pub fn last_observed(&self) -> f64 {
        self.last_observed
    }

    /// 上一次迁移事务被确认的时刻。
    pub fn last_applied(&self) -> f64 {
        self.last_applied
    }

    /// 当前等待中的目标。
    pub fn pending_target(&self) -> Option<u32> {
        self.pending_target
    }

    /// 观察一次新的目标。
    ///
    /// `now` 必须是同一次 boot 的单调秒。跨重启不复用旧计时，§5.3 要求"重启重建门控状态，
    /// 不接受旧 boot 的计时样本"。
    ///
    /// `emergency` 表示压力事件立即触发，不等滞后计时器（"不能等 10 秒伸缩滞后才处理实际
    /// OOM"）。
    pub fn observe(
        &mut self,
        current_leaves: u32,
        desired: &TopologyPlan,
        now: f64,
        emergency: bool,
    ) -> Result<ScaleDecision, TopologyError> {
        if current_leaves != 0 && !LEAF_PROFILES.contains(&current_leaves) {
            return Err(TopologyError::InvalidCurrentTopology {
                actual: current_leaves,
            });
        }
        if !now.is_finite() || now < self.last_observed {
            return Err(TopologyError::NonMonotonicTime {
                last: self.last_observed.to_string(),
                now: now.to_string(),
            });
        }
        self.last_observed = now;

        let target = desired.leaves;
        if emergency || desired.state != PlanState::Running {
            self.pending_target = None;
            self.pending_since = None;
            return Ok(ScaleDecision {
                action: ScaleAction::FreezeAndReconcile,
                target_leaves: target,
                reason: if emergency {
                    ScaleReason::PressureEmergency
                } else {
                    ScaleReason::NotRunning(desired.reason)
                },
            });
        }

        if target == current_leaves {
            self.pending_target = None;
            self.pending_since = None;
            return Ok(ScaleDecision {
                action: ScaleAction::Keep,
                target_leaves: current_leaves,
                reason: ScaleReason::AtTarget,
            });
        }

        // 目标来回波动会重置等待时间（§5.3）。
        if Some(target) != self.pending_target {
            self.pending_target = Some(target);
            self.pending_since = Some(now);
        }

        let growing = target > current_leaves;
        let required_hold = if growing {
            self.upscale_hold
        } else {
            self.downscale_hold
        };
        let since = self.pending_since.unwrap_or(now);
        let cooling = growing && now - self.last_applied < self.cooldown;
        if now - since < required_hold || cooling {
            return Ok(ScaleDecision {
                action: ScaleAction::Keep,
                target_leaves: current_leaves,
                reason: ScaleReason::Hysteresis,
            });
        }

        // 扩容每次最多上升一档；缩容可以跨多档降到可承载规模（§5.3）。
        let target = if growing {
            let ceiling = if current_leaves == 0 {
                MINIMUM_LEAF_PROFILE
            } else {
                current_leaves.saturating_mul(2)
            };
            target.min(ceiling)
        } else {
            target
        };

        Ok(ScaleDecision {
            action: ScaleAction::ProposeTransaction,
            target_leaves: target,
            reason: if growing {
                ScaleReason::StableGrowth
            } else {
                ScaleReason::StableReduction
            },
        })
    }

    /// 迁移事务提交后确认。
    pub fn acknowledge(&mut self, now: f64) -> Result<(), TopologyError> {
        if !now.is_finite() || now < self.last_observed || now < self.last_applied {
            return Err(TopologyError::NonMonotonicTime {
                last: self.last_observed.max(self.last_applied).to_string(),
                now: now.to_string(),
            });
        }
        self.last_applied = now;
        self.pending_target = None;
        self.pending_since = None;
        Ok(())
    }
}

/// 计划的可序列化形式，字段集合与参考规划器 CLI 输出一致。
///
/// `reference_simulation_only` 恒为 `true`：本次输出由调用方给出的**模拟包络**算出，
/// 没有读取机器硬件，也没有创建任何运行进程。
pub fn plan_json(plan: &TopologyPlan) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(plan)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "logical_units".to_string(),
            serde_json::json!(plan.logical_units()),
        );
        object.insert(
            "composition_depth".to_string(),
            serde_json::json!(plan.composition_depth()),
        );
        object.insert(
            "reference_simulation_only".to_string(),
            serde_json::json!(true),
        );
    }
    Ok(value)
}
