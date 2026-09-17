//! 确定性模拟 OS（P0 的"模拟 OS"工作包，§16）。
//!
//! P0 阶段不碰真实文件系统，原因不是省事，而是可判定性：§17 要求"100 个确定性模拟任务的
//! 预测先于动作、动作实际结果后验检查"全部通过。真实文件系统的并发、时间戳精度与权限差异
//! 会让"是架构错了还是环境抖了"变成无法回答的问题。模拟 OS 把环境变成纯函数，故障也变成
//! 可复现的配置。
//!
//! 两条本模块负责保证的语义：
//!
//! * **动作级幂等**：同一个 `action_id` 只应用一次副作用（§7.3 的"执行代理以动作 ID 去重"）。
//! * **故障可注入且可复现**：可以精确指定"第 N 次应用之后断电"，从而稳定复现
//!   §7.3 的 `UNKNOWN_COMMIT` 场景。

use std::collections::BTreeMap;

use serde_json::Value;
use soca_contracts::{ActionIntent, Sha256Hex};

/// 模拟文件系统里的一个对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectState {
    /// 当前内容。
    pub content: String,
    /// 当前版本。初值是空内容的哈希，与"文件存在但为空"区分见 [`ObjectState::exists`]。
    pub version: Sha256Hex,
    /// 已被写入的次数。
    pub writes: u64,
    /// 对象是否已经存在。被删除后为 `false`。
    pub exists: bool,
}

impl ObjectState {
    /// 以给定内容构造一个存在的对象。
    pub fn new(content: &str) -> Self {
        Self {
            content: content.to_string(),
            version: Sha256Hex::of_bytes(content.as_bytes()),
            writes: 0,
            exists: true,
        }
    }
}

/// 一次执行尝试的记录。
///
/// 保留**全部**尝试（包括失败与重复）是刻意的：断言"副作用只发生一次"必须能区分
/// "尝试了两次但只应用了一次"和"只尝试了一次"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// 动作标识。
    pub action_id: String,
    /// 工具。
    pub tool_id: String,
    /// 作用对象。
    pub subject_ref: String,
    /// 本次尝试的结果。
    pub outcome: AttemptOutcome,
}

/// 尝试结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// 副作用已应用。
    Applied {
        /// 应用后的版本。
        version: String,
    },
    /// 该动作 ID 之前已经应用过，本次没有再次应用。
    AlreadyApplied {
        /// 之前应用产生的版本。
        version: String,
    },
    /// 未应用，工具拒绝了这次调用。
    Failed {
        /// 拒绝原因。
        reason: String,
    },
    /// 副作用已应用，但调用方没有拿到结果（模拟断电）。
    Interrupted {
        /// 应用后的版本。
        version: String,
    },
}

impl AttemptOutcome {
    /// 本次尝试是否真的改变了世界。
    pub fn applied(&self) -> bool {
        matches!(
            self,
            Self::Applied { .. } | Self::Interrupted { .. }
        )
    }
}

/// 故障计划。默认不注入任何故障。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultPlan {
    /// 第 N 次"真正应用"之后断电：副作用已发生，调用方拿不到回执。
    pub interrupt_after_application: Option<usize>,
    /// 第 N 次执行在应用之前失败：副作用未发生。
    pub fail_before_application: Option<usize>,
}

/// 被模拟的环境。
#[derive(Debug)]
pub struct SimulatedOs {
    objects: BTreeMap<String, ObjectState>,
    attempts: Vec<Attempt>,
    /// 动作 ID -> 已经应用产生的版本。动作级幂等的依据。
    applied: BTreeMap<String, String>,
    /// 已经真正应用过副作用的次数。
    applications: usize,
    faults: FaultPlan,
}

