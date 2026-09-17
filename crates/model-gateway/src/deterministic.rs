//! 确定性传输层：把模型的**可变性**从测试里拿掉。
//!
//! §13 说得很清楚：
//!
//! > 固定策略与相同动作序列的引擎可复现，**不承诺在线 LLM API 天然 bitwise 确定**。
//!
//! 所以凡是需要复现的地方——闭环回归、崩溃恢复、迁移对照——都不能真的去调一个在线模型。
//! 本类型按脚本依次返回预设结果，让"同一条轨迹重跑一遍得到同样的结论"这件事成立。
//!
//! 它同时也证明了 [`crate::Transport`] 的 `&self` 设计是可行的：内部用原子计数与互斥量，
//! 因此可以被多个认知单元共享（§5：模型服务"按模型家族 0–2 个起步，不按单元数启动"）。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use soca_contracts::ModelOutput;

use crate::error::TransportError;
use crate::gateway::{ModelRequest, Transport};

/// 按脚本依次返回预设结果的传输层。
#[derive(Debug)]
pub struct DeterministicTransport {
    script: Mutex<VecDeque<Result<ModelOutput, TransportError>>>,
    /// 脚本用尽后是否重复最后一项。
    repeat_last: bool,
    invoked: AtomicU64,
}

impl DeterministicTransport {
    /// 用一串预设结果构造。按顺序消耗，用尽后报错。
    pub fn new(script: Vec<Result<ModelOutput, TransportError>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            repeat_last: false,
            invoked: AtomicU64::new(0),
        }
    }

    /// 永远返回同一份结果。
    ///
    /// 适合"这个单元每轮都会问一次模型"的场景：脚本式写法会把测试写成
    /// "预先知道要调用几次"，而那本身是不该被固化的实现细节。
    pub fn always(output: ModelOutput) -> Self {
        Self {
            script: Mutex::new(VecDeque::from([Ok(output)])),
            repeat_last: true,
            invoked: AtomicU64::new(0),
        }
    }

    /// 被调用过几次。用来断言"没有偷偷重试"。
    pub fn invoked(&self) -> u64 {
        self.invoked.load(Ordering::Relaxed)
    }

    /// 脚本还剩几项。
    pub fn remaining(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Result<ModelOutput, TransportError>>> {
        // 中毒只说明先前有别的线程持锁时 panic 了；对本类型而言队列本身仍是完好的，
        // 取回内部值比跟着 panic 更合适。
        self.script.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Transport for DeterministicTransport {
    fn invoke(&self, _request: &ModelRequest) -> Result<ModelOutput, TransportError> {
        self.invoked.fetch_add(1, Ordering::Relaxed);

        let mut script = self.lock();
        if script.is_empty() {
            // 脚本用尽属于测试编排问题，不是传输层故障：**不返回可重试错误**，否则
            // 网关会拿它重试，把一个写错的测试伪装成一次偶发失败。
            return Err(TransportError::Rejected {
                reason: "确定性传输层脚本已用尽".to_string(),
            });
        }
        if self.repeat_last && script.len() == 1 {
            return script.front().cloned().expect("非空");
        }
        script.pop_front().expect("非空")
    }
}
