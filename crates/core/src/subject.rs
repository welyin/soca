//! 单主体运行时：把各层接成一个能跑的闭环（§4.1、§6、§8）。
//!
//! 前面各层各自可用，但"一个主体"此前并不存在。本模块把它接起来，并且刻意只接**已有的**
//! 东西——没有任何一步是"为了看起来完整"而补的空壳：
//!
//! ```text
//!  用户消息 ──delegate──▶ L6 目标栈（§4.1 L6：出处必须是用户明确通道）
//!                            │
//!  环境读取 ──observe────▶ 事件账（§6 第 1 步）＋ L2 黑板（§4.1 L2）
//!                            │
//!                            ▼
//!            L1 叶单元 propose ──▶ L2 证据存在性校验（§6 第 4 步）
//!                            │
//!                            ▼
//!     上下文编译（§8 七项）──▶ 模型网关 ──▶ 返回物校验（§8"先解析和校验"）
//!                            │
//!                            ▼
//!                    候选（不是决定，§3.1）
//! ```
//!
//! 三处刻意的选择：
//!
//! 1. **[`Subject::observed`] 是编译上下文时唯一被承认的证据来源。** 不是"从各处汇总"。
//!    §8 要求证据在 Core 与存储中而不只在上下文窗口里；一个能容纳来路不明证据的编译器，
//!    等于把"模型不能引用它没看到的东西"这条边界从里面拆掉。
//! 2. **先扣额度，再做可能很久的事。** 反过来会让一次超时的调用白白消耗一个已经排在后面的
//!    目标的额度，而 §4.2 要求超限时"请求预算升级或返回部分结果"——那需要一个准确的计数。
//! 3. **咨询模型产出的是候选，不是动作。** §3.1："它提出假设和动作，不独占信念、记忆、
//!    权限、预算或执行权。" 从候选到副作用之间还有 L4 与执行许可两道关。

use serde::Serialize;
use soca_contracts::{
    ActionOutcomeSlice, ActionLevel, BeliefSummary, Candidate, CandidateSet, CapabilitySlice,
    CognitiveUnit, ContextBundle, DataClass, EvidenceSlice, ExplorationQuota, GoalBudget, GoalId,
    GoalStack, ModelBackend, ModelBudget, ModelOutput, ModelVersion, Observation, OutputSchema,
    PermissionScope, Provenance, TaskId, ToolId, UserChannel, WallClock, MAX_CONTEXT_EVIDENCE,
};
use soca_core_actors::DesktopAndFilesCluster;
use soca_model_gateway::{ContextCompiler, ContextInput, ModelGateway, Transport};
use soca_storage::Store;

use crate::broker::ActionBroker;
use crate::error::CoreError;
use crate::session::{ObservationRecord, Session};

/// 本版通过认知循环可用的工具。
///
/// 必须与模拟 OS 实际支持的工具一致。不一致的话，模型会提出一个执行不了的调用，而那种失败
/// 要等到执行代理那里才暴露——白花一次往返，而且模型学到的是一条错的"我能做什么"。
const AVAILABLE_TOOLS: &[&str] = &["fs.read", "fs.write"];

/// 一次模型咨询的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct ModelConsultation {
    /// 为哪个目标咨询的。
    pub goal_id: GoalId,
    /// 实际发出去的上下文。界面与审计都要能看见**模型当时看到了什么**。
    pub context: ContextBundle,
    /// 返回物。
    pub output: ModelOutput,
    /// 尝试了几次。
    pub attempts: u8,
}

/// 供界面读取的目标摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoalSummary {
    /// 标识。
    pub goal_id: String,
    /// 陈述。
    pub statement: String,
    /// 状态。
    pub state: &'static str,
    /// 深度。
    pub depth: u8,
    /// 剩余激活次数。
    pub remaining_activations: u32,
    /// 剩余探索次数。
    pub remaining_explorations: u32,
    /// 是否已经超过截止时间。
    pub expired: bool,
}

/// 供界面读取的公开状态。
///
/// 刻意只放**公开**的东西：目标、黑板主题数、证据数、记忆数、动作数。§13 要求真值与
/// 调试信息不进 agent 的输入通路，反过来同样成立——界面也不该拿到比 agent 更多的东西，
/// 否则"界面上的数字"与"agent 的依据"会开始互相解释。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PublicState {
    /// 主体。
    pub owner: String,
    /// 目标。
    pub goals: Vec<GoalSummary>,
    /// L2 黑板上的主题数。
    pub workspace_topics: usize,
    /// L2 黑板上的证据数。
    pub workspace_evidence: usize,
    /// 本主体观测到的证据数。
    pub observed_evidence: usize,
    /// 可见记忆条数。
    pub memory_entries: usize,
    /// 动作账条数。
    pub actions: i64,
    /// 累计模型调用次数（含重试）。
    pub model_calls: u64,
}

