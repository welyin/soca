//! 确定性传输层：把模型的**可变性**从测试里拿掉。
//!
//! §13 说得很清楚：
//!
//! > 固定策略与相同动作序列的引擎可复现，**不承诺在线 LLM API 天然 bitwise 确定**。
//!
//! 所以凡是需要复现的地方——闭环回归、崩溃恢复、迁移对照——都不能真的去调一个在线模型。
//! 本类型让"同一条轨迹重跑一遍得到同样的结论"成立。
//!
//! 两种应答方式，缺一不可：
//!
//! * [`DeterministicTransport::new`] 按脚本依次返回。适合"给定输入必然得到这个输出"的用例。
//! * [`DeterministicTransport::from_fn`] 按**实际收到的请求**现算。需要它是因为证据引用是
//!   运行时生成的（`obs:{uuid}`），脚本式写法根本没法预先引用它们——而"模型正确回引了它
//!   看到的证据"恰恰是最需要测的那一类行为。
//!
//! 它同时也证明了 [`crate::Transport`] 的 `&self` 设计可行：内部用原子计数与互斥量，因此可以
//! 被多个认知单元共享（§5：模型服务"按模型家族 0–2 个起步，不按单元数启动"）。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use soca_contracts::ModelOutput;

use crate::error::TransportError;
use crate::gateway::{ModelRequest, Transport};

/// 按请求现算返回物的闭包。
///
/// 要求 `Send + Sync` 是因为传输层要能被多个认知单元共享（§5：模型服务"按模型家族 0–2 个
/// 起步，不按单元数启动"）。
type Responder = Box<dyn Fn(&ModelRequest) -> Result<ModelOutput, TransportError> + Send + Sync>;

/// 应答源。
enum Source {
    /// 按脚本依次返回。
    Script {
        /// 尚未消费的脚本项。
        queue: VecDeque<Result<ModelOutput, TransportError>>,
        /// 只剩一项时是否重复它。
        repeat_last: bool,
    },
    /// 按请求现算。
    Responder(Responder),
}

/// 确定性传输层。
pub struct DeterministicTransport {
    source: Mutex<Source>,
    invoked: AtomicU64,
}

impl std::fmt::Debug for DeterministicTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &*self.lock() {
            Source::Script { .. } => "script",
            Source::Responder(_) => "responder",
        };
        f.debug_struct("DeterministicTransport")
            .field("source", &kind)
            .field("invoked", &self.invoked())
            .finish()
    }
}

impl DeterministicTransport {
    /// 用一串预设结果构造。按顺序消耗，用尽后报错。
    pub fn new(script: Vec<Result<ModelOutput, TransportError>>) -> Self {
        Self {
            source: Mutex::new(Source::Script {
                queue: script.into_iter().collect(),
                repeat_last: false,
            }),
            invoked: AtomicU64::new(0),
        }
    }

    /// 永远返回同一份结果。
    ///
    /// 适合"这个单元每轮都会问一次模型"的场景：脚本式写法会把测试写成"预先知道要调用几次"，
    /// 而那本身是不该被固化的实现细节。
    pub fn always(output: ModelOutput) -> Self {
        Self {
            source: Mutex::new(Source::Script {
                queue: VecDeque::from([Ok(output)]),
                repeat_last: true,
            }),
            invoked: AtomicU64::new(0),
        }
    }

    /// 按请求现算返回物。
    ///
    /// 闭包看到的是**它实际收到的请求**，因此可以回引 `request.context.evidence` 里的引用。
    /// 这正是测试"模型只能引用它看到的证据"所必需的：证据引用是运行时生成的，脚本写不出来。
    pub fn from_fn<F>(responder: F) -> Self
    where
        F: Fn(&ModelRequest) -> Result<ModelOutput, TransportError> + Send + Sync + 'static,
    {
        Self {
            source: Mutex::new(Source::Responder(Box::new(responder))),
            invoked: AtomicU64::new(0),
        }
    }

    /// 被调用过几次。用来断言"没有偷偷重试"。
    pub fn invoked(&self) -> u64 {
        self.invoked.load(Ordering::Relaxed)
    }

    /// 脚本还剩几项。应答式返回 `0`。
    pub fn remaining(&self) -> usize {
        match &*self.lock() {
            Source::Script { queue, .. } => queue.len(),
            Source::Responder(_) => 0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Source> {
        // 中毒只说明先前有别的线程持锁时 panic 了；对本类型而言内部状态仍是完好的，
        // 取回它比跟着 panic 更合适。
        self.source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Transport for DeterministicTransport {
    fn invoke(&self, request: &ModelRequest) -> Result<ModelOutput, TransportError> {
        self.invoked.fetch_add(1, Ordering::Relaxed);

        let mut source = self.lock();
        match &mut *source {
            Source::Responder(responder) => responder(request),
            Source::Script { queue, repeat_last } => {
                if queue.is_empty() {
                    // 脚本用尽属于测试编排问题，不是传输层故障：**不返回可重试错误**，
                    // 否则网关会拿它重试，把一个写错的测试伪装成一次偶发失败。
                    return Err(TransportError::Rejected {
                        reason: "确定性传输层脚本已用尽".to_string(),
                    });
                }
                if *repeat_last && queue.len() == 1 {
                    return queue.front().cloned().expect("非空");
                }
                queue.pop_front().expect("非空")
            }
        }
    }
}
