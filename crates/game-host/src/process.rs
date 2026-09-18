//! 子进程引擎：通过长度前缀 JSON 与独立游戏进程对话（实施规格 §11.4、§13）。
//!
//! 为什么一定要跨进程：`games/` 下的每个目录持有 seed、真实局面与 RNG 状态。只要它们与
//! 认知循环在同一地址空间里，"认知单元读不到真值"就只能靠纪律保证。分开进程之后，
//! 隐藏状态在物理上不可达：公开面是唯一出口，而公开面的形状由协议冻结。
//!
//! 帧格式与 `games/*/protocol.py` 一致：4 字节小端长度 + UTF-8 JSON，单帧上限 256 KiB。
//! 先检查长度再分配，超限直接判定协议失步而不是先读进来（§11.4）。
//!
//! 超时处理：§11.4 要求 `step` 默认 5 秒、`reset` 默认 30 秒。读超时用独立线程加
//! `recv_timeout` 实现；一旦超时，子进程被杀、引擎被标记为不可用——因为**超时不代表动作
//! 没有执行**，此时唯一正确的做法是回去查幂等账，而不是重试或继续使用这个引擎。

use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use soca_contracts::{GameAction, GameKind};

use crate::engine::{Engine, EngineFactory, EngineStep};
use crate::error::{EngineError, ProcessEngineError};

/// 单帧字节上限，与 Python 侧保持一致。
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// 各类请求的超时（§11.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineTimeouts {
    /// 关卡生成。只在私有控制面。
    pub reset: Duration,
    /// 规则步。
    pub step: Duration,
    /// 握手与事实查询。
    ///
    /// **它实际上覆盖的是"进程冷启动到手能讲话"**，而不是一次往返：`spawn` 起完子进程就
    /// 立刻发 `facts`，而子进程在那一刻还没执行到第一行 Python——`import gymnasium` 与
    /// `import minigrid` 都还没发生。所以这个上限要按**解释器启动加导入**来给，
    /// 不能按"一次消息往返"来给。
    ///
    /// 默认值原本是 2 秒，它在一台空机器上够用；而并行跑整套测试时（多个测试二进制
    /// 同时各起一个 Python），它开始稳定超时——表现是 `Unavailable { reason: "等待游戏
    /// 进程响应超时（2 秒）" }`，看起来像引擎坏了，实际是**等错了东西**：
    /// 那 2 秒被拿去等导入，而不是等一次握手。
    pub facts: Duration,
}

impl Default for EngineTimeouts {
    fn default() -> Self {
        Self {
            reset: Duration::from_secs(30),
            // 5 秒对**已经在跑**的引擎是够的：规则步不该慢。
            step: Duration::from_secs(5),
            // 20 秒给冷启动。它比一次往返宽得多，是因为它要等的东西多得多。
            facts: Duration::from_secs(20),
        }
    }
}

/// 子进程引擎的启动配置。
#[derive(Debug, Clone)]
pub struct ProcessEngineConfig {
    /// 解释器或可执行文件。
    pub program: PathBuf,
    /// 参数，例如 `["games/adapters/minigrid/driver.py", "--manifest", "…"]`。
    pub args: Vec<String>,
    /// 该进程承载的游戏。宿主据此校验动作域。
    pub game: GameKind,
    /// 工作目录。
    pub working_directory: Option<PathBuf>,
    /// 超时。
    pub timeouts: EngineTimeouts,
}

impl ProcessEngineConfig {
    /// 用仓库根下的虚拟环境启动某个游戏。
    pub fn new(program: impl Into<PathBuf>, entry: &str, game: GameKind) -> Self {
        Self {
            program: program.into(),
            args: vec!["-B".to_string(), "-X".to_string(), "utf8".to_string(), entry.to_string()],
            game,
            working_directory: None,
            timeouts: EngineTimeouts::default(),
        }
    }
}

/// 每回合一个进程的引擎工厂。
///
/// 一个回合一个进程是有意的：进程之间不共享任何内存，第 N 局的隐藏状态不可能泄漏到
/// 第 N+1 局。代价是每局一次进程启动；对评测频率来说这个开销可以接受，而它换来的隔离
/// 不是靠纪律维持的。
#[derive(Debug, Clone)]
pub struct ProcessFactory {
    config: ProcessEngineConfig,
}

