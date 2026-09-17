//! SoCA 模型网关（架构文档 §5.1、§8、§3.1；实施规格 ENG-09）。
//!
//! §5.1 给这个 crate 的职责是"模型路由、缓存与上下文编译"。当前实现的是**上下文编译与
//! 返回物校验**这两块，也就是 §8 那条边界 —— 路由与缓存留到需要多后端与多模型时再加，
//! 现在没有第二种后端，先做等于搭空壳。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`compiler`] | §8 的七项上下文、证据挑选、出站判断 |
//! | [`gateway`] | 预算、总墙钟、有界重试、返回物解析与校验 |
//! | [`credentials`] | 端点凭据。**不可序列化**，因此进不了事件账、审计与快照 |
//! | [`remote`] | OpenAI 兼容的远端传输层（DeepSeek 是它的默认配置） |
//! | [`deterministic`] | 离线可复现的传输层（§13：不承诺在线 API 天然确定） |
//!
//! 两条边界在这里各安装一次：
//!
//! * **模型不能引用它没看到的证据。** [`contracts::ContextBundle::validate_proposals`]。
//!   这与 L2 的 `Workspace::validate_candidates` 是同一道闸的两次安装——一次挡在簇边界，
//!   一次挡在模型边界。任何一处单独存在都不够：模型可以绕过簇，簇也可以绕过模型。
//! * **模型不能自己签发预测引用。** 动作提案引用的预测必须已在上下文里（§6.3：预测由单元
//!   在动作前写下）。存储层随后还会独立再查一次预测是否存在。
//!
//! 本 crate 不做网络调用、不读密钥、不持有工具句柄。[`gateway::Transport`] 是留给真实
//! 后端的接缝，当前仓库里只有离线的确定性实现。
//!
//! [`contracts`]: soca_contracts

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod compiler;
pub mod credentials;
pub mod deterministic;
pub mod error;
pub mod gateway;
pub mod remote;

pub use crate::compiler::{ContextCompiler, ContextInput};
pub use crate::credentials::{CredentialError, ModelCredentials};
pub use crate::deterministic::DeterministicTransport;
pub use crate::error::{GatewayError, TransportError};
pub use crate::gateway::{ModelGateway, ModelRequest, Transport, ValidatedOutput};
pub use crate::remote::RemoteTransport;
