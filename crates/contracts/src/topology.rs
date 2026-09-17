//! 拓扑伸缩的契约类型（实施规格 §2–§5、工单 ENG-01 的"Plan 类型"）。
//!
//! 本模块只有类型与校验，**没有规划逻辑**：把包络算成计划的部分在 `soca-core-topology`。
//! 这样拆开是因为计划要被三方共同消费——规划器产出它、协调器执行它、训练台展示它，
//! 而三者都不应该各自复制一份字段定义。
//!
//! §2 明确要求同时维护三个不同计数，不允许混成"agent 数量"。本模块对应的是
//! **topology_count**（当前参与路由与任务分解的单元槽数）。`catalog_count` 与
//! `hot_count` 分别在单元目录与驻留层统计，不在这里。

use serde::{Deserialize, Serialize};

/// 允许的叶槽档位。计数从档位里选，不取任意整数。
pub const LEAF_PROFILES: [u32; 7] = [8, 16, 32, 64, 128, 256, 512];

/// 每个能力簇的目标叶数。
pub const LEAVES_PER_CLUSTER: u32 = 8;

/// 每个中间协调器最多承载的能力簇数。
pub const CLUSTERS_PER_COORDINATOR: u32 = 8;

/// 超过这个簇数才插入中间协调层。
pub const MAX_CLUSTERS_BEFORE_COORDINATOR: u32 = 16;

/// 节点的最大直属孩子数。
pub const ROOT_MAX_CHILDREN: u32 = 16;

/// 热叶比例的分母：热叶 = 叶数 / 4。
pub const HOT_LEAF_DIVISOR: u32 = 4;

/// 每个重 worker 可以分担的热 actor 数。
pub const HOT_ACTORS_PER_WORKER: u32 = 4;

/// 拓扑规划失败原因。
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TopologyError {
    #[error("叶槽档位不受支持：{actual}；允许的档位为 {allowed:?}")]
    UnsupportedLeafProfile { actual: u32, allowed: Vec<u32> },

    #[error("遥测年龄不合法：{actual}（必须有限且非负）")]
    InvalidTelemetryAge { actual: String },

    #[error("遥测新鲜度上界不合法：{actual}（必须有限且为正）")]
    InvalidTelemetryBound { actual: String },

    #[error("模型保留量不合法：{reason}")]
    InvalidModelReservation { reason: &'static str },

    #[error("规划策略不合法：{reason}")]
    InvalidPolicy { reason: &'static str },

    #[error("需求槽数不合法：必须是 0..={limit} 的整数")]
    InvalidDemand { limit: u64 },

    #[error("滞后时限不合法：{reason}")]
    InvalidHysteresis { reason: &'static str },

    #[error("当前拓扑不是合法档位：{actual}")]
    InvalidCurrentTopology { actual: u32 },

    #[error("观测时间不单调：上次 {last},本次 {now}")]
    NonMonotonicTime { last: String, now: String },
}

/// 资源包络。由 `ResourceGovernor` 归一后交给规划器（§4.1）。
///
/// `ram_limit_mib` 是**整个 SoCA 进程组可重新分配的绝对目标额度**，已扣除 OS 与其他应用
/// 的余量，但尚未扣除本方案的模型、Core 与 actor 成本。它不是瞬时可用内存。
/// `cpu_slots` 是预算内可同时运行的重工作槽数，不是逻辑线程总数。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceEnvelope {
    /// 可分配 RAM 额度（MiB，2^20 字节）。
    pub ram_limit_mib: u64,
    /// 可同时运行的重工作槽数。
    pub cpu_slots: u32,
    /// 可使用的安全 VRAM 额度（MiB）。API 不可用时为 0，不猜测。
    pub gpu_allocatable_mib: u64,
    /// 最近一次硬件采样的年龄（秒）。超过策略上限即禁止扩容。
    pub telemetry_age_seconds: f64,
}

impl ResourceEnvelope {
    /// 构造一个遥测新鲜的包络。
    pub fn new(ram_limit_mib: u64, cpu_slots: u32) -> Self {
        Self {
            ram_limit_mib,
            cpu_slots,
            gpu_allocatable_mib: 0,
            telemetry_age_seconds: 0.0,
        }
    }

    /// 校验。规划器在开始计算前调用它，绕过构造函数直接拼结构体也会被拦住。
    pub fn validate(&self) -> Result<(), TopologyError> {
        if !self.telemetry_age_seconds.is_finite() || self.telemetry_age_seconds < 0.0 {
            return Err(TopologyError::InvalidTelemetryAge {
                actual: self.telemetry_age_seconds.to_string(),
            });
        }
        Ok(())
    }
}

/// 模型服务后端。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelBackend {
    /// 本地 CPU 推理。
    Cpu,
    /// 本地 GPU 推理。
    Gpu,
    /// 远端 API。
    Remote,
}

impl ModelBackend {
    /// 稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Remote => "remote",
        }
    }
}