impl ProcessFactory {
    /// 用给定的启动配置建工厂。
    pub fn new(config: ProcessEngineConfig) -> Self {
        Self { config }
    }
}

impl EngineFactory for ProcessFactory {
    fn create(&self, game: GameKind, _seed: u64) -> Result<Box<dyn Engine>, EngineError> {
        if game != self.config.game {
            return Err(EngineError::Unavailable {
                reason: format!(
                    "该工厂绑定的是 {:?}，收到 {:?}",
                    self.config.game, game
                ),
            });
        }
        Ok(Box::new(ProcessEngine::spawn(self.config.clone())?))
    }
}

/// 一个游戏进程。
pub struct ProcessEngine {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    config: ProcessEngineConfig,
    /// `Some(reason)` 表示引擎已经不可再用。§11.4：超时不代表动作未执行，不能盲重试。
    unusable: Option<String>,
    /// 进程自报的规则版本，握手时取得。
    rules_version: Option<String>,
}

impl std::fmt::Debug for ProcessEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessEngine")
            .field("game", &self.config.game)
            .field("rules_version", &self.rules_version)
            .field("unusable", &self.unusable)
            .finish_non_exhaustive()
    }
}

impl ProcessEngine {
    /// 启动游戏进程并完成握手。
    pub fn spawn(config: ProcessEngineConfig) -> Result<Self, ProcessEngineError> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 引擎的日志只进私有评估域；公开面只有 stdout 上的协议消息（§13）。
            .stderr(Stdio::null());
        if let Some(directory) = &config.working_directory {
            command.current_dir(directory);
        }

        let mut child = command.spawn().map_err(ProcessEngineError::Spawn)?;
        let stdin = child.stdin.take().ok_or(ProcessEngineError::Unusable {
            reason: "无法取得子进程标准输入".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or(ProcessEngineError::Unusable {
            reason: "无法取得子进程标准输出".to_string(),
        })?;

        let mut engine = Self {
            child,
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            config,
            unusable: None,
            rules_version: None,
        };

        let facts = engine.exchange(&json!({ "type": "facts" }), engine.config.timeouts.facts)?;
        if facts.get("type").and_then(Value::as_str) != Some("facts_result") {
            return Err(ProcessEngineError::MissingField {
                field: "facts_result",
            });
        }
        engine.rules_version = facts
            .get("rules_version")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        engine.check_game(&facts)?;
        Ok(engine)
    }

    /// 进程自报的规则版本。
    pub fn rules_version(&self) -> Option<&str> {
        self.rules_version.as_deref()
    }

    /// 引擎当前是否可用。
    pub fn is_usable(&self) -> bool {
        self.unusable.is_none()
    }

    /// 关闭游戏进程。
    pub fn shutdown(&mut self) -> Result<(), ProcessEngineError> {
        if self.unusable.is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            return Ok(());
        }
        let response = self.exchange(&json!({ "type": "close" }), self.config.timeouts.facts)?;
        if response.get("type").and_then(Value::as_str) != Some("close_result") {
            return Err(ProcessEngineError::MissingField {
                field: "close_result",
            });
        }
        let _ = self.child.wait();
        Ok(())
    }

    fn check_game(&self, facts: &Value) -> Result<(), ProcessEngineError> {
        let reported = facts
            .get("game")
            .and_then(Value::as_str)
            .ok_or(ProcessEngineError::MissingField { field: "game" })?;
        let expected = match self.config.game {
            GameKind::Maze => "maze",
            GameKind::Minesweeper => "minesweeper",
        };
        if reported != expected {
            return Err(ProcessEngineError::Refused {
                kind: "handshake".to_string(),
                reason: format!("进程自报游戏 {reported}，配置为 {expected}"),
            });
        }
        Ok(())
    }

