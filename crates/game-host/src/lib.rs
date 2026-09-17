//! 游戏规则宿主（实施规格 §8、§9、§11；工单 ENG-06 的宿主侧）。
//!
//! 本 crate 实现 SoCA 与规则引擎之间的那道闸门，以及被闸门调用的引擎接口。它**不含**
//! 迷宫或扫雷规则：§10.1 明确要求不手写第二套看似相同的规则，真实引擎来自 MiniGrid 适配器
//! （ENG-07）与成熟 Mines 内核封装（ENG-08）。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`engine`] | 规则引擎接口，以及明确标注为测试替身的 [`ProtocolProbeEngine`] |
//! | [`host`] | 幂等账、新鲜度校验、终局区分与一步一写者 |
//!
//! 三条本 crate 落实的隔离：
//!
//! 1. **隐藏真值没有出口**：引擎返回的已经是投影后的公开感知，宿主再把它包成
//!    [`soca_contracts::GameObservation`]；`seed` 与 `info` 在这条路径上无处安放（§11.1、§13）。
//! 2. **一次世界步只发生一次**：登记发生在执行之前，崩溃留下的 `PENDING` 会被判定为
//!    `unknown_commit` 并截断回合，而不是被重放（§11.2、§13）。
//! 3. **故障不等于判负**：`terminated` 与 `truncated` 各自成立，基础设施截断不能被
//!    从分母里删除（§11.3、GM-07）。
//!
//! 本 crate 不做网络、不写用户文件、不调用模型。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod engine;
pub mod error;
pub mod host;

pub use crate::engine::{Engine, EngineFactory, EngineStep, ProbeFactory, ProtocolProbeEngine};
pub use crate::error::{EngineError, HostError};
pub use crate::host::{GameHost, HostConfig};