/// 单主体运行时。
pub struct Subject {
    store: Store,
    broker: ActionBroker,
    cluster: DesktopAndFilesCluster,
    goals: GoalStack,
    gateway: ModelGateway<Box<dyn Transport>>,
    owner: soca_contracts::SubjectId,
    task: TaskId,
    boot: soca_contracts::BootId,
    /// 是否已获准把上下文发往远端（§8）。
    ///
    /// 只在构造时确定，不提供运行时开关：§8 的云端授权是一种策略，不是一个随手可以翻的
    /// 布尔量。要改变它，得重新装配主体，而那次装配会留下记录。
    remote_authorized: bool,
    /// 本主体实际观测到的证据。上下文编译只从这里取。
    ///
    /// 这是一个**有界的近期窗口**，不是长期记忆——长期记忆是 L5 的职责。让它无限增长的话，
    /// 一个跑了一天的进程会攒下几万条，而 §17 的长时运行验收要求"内存和日志增长符合配额"。
    observed: Vec<EvidenceSlice>,
}

impl std::fmt::Debug for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Box<dyn Transport>` 不是 `Debug`，所以手写实现，只输出标识与计数。
        f.debug_struct("Subject")
            .field("owner", &self.owner)
            .field("goals", &self.goals.len())
            .field("observed", &self.observed.len())
            .field("workspace_topics", &self.cluster.workspace().topic_count())
            .finish()
    }
}

impl Subject {
    /// 装配一个主体。
    ///
    /// 传输层收 `Box<dyn Transport>` 而不是泛型：§5 要求模型服务"按模型家族 0–2 个起步，
    /// 不按单元数启动"，也就是许多主体共享同一个模型服务。装箱之后，换后端不需要重新编译
    /// 主体这一层。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Store,
        broker: ActionBroker,
        cluster: DesktopAndFilesCluster,
        owner: soca_contracts::SubjectId,
        boot: soca_contracts::BootId,
        transport: Box<dyn Transport>,
        backend: ModelBackend,
        remote_authorized: bool,
        budget: ModelBudget,
        model_version: ModelVersion,
    ) -> Result<Self, CoreError> {
        let gateway = ModelGateway::new(transport, backend, remote_authorized, budget, model_version)?;
        let task = TaskId::new(format!("task:{}", owner.as_str()))?;
        Ok(Self {
            store,
            broker,
            cluster,
            goals: GoalStack::new(owner.clone()),
            gateway,
            owner,
            task,
            boot,
            remote_authorized,
            observed: Vec::new(),
        })
    }

    /// 主体标识。
    pub fn owner(&self) -> &soca_contracts::SubjectId {
        &self.owner
    }

    /// 目标栈（只读）。
    pub fn goals(&self) -> &GoalStack {
        &self.goals
    }

    /// 能力簇（只读）。
    pub fn cluster(&self) -> &DesktopAndFilesCluster {
        &self.cluster
    }

    /// 存储（只读）。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 存储（可变）。给界面之外的维护操作使用。
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// 执行代理（可变）。故障注入与恢复测试需要它。
    pub fn broker_mut(&mut self) -> &mut ActionBroker {
        &mut self.broker
    }

    /// 委托一个目标（§4.1 L6）。
    ///
    /// 走的是 [`GoalStack::delegate`]，因此"出处必须是用户明确通道"这条约束在主体这一层
    /// 无法被绕过——不是"记得传对参数"，是那条路本来就只有一条。
    #[allow(clippy::too_many_arguments)]
    pub fn delegate(
        &mut self,
        statement: impl Into<String>,
        channel: UserChannel,
        permission_scope: PermissionScope,
        budget: GoalBudget,
        exploration: ExplorationQuota,
        at: WallClock,
        deadline: Option<WallClock>,
    ) -> Result<GoalId, CoreError> {
        let goal_id = self.next_goal_id()?;
        self.goals.delegate(
            goal_id.clone(),
            statement,
            Provenance::User { channel },
            permission_scope,
            budget,
            exploration,
            at,
            deadline,
        )?;
        // 委托之后立刻落库：§13.3 要求目标属于主体，而"属于"意味着它能跨重启存在。
        self.store.save_goal_stack(&self.goals, at)?;
        Ok(goal_id)
    }

    /// 受理一个目标，让它从 `Proposed` 变成 `Active`。
    pub fn accept(&mut self, goal_id: &GoalId, at: WallClock) -> Result<(), CoreError> {
        self.goals
            .transition(goal_id, soca_contracts::GoalState::Active)?;
        self.store.save_goal_stack(&self.goals, at)?;
        Ok(())
    }

    /// 让一个目标及其全部后代结束（§6 第 9 步）。
    pub fn abandon(&mut self, goal_id: &GoalId, at: WallClock) -> Result<usize, CoreError> {
        let abandoned = self.goals.abandon(goal_id)?;
        self.store.save_goal_stack(&self.goals, at)?;
        Ok(abandoned)
    }

    /// §6 第 1–3 步：读取环境状态，写成事件，并喂给能力簇。
    ///
    /// 两条路共用同一份信封。分开构造的话，"记进账的那份"和"单元看到的那份"会在某个字段上
    /// 悄悄分叉，而那种分叉不会报错，只会让重放对不上。
    pub fn observe(
        &mut self,
        subject_ref: &str,
        data_class: DataClass,
        at: WallClock,
    ) -> Result<ObservationRecord, CoreError> {
        let (record, envelope) = {
            let unit = self.cluster.unit_id().clone();
            let task = self.task.clone();
            let mut session = Session::new(&mut self.store, &mut self.broker, unit, task, self.boot);
            session.observe_event(subject_ref, at)?
        };

        let Observation {
            subject,
            value,
            evidence_ref,
            ..
        } = &record.observation;
        self.remember_evidence(EvidenceSlice {
            evidence_ref: evidence_ref.clone(),
            subject_ref: subject.clone(),
            observed_value: value.clone(),
            data_class,
        });
        self.cluster.observe(&envelope, at)?;
        Ok(record)
    }

    /// 把一条证据放进池子，只保留最近 [`MAX_CONTEXT_EVIDENCE`] 条。
    ///
    /// 同一个引用不会被记两次；但**同一次观测永远产生新的引用**（`obs:{event_id}`），
    /// 所以"同一对象看两眼"确实是两份证据，而不是一份被覆盖。这是有意的：第二次观测发生在
    /// 另一个时刻，它证明的是那一刻的事实。去重去的是重复的回引，不是重复的观测。
    fn remember_evidence(&mut self, slice: EvidenceSlice) {
        if self
            .observed
            .iter()
            .any(|existing| existing.evidence_ref == slice.evidence_ref)
        {
            return;
        }
        self.observed.push(slice);
        if self.observed.len() > MAX_CONTEXT_EVIDENCE {
            self.observed.remove(0);
        }
    }

    /// §6 第 4 步：提出候选，并过 L2 的证据存在性校验。
    pub fn propose(&self, at: WallClock) -> Result<CandidateSet, CoreError> {
        Ok(self.cluster.propose(at)?)
    }

    /// §8：编译上下文、咨询模型、校验返回物。
    ///
    /// 返回的是**候选**。从候选到副作用之间还有 L4 与执行许可两道关，本方法一道也不碰。
    pub fn consult_model(
        &mut self,
        goal_id: &GoalId,
        output_schema: OutputSchema,
        at: WallClock,
    ) -> Result<ModelConsultation, CoreError> {
        let goal = self
            .goals
            .goal(goal_id)
            .ok_or_else(|| CoreError::GoalNotFound {
                goal_id: goal_id.to_string(),
            })?
            .clone();

        // 先扣额度。反过来的话，一次超时的调用会白白消耗一个排在后面的目标的额度，
        // 而 §4.2 要求超限时给出准确的计数以便"请求预算升级或返回部分结果"。
        self.goals.activate(goal_id)?;

        let candidates = self.cluster.propose(at)?;
        let belief: Vec<BeliefSummary> = candidates
            .candidates
            .iter()
            .filter_map(|candidate| match candidate {
                Candidate::Claim {
                    statement,
                    evidence_refs,
                } => Some(BeliefSummary {
                    statement: statement.clone(),
                    evidence_refs: evidence_refs.clone(),
                }),
                _ => None,
            })
            .collect();

        // §8 的"过去动作结果"来自动作账，不是来自本模块自己记的一份计数器。
        let past_outcomes: Vec<ActionOutcomeSlice> = self
            .store
            .recent_outcomes(16)?
            .into_iter()
            .map(|record| ActionOutcomeSlice {
                action_id: record.action_id,
                tool_id: record.tool_id,
                verdict: record.verdict,
                observation_refs: record.observation_refs,
            })
            .collect();

        let capabilities = CapabilitySlice {
            tool_ids: AVAILABLE_TOOLS
                .iter()
                .map(|name| ToolId::new(*name))
                .collect::<Result<Vec<_>, _>>()?,
            max_action_level: goal.permission_scope.max_action_level,
        };

        let deadline = goal.deadline.unwrap_or_else(|| at.plus_seconds(600));
        let input = ContextInput {
            goal: goal.statement.clone(),
            evidence: self.observed.clone(),
            belief,
            past_outcomes,
            capabilities,
            deadline,
            output_schema,
            recorded_predictions: self.recorded_predictions()?,
        };

        // 证据池里可能出现等级高于目标允许范围的内容。这里不做"过滤掉那条"，而是把
        // 类别最高的那一条如实带出去，让出站判断去拒绝——过滤后模型看到的结论会缺少依据。
        let context = ContextCompiler::new(self.gateway.backend(), self.is_remote_authorized())
            .compile(input)?;

        let profile = soca_contracts::ModelProfileRef::new("profile:reasoning")?;
        let validated = self.gateway.invoke(&context, profile)?;

        // token 用量记进目标额度。超预算时返回错误而不是"继续但不管账"。
        self.goals
            .goal_mut(goal_id)
            .expect("刚刚读过，必然还在")
            .budget
            .spend_tokens(validated.output.total_tokens())?;

        Ok(ModelConsultation {
            goal_id: goal_id.clone(),
            context,
            output: validated.output,
            attempts: validated.attempts,
        })
    }

    /// 记一次探索（§4.1 L6 的探索配额）。
    pub fn explore(&mut self, goal_id: &GoalId, at: WallClock) -> Result<(), CoreError> {
        self.goals.explore(goal_id)?;
        self.store.save_goal_stack(&self.goals, at)?;
        Ok(())
    }

    /// 把目标栈写回存储。
    pub fn checkpoint(&mut self, at: WallClock) -> Result<u64, CoreError> {
        Ok(self.store.save_goal_stack(&self.goals, at)?)
    }

    /// 从存储读回目标栈。重启之后调用它来恢复。
    pub fn restore_goals(&mut self) -> Result<bool, CoreError> {
        match self.store.goal_stack(&self.owner)? {
            Some(stack) => {
                self.goals = stack;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 供界面读取的公开状态。
    pub fn public_state(&self, at: WallClock) -> Result<PublicState, CoreError> {
        let goals = self
            .goals
            .iter()
            .map(|goal| GoalSummary {
                goal_id: goal.goal_id.to_string(),
                statement: goal.statement.clone(),
                state: goal.state.as_str(),
                depth: goal.depth,
                remaining_activations: goal.budget.remaining_activations(),
                remaining_explorations: goal.exploration.remaining(),
                expired: goal.is_expired_at(at),
            })
            .collect();

        let workspace = self.cluster.workspace();
        Ok(PublicState {
            owner: self.owner.to_string(),
            goals,
            workspace_topics: workspace.topic_count(),
            workspace_evidence: workspace.evidence().len(),
            observed_evidence: self.observed.len(),
            memory_entries: self.store.memory_count(&self.owner)?,
            actions: self.store.action_count()?,
            model_calls: self.gateway.calls(),
        })
    }

    /// 已记录在案、可供模型引用的预测。
    ///
    /// 从动作账里读，而不是由本模块自己维护一份清单：§6.3 的预测是**单元**写下的，
    /// 主体再记一份就会有两个来源，而两者迟早会不一致。
    fn recorded_predictions(&self) -> Result<Vec<soca_contracts::PredictionRef>, CoreError> {
        let mut seen: Vec<soca_contracts::PredictionRef> = Vec::new();
        for record in self.store.actions_for_task(&self.task, 16)? {
            let reference = soca_contracts::PredictionRef::new(record.prediction_ref)?;
            if !seen.contains(&reference) {
                seen.push(reference);
            }
        }
        Ok(seen)
    }

    /// 目标栈上的下一个可用标识。
    ///
    /// 目标只被标记结束、从不从栈里移除，所以"已用标识"是一个只增不减的集合；从一个下界
    /// 往上找第一个空位即可，不需要随机 UUID。
    fn next_goal_id(&self) -> Result<GoalId, CoreError> {
        let mut serial = self.goals.len() + 1;
        loop {
            let candidate = GoalId::new(format!("goal:{serial}"))?;
            if self.goals.goal(&candidate).is_none() {
                return Ok(candidate);
            }
            serial = serial.saturating_add(1);
        }
    }

    /// 是否已获准把上下文发往远端。
    ///
    /// 网关自己知道这件事，但编译发生在调用之前，所以主体要能把它传给编译器。这条信息
    /// 只从构造时的那一个来源读，不提供运行时开关——§8 的云端授权是一种策略，不是一个
    /// 随手可以翻的布尔量。
    fn is_remote_authorized(&self) -> bool {
        self.remote_authorized
    }

    /// 可用动作等级的上限（供界面显示"这个主体现在最多能做到什么"）。
    pub fn max_action_level(&self) -> ActionLevel {
        self.goals
            .open_goals()
            .iter()
            .map(|goal| goal.permission_scope.max_action_level)
            .max()
            .unwrap_or(ActionLevel::A0)
    }
}
