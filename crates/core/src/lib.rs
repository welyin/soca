//! SoCA 单主体运行时（实施路线 P0 的第三个工作包）。
//!
//! 本 crate 把契约层与存储层接成一个**可以停下来、可以崩溃、可以重放**的闭环：
//!
//! | 模块 | 职责 | 对应文档 |
//! |---|---|---|
//! | [`os`] | 确定性模拟 OS，含动作级幂等与可复现故障注入 | §16 P0 的"模拟 OS" |
//! | [`broker`] | 副作用唯一出口，只有执行许可能通过 | §12.2 |
//! | [`policy`] | 策略代理：决定要不要签发执行许可 | §12.1、§12.2 |
//! | [`session`] | §6 的九步循环，逐步可调用 | §6 |
//! | [`replay`] | 只读轨迹重放与自洽性检查 | §7.3 |
//!
//! 三条设计取舍值得写明：
//!
//! 1. **[`session::Session`] 的方法可以单独调用**，而不是只暴露一个 `run()`。§17 要求
//!    "覆盖意图前/后、执行后回执前、快照提交前/后"的故障注入点；只有每一步独立可调用，
//!    崩溃点才能被精确表达，而不是靠伪造异常。
//! 2. **[`replay::Trajectory::load`] 拿不到执行代理**。重放不会重新执行外部动作这件事，
//!    由签名保证，而不是由纪律保证。
//! 3. **P0 的预测内容由运行时按工具后置条件生成**。接入模型之后，预测内容改由单元提出、
//!    运行时只校验结构合规。让确定性代码负责可判定的部分，模型负责它擅长的部分（§8）。
//!
//! 本 crate 不碰真实文件系统、不发网络请求、不读用户私人内容。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod broker;
pub mod error;
pub mod lifecycle;
pub mod os;
pub mod policy;
pub mod replay;
pub mod session;
pub mod subject;

pub use crate::broker::{ActionBroker, BrokerError, BrokerOutcome};
pub use crate::error::CoreError;
pub use crate::lifecycle::{
    CheckpointOutcome, UnitRegistry, WakeOutcome, WakePolicy, WAKE_CATCH_UP_LIMIT,
};
pub use crate::os::{Attempt, AttemptOutcome, FaultPlan, ObjectState, SimulatedOs};
pub use crate::policy::{PermitDecision, PolicyAgent, DEFAULT_PERMIT_TTL_SECONDS};
pub use crate::replay::{ActionTrajectory, ReplayViolation, Trajectory};
pub use crate::session::{
    evaluate, DispatchOutcome, ObservationRecord, RoundReport, Session, ABSENT_VALUE,
};
pub use crate::subject::{
    AdvanceStep, GoalSummary, LoopRound, ModelConsultation, PublicState, RoundOutcome, Subject,
};
