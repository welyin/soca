//! 模型网关：预算、超时、有界重试与返回物校验（§8、§3.1）。
//!
//! 网关只做四件事，每一件都对应一条硬约束：
//!
//! | 做的事 | 依据 |
//! |---|---|
//! | 出站判断 | §8：私人数据不得出站；未授权不做云端请求 |
//! | 总墙钟上限 | §8：超时无无限重试 |
//! | 返回物解析与校验 | §8：模型返回候选后**先解析和校验**，再决定工具动作 |
//! | 记录尝试次数 | §6.8：失败、超时、否决均保留在最小审计账中 |
//!
//! 它**不**做的一件事：不替模型决定任何事。返回的 `proposals` 是候选，要经过 L2、
//! L4 与执行许可三道各自独立的关卡才能变成动作（§3.1："它提出假设和动作，不独占信念、
//! 记忆、权限、预算或执行权"）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use soca_contracts::{
    ContextBundle, ModelBackend, ModelBudget, ModelOutput, ModelProfileRef, ModelVersion,
};

use crate::error::{GatewayError, TransportError};

/// 一次模型调用的请求。
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    /// 已经编译并校验过的上下文。
    pub context: ContextBundle,
    /// 本次预算。传输层据此设置自己的超时。
    pub budget: ModelBudget,
    /// 选用哪个模型画像。
    ///
    /// 传的是**引用**而不是权重或句柄：§8 要求"工具句柄和预算在 Core 与存储中，不仅在 LLM
    /// 上下文窗口里"，模型权重同理。
    pub model_profile_ref: ModelProfileRef,
}

/// 模型调用的传输层。
///
/// 收 `&self` 而不是 `&mut self`：§5 要求模型服务"按模型家族 0–2 个起步，**不按单元数启动**"，
/// 也就是许多认知单元共享同一个模型服务。`&self` 才能让一个传输层挂在 `Arc` 后面被共享。
/// 因此实现方若有计数需求，用原子量而不是 `&mut self`。
pub trait Transport {
    /// 发一次调用。
    ///
    /// 实现方**必须**遵守 `request.budget.max_wall_millis` 设置自己的超时，并在超时后
    /// 返回 [`TransportError::Timeout`]，而不是一直等下去。网关另有总墙钟把关，但那是兜底：
    /// 让传输层自己遵守约定，才能避免一次调用占满整个额度。
    fn invoke(&self, request: &ModelRequest) -> Result<ModelOutput, TransportError>;
}

/// 通过校验的模型返回物。
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedOutput {
    /// 返回物本身。
    pub output: ModelOutput,
    /// 实际尝试了几次。
    pub attempts: u8,
    /// 端到端耗时（毫秒，单调时钟）。
    pub elapsed_millis: u64,
}

/// 模型网关。
#[derive(Debug)]
pub struct ModelGateway<T> {
    transport: T,
    backend: ModelBackend,
    remote_authorized: bool,
    budget: ModelBudget,
    model_version: ModelVersion,
    /// 累计发起的调用次数（含重试）。只用于记账与审计。
    calls: AtomicU64,
}

impl<T> ModelGateway<T> {
    /// 装配网关。
    pub fn new(
        transport: T,
        backend: ModelBackend,
        remote_authorized: bool,
        budget: ModelBudget,
        model_version: ModelVersion,
    ) -> Result<Self, GatewayError> {
        budget.validate()?;
        Ok(Self {
            transport,
            backend,
            remote_authorized,
            budget,
            model_version,
            calls: AtomicU64::new(0),
        })
    }

    /// 目标后端。
    pub fn backend(&self) -> ModelBackend {
        self.backend
    }

    /// 当前预算。
    pub fn budget(&self) -> ModelBudget {
        self.budget
    }

    /// 产出结果的模型版本。
    pub fn model_version(&self) -> &ModelVersion {
        &self.model_version
    }

    /// 累计发起的调用次数（含重试）。
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// 传输层的只读借用。
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: Transport> ModelGateway<T> {
    /// 发一次调用，并校验返回物。
    ///
    /// 重试策略由 [`TransportError::is_retryable`] 决定，**且**每一步都受两个上限约束：
    /// 尝试次数（`budget.max_attempts`）与总墙钟（`budget.max_wall_millis`）。
    /// 两个都要有：只限次数，一次卡死就能占满整个任务；只限墙钟，一串快速失败就能打出
    /// 无意义的请求量。
    pub fn invoke(
        &mut self,
        context: &ContextBundle,
        model_profile_ref: ModelProfileRef,
    ) -> Result<ValidatedOutput, GatewayError> {
        // §8 的出站判断。放在最前面：放晚了就有人能先构造好请求再"顺便"检查。
        context.authorize_backend(self.backend, self.remote_authorized)?;

        let request = ModelRequest {
            context: context.clone(),
            budget: self.budget,
            model_profile_ref,
        };

        let started = Instant::now();
        let mut attempts: u8 = 0;

        loop {
            let elapsed = millis_since(started);
            if elapsed >= self.budget.max_wall_millis {
                return Err(GatewayError::WallClockExceeded {
                    limit_millis: self.budget.max_wall_millis,
                    elapsed_millis: elapsed,
                });
            }

            attempts = attempts.saturating_add(1);
            self.calls.fetch_add(1, Ordering::Relaxed);

            match self.transport.invoke(&request) {
                Ok(output) => {
                    // §8："模型返回候选后先解析和校验，再决定工具动作。"
                    output.validate(&self.budget)?;
                    context.validate_proposals(&output.proposals)?;
                    return Ok(ValidatedOutput {
                        output,
                        attempts,
                        elapsed_millis: millis_since(started),
                    });
                }
                Err(error) => {
                    if !error.is_retryable() {
                        // 鉴权失败或返回物解析不了——再试多少次都是同一个结果。
                        return Err(GatewayError::Transport(error));
                    }
                    if !self.budget.allows_another_attempt(attempts) {
                        return Err(GatewayError::AttemptsExhausted {
                            attempts,
                            last: error,
                        });
                    }
                }
            }
        }
    }
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