/// 模型服务预约（§4.3）。
///
/// 必须是选定模型配置的**实测峰值**预算：权重、KV 上限、后端工作区与所选最大并发都包含在
/// `ram_mib` / `vram_mib` 里，不仅是权重文件大小。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelReservation {
    /// 常驻 RAM 峰值（MiB）。
    pub ram_mib: u64,
    /// VRAM 峰值（MiB）。只有 GPU 后端可以非零。
    pub vram_mib: u64,
    /// 允许的并发模型调用数。
    pub max_parallel_calls: u32,
    /// 后端。
    pub backend: ModelBackend,
    /// 是否已获得云端出站授权。
    ///
    /// §8 与 §4.3 要求：未授权时规划器显式返回 `PAUSED`，不得因为本地资源不足而自动
    /// 把私人上下文发到云端。
    pub remote_authorized: bool,
}

impl Default for ModelReservation {
    fn default() -> Self {
        Self {
            ram_mib: 1024,
            vram_mib: 0,
            max_parallel_calls: 1,
            backend: ModelBackend::Cpu,
            remote_authorized: false,
        }
    }
}

impl ModelReservation {
    /// 校验。
    pub fn validate(&self) -> Result<(), TopologyError> {
        if self.max_parallel_calls < 1 {
            return Err(TopologyError::InvalidModelReservation {
                reason: "至少要允许一次模型调用",
            });
        }
        if self.backend != ModelBackend::Gpu && self.vram_mib != 0 {
            return Err(TopologyError::InvalidModelReservation {
                reason: "只有 GPU 后端可以预约 VRAM",
            });
        }
        Ok(())
    }
}

/// 成本与上限策略（§5.2 的参考成本）。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerPolicy {
    /// 人工叶数上限。
    pub max_leaves: u32,
    /// 重 worker 上限。
    pub max_workers: u32,
    /// Core 控制服务预算（MiB）。单主体规划范围一次，不按叶重复。
    pub core_mib: u64,
    /// 通用缓存预算（MiB）。有界且可优先回收。
    pub cache_mib: u64,
    /// 每个簇 / 协调器 / 主体的控制状态（MiB）。
    pub controller_mib: u64,
    /// 每个叶的路由与轻状态（KiB）。参与拓扑的所有叶都计费。
    pub catalog_kib_per_leaf: u64,
    /// 每个热叶新增状态（MiB）。只对热叶计费。
    pub hot_state_mib_per_leaf: u64,
    /// 每个重 worker 的峰值增量（MiB）。
    pub worker_peak_mib: u64,
    /// 遥测新鲜度上界（秒）。
    pub max_telemetry_age_seconds: f64,
}

impl Default for PlannerPolicy {
    fn default() -> Self {
        Self {
            max_leaves: 512,
            max_workers: 32,
            core_mib: 256,
            cache_mib: 256,
            controller_mib: 2,
            catalog_kib_per_leaf: 64,
            hot_state_mib_per_leaf: 16,
            worker_peak_mib: 256,
            max_telemetry_age_seconds: 3.0,
        }
    }
}

impl PlannerPolicy {
    /// 校验。
    pub fn validate(&self) -> Result<(), TopologyError> {
        if !LEAF_PROFILES.contains(&self.max_leaves) {
            return Err(TopologyError::UnsupportedLeafProfile {
                actual: self.max_leaves,
                allowed: LEAF_PROFILES.to_vec(),
            });
        }
        if self.max_workers < 1 {
            return Err(TopologyError::InvalidPolicy {
                reason: "至少要允许一个重 worker",
            });
        }
        if !self.max_telemetry_age_seconds.is_finite() || self.max_telemetry_age_seconds <= 0.0 {
            return Err(TopologyError::InvalidTelemetryBound {
                actual: self.max_telemetry_age_seconds.to_string(),
            });
        }
        Ok(())
    }
}