impl Default for SimulatedOs {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedOs {
    /// 空环境。
    pub fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
            attempts: Vec::new(),
            applied: BTreeMap::new(),
            applications: 0,
            faults: FaultPlan::default(),
        }
    }

    /// 带故障计划的环境。
    pub fn with_faults(faults: FaultPlan) -> Self {
        Self {
            faults,
            ..Self::new()
        }
    }

    /// 预置一个已存在的对象。
    pub fn seed(&mut self, subject_ref: &str, content: &str) {
        self.objects
            .insert(subject_ref.to_string(), ObjectState::new(content));
    }

    /// 读取对象。返回 `None` 表示该对象不存在。
    pub fn read(&self, subject_ref: &str) -> Option<&ObjectState> {
        self.objects.get(subject_ref).filter(|state| state.exists)
    }

    /// 当前版本，对象不存在时返回 `None`。
    pub fn version_of(&self, subject_ref: &str) -> Option<&str> {
        self.read(subject_ref).map(|state| state.version.as_str())
    }

    /// 全部尝试记录。
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    /// 真正改变过世界的次数。
    pub fn applications(&self) -> usize {
        self.applications
    }

    /// 清空尝试记录。用于在重放前后对比"有没有重新执行"。
    pub fn reset_attempts(&mut self) {
        self.attempts.clear();
    }

    /// 执行一次工具调用。
    ///
    /// 注意本方法不检查任何权限：权限属于执行代理（[`crate::broker`]）。模拟 OS 只回答
    /// "这个世界会怎么变"，不回答"你允不允许"。
    pub fn execute(&mut self, intent: &ActionIntent) -> AttemptOutcome {
        let action_id = intent.action_id.to_string();
        let tool_id = intent.tool_id.to_string();

        let Some((subject_ref, content)) = parse_write(&intent.parameters) else {
            let outcome = AttemptOutcome::Failed {
                reason: "fs.write 需要字符串字段 path 与 content".to_string(),
            };
            self.attempts.push(Attempt {
                action_id,
                tool_id,
                subject_ref: "<无法解析>".to_string(),
                outcome: outcome.clone(),
            });
            return outcome;
        };

        // 动作级幂等：同一动作 ID 不产生第二次副作用（§7.3）。
        if let Some(version) = self.applied.get(&action_id) {
            let outcome = AttemptOutcome::AlreadyApplied {
                version: version.clone(),
            };
            self.attempts.push(Attempt {
                action_id,
                tool_id,
                subject_ref,
                outcome: outcome.clone(),
            });
            return outcome;
        }

        let execution_ordinal = self.attempts.len() + 1;
        if self.faults.fail_before_application == Some(execution_ordinal) {
            let outcome = AttemptOutcome::Failed {
                reason: "注入故障：应用之前失败".to_string(),
            };
            self.attempts.push(Attempt {
                action_id,
                tool_id,
                subject_ref,
                outcome: outcome.clone(),
            });
            return outcome;
        }

        // 应用副作用。
        let version = Sha256Hex::of_bytes(content.as_bytes());
        let entry = self
            .objects
            .entry(subject_ref.clone())
            .or_insert_with(|| ObjectState::new(""));
        entry.content = content;
        entry.version = version.clone();
        entry.writes += 1;
        entry.exists = true;

        self.applications += 1;
        self.applied.insert(action_id.clone(), version.to_string());

        let outcome = if self.faults.interrupt_after_application == Some(self.applications) {
            AttemptOutcome::Interrupted {
                version: version.to_string(),
            }
        } else {
            AttemptOutcome::Applied {
                version: version.to_string(),
            }
        };

        self.attempts.push(Attempt {
            action_id,
            tool_id,
            subject_ref,
            outcome: outcome.clone(),
        });
        outcome
    }
}

/// 从结构化参数里取出 `path` 与 `content`。
fn parse_write(parameters: &Value) -> Option<(String, String)> {
    let path = parameters.get("path")?.as_str()?;
    let content = parameters.get("content")?.as_str()?;
    Some((format!("file:{path}"), content.to_string()))
}

/// 写入类工具作用的对象引用。
///
/// 观测、预测与核验必须使用同一个引用，核验才能逐字比较；让三者都调用本函数，而不是各自
/// 拼一遍 `format!("file:{}", path)`。
pub fn write_subject_ref(parameters: &Value) -> Option<String> {
    parse_write(parameters).map(|(subject_ref, _)| subject_ref)
}

/// 写入类工具将要写下的内容。
pub fn write_content(parameters: &Value) -> Option<String> {
    parse_write(parameters).map(|(_, content)| content)
}