    /// 发一条请求并等一条响应。
    fn exchange(
        &mut self,
        message: &Value,
        timeout: Duration,
    ) -> Result<Value, ProcessEngineError> {
        if let Some(reason) = &self.unusable {
            return Err(ProcessEngineError::Unusable {
                reason: reason.clone(),
            });
        }

        let stdin = self.stdin.as_mut().ok_or(ProcessEngineError::Unusable {
            reason: "标准输入已经关闭".to_string(),
        })?;
        write_frame(stdin, message)?;

        let mut reader = self.stdout.take().ok_or(ProcessEngineError::Unusable {
            reason: "标准输出已经被占用".to_string(),
        })?;
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let result = read_frame(&mut reader);
            let _ = sender.send(result);
            reader
        });

        match receiver.recv_timeout(timeout) {
            Ok(result) => {
                if let Ok(reader) = handle.join() {
                    self.stdout = Some(reader);
                } else {
                    self.unusable = Some("读取线程异常结束".to_string());
                }
                result
            }
            Err(_) => {
                // 超时：杀掉子进程让读线程解锁。引擎从此不可用——超时不代表动作没执行，
                // 调用方必须回去查幂等账（§11.4）。
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.unusable = Some(format!("等待响应超过 {timeout:?}"));
                Err(ProcessEngineError::Timeout {
                    seconds: timeout.as_secs_f64(),
                })
            }
        }
    }

    /// 解析一条 `*_result` 响应里的 `step`。
    fn decode_step(response: &Value) -> Result<EngineStep, ProcessEngineError> {
        reject_error(response)?;
        let step = response
            .get("step")
            .ok_or(ProcessEngineError::MissingField { field: "step" })?;
        serde_json::from_value(step.clone()).map_err(ProcessEngineError::Malformed)
    }
}

impl Engine for ProcessEngine {
    fn game(&self) -> GameKind {
        self.config.game
    }

    fn reset(&mut self, seed: u64) -> Result<EngineStep, EngineError> {
        let response = self.exchange(&json!({ "type": "reset", "seed": seed }), self.config.timeouts.reset)?;
        Ok(Self::decode_step(&response)?)
    }

    fn step(&mut self, action: GameAction) -> Result<EngineStep, EngineError> {
        let action = serde_json::to_value(action).map_err(|error| EngineError::Unavailable {
            reason: error.to_string(),
        })?;
        let response = self.exchange(
            &json!({ "type": "step", "action": action }),
            self.config.timeouts.step,
        )?;
        Ok(Self::decode_step(&response)?)
    }

    fn supports_snapshot(&self) -> bool {
        // games/*/manifest.json 一律声明 supports_snapshot=false，直到某个适配器的
        // 完整局面恢复被真正验证过。§13 不允许"能 reset 就假装能接着玩"。
        false
    }
}

impl Drop for ProcessEngine {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// 把 error 响应翻译成引擎错误。
fn reject_error(response: &Value) -> Result<(), ProcessEngineError> {
    if response.get("type").and_then(Value::as_str) != Some("error") {
        return Ok(());
    }
    Err(ProcessEngineError::Refused {
        kind: response
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        reason: response
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("引擎未给出原因")
            .to_string(),
    })
}

fn write_frame(writer: &mut ChildStdin, message: &Value) -> Result<(), ProcessEngineError> {
    let payload = serde_json::to_vec(message).map_err(ProcessEngineError::Malformed)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProcessEngineError::FrameTooLarge {
            declared: u32::try_from(payload.len()).unwrap_or(u32::MAX),
            limit: MAX_FRAME_BYTES,
        });
    }
    writer
        .write_all(&(payload.len() as u32).to_le_bytes())
        .map_err(ProcessEngineError::Io)?;
    writer.write_all(&payload).map_err(ProcessEngineError::Io)?;
    writer.flush().map_err(ProcessEngineError::Io)
}

fn read_frame(reader: &mut BufReader<ChildStdout>) -> Result<Value, ProcessEngineError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ProcessEngineError::Exited
        } else {
            ProcessEngineError::Io(error)
        }
    })?;
    let declared = u32::from_le_bytes(header);
    // 先检查长度再分配（§11.4）。
    if declared as usize > MAX_FRAME_BYTES {
        return Err(ProcessEngineError::FrameTooLarge {
            declared,
            limit: MAX_FRAME_BYTES,
        });
    }
    let mut payload = vec![0u8; declared as usize];
    reader
        .read_exact(&mut payload)
        .map_err(ProcessEngineError::Io)?;
    serde_json::from_slice(&payload).map_err(ProcessEngineError::Malformed)
}