/// 计划状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanState {
    /// 可以运行。
    Running,
    /// 暂停：不创建 worker，也不丢弃主体身份。
    Paused,
}

/// 计划的理由代码。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanReason {
    /// 通过准入。
    Admitted,
    /// 连 256 MiB 参考控制预算都不可得。
    ControlReserveUnavailable,
    /// 遥测超过新鲜度上界。
    StaleTelemetry,
    /// 选定远端后端未获授权。
    RemoteNotAuthorized,
    /// 选定模型的 VRAM 超出可用额度。
    ModelVramUnavailable,
    /// 连最小拓扑都放不下。
    MinimumTopologyUnavailable,
    /// 压力紧急，需要冻结并核对。
    PressureEmergency,
}

impl PlanReason {
    /// 稳定名称，与参考实现一致。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "ADMITTED",
            Self::ControlReserveUnavailable => "CONTROL_RESERVE_UNAVAILABLE",
            Self::StaleTelemetry => "STALE_TELEMETRY",
            Self::RemoteNotAuthorized => "REMOTE_NOT_AUTHORIZED",
            Self::ModelVramUnavailable => "MODEL_VRAM_UNAVAILABLE",
            Self::MinimumTopologyUnavailable => "MINIMUM_TOPOLOGY_UNAVAILABLE",
            Self::PressureEmergency => "PRESSURE_EMERGENCY",
        }
    }
}

/// 一份拓扑计划。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyPlan {
    /// 状态。
    pub state: PlanState,
    /// 叶槽数。暂停时为 0。
    pub leaves: u32,
    /// 能力簇数。
    pub clusters: u32,
    /// 中间协调层数。簇数不超过 16 时为 0。
    pub coordinators: u32,
    /// 主体数。**身份不自动分裂或合并**，资源不足时暂停工作而不是删除主体。
    pub subjects: u32,
    /// 热叶数。
    pub hot_leaves: u32,
    /// 重 worker 数。
    pub workers: u32,
    /// 允许的并发模型调用数。
    pub llm_parallel_calls: u32,
    /// 所需 RAM（MiB）。
    pub ram_required_mib: u64,
    /// 可用 RAM 额度（MiB）。
    pub ram_limit_mib: u64,
    /// 理由。
    pub reason: PlanReason,
}

impl TopologyPlan {
    /// 逻辑单元总数。
    pub fn logical_units(&self) -> u32 {
        self.leaves + self.clusters + self.coordinators + self.subjects
    }

    /// 组合深度。未运行时为 0；有中间协调层时为 4。
    pub fn composition_depth(&self) -> u32 {
        if self.state != PlanState::Running {
            return 0;
        }
        3 + u32::from(self.coordinators > 0)
    }

    /// 是否是一个可以运行的拓扑。
    pub fn is_running(&self) -> bool {
        self.state == PlanState::Running
    }

    /// 拓扑数量三元组的可读形式，供日志与 UI 使用。
    pub fn counts(&self) -> (u32, u32, u32) {
        (self.leaves, self.clusters, self.subjects)
    }
}

/// 伸缩建议。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScaleAction {
    /// 保持现状。
    Keep,
    /// 提出一次迁移事务。**提出不等于完成**。
    ProposeTransaction,
    /// 冻结工作并在安全点核对，而不是即时删除 actor。
    FreezeAndReconcile,
}

/// 一次伸缩决策。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleDecision {
    /// 建议动作。
    pub action: ScaleAction,
    /// 目标叶数。
    pub target_leaves: u32,
    /// 理由。
    pub reason: ScaleReason,
}

/// 伸缩决策的理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScaleReason {
    /// 已在目标上。
    AtTarget,
    /// 处在滞后等待或冷却期。
    Hysteresis,
    /// 目标连续稳定的增长。
    StableGrowth,
    /// 目标连续稳定的缩减。
    StableReduction,
    /// 压力紧急：由外部压力事件显式触发，不是等滞后计时器到了才反应。
    PressureEmergency,
    /// 规划器认为当前不该运行，原样传递它的理由。
    NotRunning(PlanReason),
}
