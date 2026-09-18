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

use std::collections::BTreeMap;

use serde::Serialize;
use soca_contracts::{
    ActionId, ActionIntent, ActionLevel, ActionOutcomeSlice, Approval, BeliefSummary, BudgetRef,
    Candidate, CandidateSet, CapabilityPolicyRef, CapabilitySlice, CognitiveUnit, ContextBundle,
    ContractError, DataClass, DerivationKind, EgressPolicy, Envelope, EventId, EvidenceRef,
    EvidenceSlice, Expectation, ExplorationQuota, GrantScope, GoalBudget, GoalId, GoalStack,
    GoalState, IdempotencyKey, MediaType, MemoryEntry, MemoryId,
    MemoryKind, ModelBackend, ModelBudget, ModelOutput, ModelVersion, Monotonic, Observation,
    OutputSchema, PayloadRef, PermitId, PermissionScope, PredictionRef, Provenance, ResourceCost,
    ResourceScope, Selection, SelectionOutcome, SelectionPolicy, Sha256Hex, SourceId, TaskId,
    ToolId, UserChannel, WallClock, MAX_CONTEXT_EVIDENCE, select as select_candidate,
};
use soca_core_actors::{DesktopAndFilesCluster, ReviewPolicy, review_all};
use soca_model_gateway::{ContextCompiler, ContextInput, ModelGateway, Transport};
use soca_storage::audit::AuditCategory;
use soca_storage::{ContentRetention, ContentStore, Store};

use crate::broker::ActionBroker;
use crate::error::CoreError;
use crate::policy::{PermitDecision, PolicyAgent};
use crate::retention::{
    expire_retained, forget as forget_memory, purge_retained, RetentionPolicy, RetentionReport,
};
use crate::session::DispatchOutcome;
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

/// 本次会话里最多记住多少条"已推进过的结论"。
pub const MAX_HANDLED_CLAIMS: usize = 128;

/// 写入类工具的标识。§12.2 禁止的是任意拼接的 shell 字符串，不是一个结构化的写入工具。
pub const WRITE_TOOL: &str = "fs.write";

/// 对话内容的默认保留天数（§12.3 的初值）。
pub const CONVERSATION_RETENTION_DAYS: i64 = 7;

/// 一次能力撤回的报告（§12.1、§12.3）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RevocationReport {
    /// 撤回之前它是否还在授权内。`false` 表示这是一次幂等重放。
    pub was_granted: bool,
    /// 这个能力覆盖了多少个事件。
    pub events_covered: usize,
    /// 有多少条记忆因此**立即**不可见。
    pub memories_invalidated: usize,
    /// 有多少条**还在能力簇手里的**证据因此不能再用来下结论（§7.2）。
    ///
    /// 与上一条分开报，是因为它们是撤回的两个不同后果，而只有前者时看起来像已经做完了。
    /// 记忆是"已经下过的结论"，簇手里的证据是"还能拿来下结论的材料"。
    pub evidence_retracted: usize,
    /// 当前等着清理的记忆条数。
    pub awaiting_purge: usize,
}

/// 一条读回来的用户输入。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UserInput {
    /// 事件标识。
    pub event_id: String,
    /// 通道。
    pub channel: &'static str,
    /// 收到时刻。
    pub at: WallClock,
    /// 正文。`None` 表示内容已按保留期被清理（§12.3）。
    ///
    /// **刻意留成 `None` 而不是报错。** 一段过期的对话不是"坏了"，它是按约定走完了自己的
    /// 保留期；把它报成读取失败，界面就只能说"出错了"，而用户需要知道的是"那段内容到期了"。
    /// 反过来，真正的损坏（元数据在册、字节却读不到）仍然报错，不会被这里吞掉——
    /// §9.3 要的是可诊断缺失，不是把所有缺失都说成"过期了"。
    pub text: Option<String>,
}

/// 用户输入事件流的 epoch。
///
/// 用户输入是本地产生的、不经过任何设备适配器，所以它没有"适配器换代"这回事。用一个固定
/// 值而不是 0 之外的别的东西：§7.1 只要求同一来源同一 epoch 内的序号可比，而给一个永远
/// 只有一个世代的流编造世代号，只会让读的人以为它有意义。
pub const USER_INPUT_EPOCH: u32 = 1;

/// 派生出记忆条目标识的种子：命题 + 排序后的证据集合。
///
/// 用 `\u{1f}`（单元分隔符）而不是逗号或空串连接，是为了让 `"ab" + "c"` 与 `"a" + "bc"`
/// 得到不同的种子。用可打印字符当分隔符的话，命题里本来就可能出现它。
fn derivation_seed(statement: &str, evidence_refs: &[soca_contracts::EvidenceRef]) -> String {
    let mut seed = statement.to_string();
    let mut refs: Vec<String> = evidence_refs
        .iter()
        .map(ToString::to_string)
        .collect();
    refs.sort_unstable();
    for reference in refs {
        seed.push('\u{1f}');
        seed.push_str(&reference);
    }
    seed
}

/// 一轮闭环的走向（§6 第 9 步）。
///
/// §6 第 9 步的原话是"有限预算内继续、回退、请求澄清或结束"。这些变体就是那句话的类型化
/// 形式——把"结束"与"空转"分开，是因为它们对界面与审计的含义不同：前者说明该收工了，
/// 后者说明这一轮什么也没发生但还可能继续。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoundOutcome {
    /// 按选中的候选推进了一步。
    Advanced {
        /// 走的是哪一步。
        step: AdvanceStep,
    },
    /// 需要外部输入（§6 第 9 步的"请求澄清"）。
    NeedsInput {
        /// 缺什么。
        missing: Vec<String>,
    },
    /// 没有可推进的候选，这一轮空转。
    Idle,
    /// 没有还能推进的目标了。§6 第 9 步的"结束"。
    Finished {
        /// 为什么结束。
        reason: &'static str,
    },
}

/// 一轮里实际做成了什么。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdvanceStep {
    /// 补了一次观测（§6 第 1 步）。
    Observation {
        /// 观测对象。
        subject_ref: String,
        /// 新产生的证据引用。
        evidence_ref: String,
    },
    /// 把一条结论写成了记忆（§4.1 L5、§13.2）。
    Claim {
        /// 命题。
        statement: String,
        /// 记忆条目标识。
        memory_id: String,
        /// 是否真的新增了条目。`false` 表示这条结论此前已记过（幂等）。
        recorded: bool,
    },
    /// 签发许可并执行了一次动作（§6 第 3–8 步）。
    Action {
        /// 动作标识。
        action_id: String,
        /// 工具。
        tool_id: String,
        /// 消耗的许可。
        permit_id: String,
        /// 执行回执状态。回执**不等于**后置条件已验证（§7.2）。
        receipt: String,
        /// 后置条件判定。`None` 表示结果未知或未执行。
        verdict: Option<String>,
    },
    /// 需要一次人工批准（§12.1）。目标已转入等待审批。
    NeedsApproval {
        /// 动作等级。
        level: String,
        /// 为什么还不能放行。
        reason: String,
    },
    /// 被策略代理拒绝。**再多的人工批准也改变不了它**（§12.2）。
    Refused {
        /// 拒绝原因。
        reason: String,
    },
    /// 这条候选需要一条本版还没有的通路。
    Unsupported {
        /// 候选种类。
        candidate_kind: String,
        /// 为什么走不通。
        reason: String,
    },
}

/// 一轮闭环的报告（§6）。
///
/// 与 [`crate::session::RoundReport`] 不是一回事：那个是一**次动作**走完 §6 第 3–8 步的报告，
/// 这个是**一轮认知循环**（观测 → 检验 → 选择 → 推进）的报告。一个认知轮里可以包含零个
/// 动作，也可以在一次动作上停住等审批。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoopRound {
    /// 第几轮（主体内部计数）。
    pub round: u64,
    /// 走向。
    pub outcome: RoundOutcome,
    /// 第 4–5 步的检验与选择。没有可推进的目标时为 `None`。
    pub selection: Option<Selection>,
    /// 选中的是第几条候选。
    pub selected: Option<usize>,
    /// 这一轮消耗的激活次数。
    pub activations: u32,
    /// 这一轮推进的目标。
    pub goal_id: Option<GoalId>,
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
    /// 能力簇证据台账里的条数。核对结论时手边有多少材料，看的是这个数。
    pub ledger_records: usize,
    /// 可见记忆条数。
    pub memory_entries: usize,
    /// 已经隐藏、等着清理的记忆条数（§12.3）。
    pub memories_awaiting_purge: usize,
    /// 动作账条数。
    pub actions: i64,
    /// 已投递、尚未推进的动作数（§12.1 的 A2 之类）。
    ///
    /// 界面需要它来回答"我说要做的那件事现在在哪"：队列里还有几个、是不是卡在审批上。
    /// 只看目标状态看不出来——目标可以是"进行中"而动作正等着批准。
    pub pending_actions: usize,
    /// 当前可用的人工批准数（未过期且有余量）。
    pub usable_approvals: usize,
    /// 累计模型调用次数（含重试）。
    pub model_calls: u64,
    /// 当前模型后端。
    pub backend: &'static str,
    /// 当前个人数据出站策略（§8）。界面要能显示它是 `strict` 还是已被用户放开。
    pub egress_policy: &'static str,
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
    /// 当前模型后端。
    backend: ModelBackend,
    /// 是否已获准把上下文发往远端（§8）。
    remote_authorized: bool,
    /// 个人数据的出站策略（§8）。默认 [`EgressPolicy::Strict`]。
    egress: EgressPolicy,
    /// 本主体实际观测到的证据。上下文编译只从这里取。
    ///
    /// 这是一个**有界的近期窗口**，不是长期记忆——长期记忆是 L5 的职责。让它无限增长的话，
    /// 一个跑了一天的进程会攒下几万条，而 §17 的长时运行验收要求"内存和日志增长符合配额"。
    observed: Vec<EvidenceSlice>,
    /// 已经跑过的轮数（§6 的闭环计数）。
    rounds: u64,
    /// 每个待推进动作归属于哪个目标。
    ///
    /// 没有它的话，为 A 目标投递的写入会在 B 目标下被核对与执行——权限范围按**当前**目标
    /// 算，而当前目标是 B。两者的检查都会通过，但结果仍然是错的：B 这个任务不该执行 A 的动作，
    /// 而用户放弃 A 的意图更不该在别处生效。
    action_goals: BTreeMap<String, GoalId>,
    /// 分段内容仓（§9.3）。用户输入与将来的大载荷都落在这里，信封只带引用。
    content: ContentStore,
    /// 新写入的对话内容的保留期（§12.3："本地可配置保留，初值 7 天"）。
    ///
    /// 注意它只作用于**之后**写入的内容：已经记下的保留期留在内容对象上不动。理由见迁移 7——
    /// 一段被承诺"留 7 天"的对话，不该因为用户第二天把设置改成 3 天就在今晚消失。
    conversation_retention: ContentRetention,
    /// 本次会话里已经写进记忆的结论，键是 [`derivation_seed`]。
    ///
    /// 这是 §6 第 2 步那个"路由器"的雏形。那句话是"按任务、权限、来源和预算**选择少数
    /// 单元**"，而本版只做了其中最小的一条：**同一条结论不重复推进**。
    ///
    /// 只过滤结论、不过滤观测请求，是因为两者的重复语义不同：重推一条已经记过的结论什么也
    /// 不改变（它是幂等的），而**再观测一次是合理的**——世界会变，同一对象的新观测正是
    /// "我先前判断错了"的证据来源。把观测也一并过滤掉，等于把那条路封死。
    handled_claims: Vec<String>,
    /// 策略代理（§12.2）。执行许可由它判定，而不是由调用方手搓。
    policy: PolicyAgent,
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
        // 内容仓放在事务库旁边（§9.3 把内容与元数据分开的那条理由）。内存存储没有路径，
        // 就退到一个临时仓——**那意味着重启之后内容就没了**，所以它只应当出现在测试与
        // 模拟里，而 `with_content_store` 给真实部署一个指定位置的机会。
        let content = match store.location() {
            Some(path) => ContentStore::open(path.with_extension("content"))?,
            None => ContentStore::temporary()?,
        };

        // §12.1 的"范围限定授权"。把默认能力收窄到守望对象所在的那一层。
        //
        // 为什么由主体来收窄而不是 `PolicyAgent::default()` 自己带一个范围：策略代理不知道
        // 这次任务守望哪个目录，而替它猜一个（"当前目录"？"用户主目录"？）会猜出一次
        // **比调用方以为的更宽**的授权——那正是最难发现的一类越权。
        // 收窄是一次显式的、有依据的动作，它属于知道守望对象是谁的那一层。
        let mut policy = PolicyAgent::default();
        if let Some(root) = cluster.authorized_root() {
            let capability = policy.default_capability().clone();
            policy.grant(capability, GrantScope::under(root)?);
        }

        Ok(Self {
            store,
            broker,
            cluster,
            goals: GoalStack::new(owner.clone()),
            gateway,
            owner,
            task,
            boot,
            backend,
            remote_authorized,
            egress: EgressPolicy::Strict,
            observed: Vec::new(),
            rounds: 0,
            action_goals: BTreeMap::new(),
            content,
            conversation_retention: ContentRetention::Days(CONVERSATION_RETENTION_DAYS),
            handled_claims: Vec::new(),
            policy,
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
        let statement = statement.into();

        // §6 第 1 步："L0 把用户输入或设备结果写成事件。"
        //
        // 在此之前，用户的话只活在目标陈述里——那不是"存下来了"，那是"被引用过一次"。
        // 目标改了、结束了、被放弃了，那句话就再没有地方可以回看；而 §7.1 的信封（来源、
        // 时刻、权限范围、数据类别）更是完全没施加到它身上。
        //
        // 顺序：先记话，再建目标。**目标建不出来时事件照样留着**——话是用户说的，
        // 这件事已经发生了；"我们没能把它变成一个任务"是另一件事，不该让前者消失。
        self.record_user_input(&statement, channel, &permission_scope, at)?;

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

    /// §6 第 1 步：把用户输入写成事件。
    ///
    /// 内容进内容仓、信封只带引用，**哪怕只有一句话也一样**。理由不是大小：§12.3 给对话与
    /// 转写定了保留期，而保留期要能把内容撤下来。内容内联在事件行上的话，"删掉这段对话"
    /// 就得改写一条 append-only 的记录——那不是删除，那是篡改历史。
    fn record_user_input(
        &mut self,
        text: &str,
        channel: UserChannel,
        permission_scope: &PermissionScope,
        at: WallClock,
    ) -> Result<EventId, CoreError> {
        let media_type = MediaType::new("text/plain")?;
        let written = self.content.put(text.as_bytes(), DataClass::Personal)?;
        self.store.record_blob(
            &written.blob_ref,
            &written.sha256,
            media_type.as_str(),
            written.bytes,
            // §12.3："对话与转写 | 本地可配置保留，初值 7 天。" 保留期记在内容对象上，
            // 由保留期驱动去执行——它是这一行从"没有生产者"变成"有对象可执行"的那一步。
            self.conversation_retention,
            at,
        )?;

        // 每条用户通道一条事件流：序号按（来源，epoch，boot）分配，不同通道各自连续。
        let source = SourceId::new(format!("channel:{}", channel.as_str()))?;
        let sequence = self
            .store
            .next_stream_sequence(&source, USER_INPUT_EPOCH, &self.boot)?;
        let event_id = EventId::generate();

        let envelope = Envelope::new(
            event_id,
            source,
            USER_INPUT_EPOCH,
            self.boot,
            sequence,
            self.task.clone(),
            Vec::new(),
            at,
            Monotonic::new(self.boot, sequence.saturating_mul(1_000_000)),
            // §6.1／§11.1：**整个系统里唯一一个 `is_instruction_authority` 为真的出处。**
            // 屏幕文字、麦克风转写、文档内容、工具输出都不是。写错方向比别处都严重，
            // 所以这一句旁边放的是引用而不是解释。
            Provenance::User { channel },
            PayloadRef::Blob {
                blob_ref: written.blob_ref,
                media_type,
                bytes: written.bytes,
                sha256: written.sha256,
            },
            permission_scope.clone(),
            DataClass::Personal,
            None,
            IdempotencyKey::new(format!("idem:{event_id}"))?,
        );
        self.store.append_event(&envelope, at)?;
        Ok(event_id)
    }

    /// 读回事件账里的用户输入，按提交序升序，最多 `limit` 条。
    ///
    /// 过滤用的是 [`Provenance::is_instruction_authority`]，而不是"看来源字符串像不像 chat"。
    /// 判断"这句话是不是用户说的"整个系统只有那一处依据（§6.1），把那份判断复制到这里，
    /// 两处迟早在某个新通道上分叉——而分叉的方向是某一类输入被当成了指令。
    pub fn user_inputs(&self, limit: usize) -> Result<Vec<UserInput>, CoreError> {
        let mut found = Vec::new();
        for event in self.store.read_events_after(0, limit)? {
            if !event.envelope.provenance.is_instruction_authority() {
                continue;
            }
            let Provenance::User { channel } = &event.envelope.provenance else {
                continue;
            };
            let PayloadRef::Blob { blob_ref, .. } = &event.envelope.payload_ref else {
                // 内联载荷不是本路径写出来的。跳过而不是猜内容：§11.1 的同一条原则，
                // 拿不到就说不知道。
                continue;
            };

            // 元数据还在册 → 内容本该在；读不到就是**真的坏了**，按 §9.3 报可诊断缺失。
            // 元数据已经不在了 → 它按保留期被清理过，那是约定内的消失，不是故障。
            // 把两者都说成"过期了"，会让一段被悄悄破坏的内容看起来完全正常。
            let text = if self.store.blob(blob_ref)?.is_some() {
                let bytes = self.store.read_content(&self.content, blob_ref)?;
                Some(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                None
            };

            found.push(UserInput {
                event_id: event.envelope.event_id.to_string(),
                channel: channel.as_str(),
                at: event.envelope.observed_at_utc,
                text,
            });
        }
        Ok(found)
    }

    /// 内容仓（只读）。
    pub fn content(&self) -> &ContentStore {
        &self.content
    }

    /// 改对话内容的保留期（§12.3："本地**可配置**保留，初值 7 天"）。
    ///
    /// 只影响之后写入的内容。想让存量内容跟随新设置，那是一次显式的操作——不该是改一个
    /// 数字的副作用。
    pub fn with_conversation_retention_days(mut self, days: i64) -> Self {
        self.conversation_retention = ContentRetention::from_days(Some(days));
        self
    }

    /// 当前对话内容的保留期。
    pub fn conversation_retention(&self) -> ContentRetention {
        self.conversation_retention
    }

    /// 换一个内容仓。
    ///
    /// 真实部署应当把它指到与事务库一起备份的位置。默认由事务库路径派生，内存存储则退到
    /// 一个临时仓——**那个临时仓重启就没了**，所以它只适合测试与模拟。
    pub fn with_content_store(mut self, content: ContentStore) -> Self {
        self.content = content;
        self
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
        // §6 第 9 步：结束之后，这个目标名下尚未推进的动作不该继续等着——一次已经不被想要的
        // 写入不该在下一个任务里悄悄发生。`abandon` 可能连带放弃了子目标，所以按"所有已结束的
        // 目标"扫一遍，而不是只清传入的那一个。
        self.drop_actions_for_finished_goals();
        Ok(abandoned)
    }

    /// 清掉所有已结束目标名下的待推进动作。
    ///
    /// 这是**急切**的一侧。[`Subject::select_filtered`] 里的那道过滤是**兜底**的一侧：
    /// 目标可能因为别的原因进入终态（主目标达成、用户撤回），而那些路径不一定经过本方法。
    /// 两处都做，是因为漏掉一次急切清理的后果是"一个不该发生的写悄悄发生了"。
    fn drop_actions_for_finished_goals(&mut self) {
        let finished: Vec<GoalId> = self
            .goals
            .iter()
            .filter(|goal| goal.state.is_terminal())
            .map(|goal| goal.goal_id.clone())
            .collect();
        for goal_id in finished {
            self.drop_actions_for(&goal_id);
        }
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
        // 这次观测在哪个权限范围下进行，以及那个范围**现在还有效吗**。
        let scope = self.observation_scope()?;
        let (record, envelope) = {
            let unit = self.cluster.unit_id().clone();
            let task = self.task.clone();
            let mut session =
                Session::new(&mut self.store, &mut self.broker, unit, task, self.boot)
                    .with_policy(scope, data_class);
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
        let context = ContextCompiler::new(self.backend, self.remote_authorized)
            .with_egress_policy(self.egress)
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

    /// §4.1 L3、§6 第 4–5 步：在当前候选上做检验，然后选一条推进。
    ///
    /// 两段合成一次调用是有意的：检验的**产出**就是选择的**输入**，而把两段拆成两个公开方法
    /// 会允许调用方跳过检验直接选——那样证据门槛就只剩一个数字，没有任何东西在它前面核对
    /// 结论。要单看检验结果，读返回的 [`Selection::reviews`] 即可。
    ///
    /// 返回候选集合一起交出去，是因为选择结果是**下标**：调用方要能自己看到被选中的是哪一条，
    /// 而不是只能相信一个数字。
    pub fn select(
        &self,
        policy: &SelectionPolicy,
        risk: ActionLevel,
        at: WallClock,
    ) -> Result<(CandidateSet, Selection), CoreError> {
        let candidates = self.cluster.propose(at)?;
        let review_policy = ReviewPolicy::for_risk(risk, policy.high_risk_from, policy.max_checks);
        let reviews = review_all(&candidates, self.cluster.ledger(), &review_policy);
        let selection = select_candidate(&candidates, reviews, policy, risk)?;
        Ok((candidates, selection))
    }

    /// 请求在授权目录里写一份内容（§12.1 的 A2）。
    ///
    /// 本方法只**投递**一次动作：构造意图、算出预期后果、交给能力簇的"待推进动作"槽位。
    /// 它不签发许可、不执行、也不绕过任何一道关——那三件事分别在 L3、[`PolicyAgent`] 与
    /// 执行代理手里。把"想要做"与"可以做"分成两处，就是 §7.2 那张表最直接的样子。
    ///
    /// 动作标识由**内容**派生，因此同一次写入重复投递是幂等的。动作标识是去重与恢复核对的
    /// 锚点（§7.3），用随机标识会让"这次动作做过没有"变成一个必须查账才能回答的问题——
    /// 而那正是账要解决的问题，不是账要制造的问题。
    pub fn request_write(
        &mut self,
        subject_ref: &str,
        content: &str,
        _at: WallClock,
    ) -> Result<ActionId, CoreError> {
        // 动作必须归属于一个目标。这既是它的存在理由（没有目标就不会有人要写这份东西），
        // 也是它被核对权限范围时的依据——用的是**它自己那个目标**的范围，而不是碰巧轮到的
        // 那个目标的范围。
        let Some(goal_id) = self.next_open_goal() else {
            return Err(CoreError::UnresolvedSubject(
                "没有可推进的目标，这次动作无处归属".to_string(),
            ));
        };

        let path = subject_ref
            .strip_prefix("file:")
            .ok_or_else(|| CoreError::UnresolvedSubject(subject_ref.to_string()))?;

        let action_id = ActionId::new(format!(
            "action:{}",
            Sha256Hex::of_bytes(format!("{subject_ref}\u{1f}{content}").as_bytes())
        ))?;

        let intent = ActionIntent::new(
            action_id.clone(),
            ToolId::new(WRITE_TOOL)?,
            ResourceScope::new(subject_ref)?,
            serde_json::json!({ "path": path, "content": content }),
            Vec::new(),
            PredictionRef::new(format!("prediction:{action_id}"))?,
            // §12.1："在指定目录生成/重命名文件"就是 A2。写死在这里而不是做成参数：
            // 等级是动作**性质**的函数，让调用方自己声明，等于让提议方给自己的动作定风险。
            ActionLevel::A2,
            ResourceCost {
                est_ram_bytes: content.len() as u64,
                est_tokens: 0,
                est_millis: 50,
            },
            self.cluster.unit_id().clone(),
        )?;

        self.cluster.queue_action(
            goal_id.clone(),
            intent,
            Expectation::VersionEquals {
                subject_ref: subject_ref.to_string(),
                expected: Sha256Hex::of_bytes(content.as_bytes()).to_string(),
            },
        )?;
        self.action_goals.insert(action_id.to_string(), goal_id);
        Ok(action_id)
    }

    /// 某个目标名下还有几个待推进的动作。
    pub fn pending_actions_for(&self, goal_id: &GoalId) -> usize {
        self.cluster.pending_actions_for(goal_id)
    }

    /// 丢掉某个目标名下尚未推进的动作。返回丢掉几个。
    ///
    /// §6 第 9 步的"计划外动作不继续后台执行"。放弃一个目标时调用它——一次已经不被想要的
    /// 写入不该在下一个任务里悄悄发生。
    pub fn drop_actions_for(&mut self, goal_id: &GoalId) -> usize {
        self.action_goals
            .retain(|_, bound| bound != goal_id);
        self.cluster.release_goal(goal_id)
    }

    /// 还有几个动作等待推进。
    pub fn pending_actions(&self) -> usize {
        self.cluster.pending_actions()
    }

    /// 跑一轮 §6 的闭环（第 3–8 步），并按第 9 步给出走向。
    ///
    /// 这一轮**不**执行副作用。本版还没有执行许可的签发与审批通路（§12.2），所以
    /// `RequestTool` 与 `RequestAction` 一律记为 [`AdvanceStep::Unsupported`] 并如实说出
    /// 原因。把它做成"静默跳过"的话，界面上会显示"跑了二十轮什么也没发生"，而真正的原因
    /// ——缺一条通路——看不出来。
    ///
    /// 一轮消耗一次激活（§4.2）。额度耗尽的判定在 [`GoalStack`] 里，因此"跑到一半没额度了"
    /// 与"一开始就没有可推进的目标"是两条可区分的路径。
    pub fn run_round(
        &mut self,
        policy: &SelectionPolicy,
        risk: ActionLevel,
        at: WallClock,
    ) -> Result<LoopRound, CoreError> {
        self.rounds = self.rounds.saturating_add(1);
        let round = self.rounds;

        let Some(goal_id) = self.next_open_goal() else {
            return Ok(LoopRound {
                round,
                outcome: RoundOutcome::Finished {
                    reason: "没有还能推进的目标（全部结束，或额度已耗尽）",
                },
                selection: None,
                selected: None,
                activations: 0,
                goal_id: None,
            });
        };

        // 先扣额度再干活。反过来的话，一次失败的运行不会留下痕迹，而额度记账一旦漏记，
        // 它就失去了作为"该升级预算了"触发器的意义（§4.2）。
        self.goals.activate(&goal_id)?;

        let (candidates, selection) = self.select_filtered(policy, risk, at)?;
        let selected = selection.selected_index();

        let outcome = match selection.outcome {
            SelectionOutcome::Selected { index } => {
                let step = self.advance(&candidates.candidates[index], at)?;
                // 推进过的结论记下来，下一轮不再重复推它（§6 第 2 步的路由雏形）。
                if let Candidate::Claim {
                    statement,
                    evidence_refs,
                } = &candidates.candidates[index]
                {
                    self.remember_handled(derivation_seed(statement, evidence_refs));
                }
                RoundOutcome::Advanced { step }
            }
            SelectionOutcome::NeedsMoreInformation { ref missing } => RoundOutcome::NeedsInput {
                missing: missing.clone(),
            },
            SelectionOutcome::NothingToPursue => RoundOutcome::Idle,
        };

        Ok(LoopRound {
            round,
            outcome,
            selection: Some(selection),
            selected,
            activations: 1,
            goal_id: Some(goal_id),
        })
    }

    /// 把选中的一条候选推进一步。
    fn advance(&mut self, candidate: &Candidate, at: WallClock) -> Result<AdvanceStep, CoreError> {
        match candidate {
            Candidate::RequestObservation { subject_ref, .. } => {
                // 观测的数据类别取 personal：**保守的那一档**。低估类别会让本该留在本地的
                // 内容被允许出站，而 §8 的默认是"私人数据不得出站"；高估的代价只是让本地模型
                // 也守 strict。默认值的方向要朝安全那一侧。
                match self.observe(subject_ref, DataClass::Personal, at) {
                    Ok(record) => Ok(AdvanceStep::Observation {
                        subject_ref: subject_ref.clone(),
                        evidence_ref: record.observation.evidence_ref.to_string(),
                    }),
                    // 授权已撤回不是一个"运行出错了"，而是一个需要人处理的拒绝——和等级
                    // 超范围、全局暂停同一类。把它当成普通错误抛出，环路会中断在一句
                    // "内部错误"上，而真正的原因看不出来。
                    Err(CoreError::CapabilityRevoked { capability }) => {
                        self.store.audit(
                            at,
                            AuditCategory::CapabilityDenied,
                            capability.as_str(),
                            "denied",
                            "授权已撤回，拒绝新的观测",
                        )?;
                        Ok(AdvanceStep::Refused {
                            reason: format!(
                                "能力策略 {capability} 已撤回，新的观测不再发生（§12.1）"
                            ),
                        })
                    }
                    Err(other) => Err(other),
                }
            }
            Candidate::Claim {
                statement,
                evidence_refs,
            } => {
                // §13.2 把学习分成三种，语义候选是其中一种。结论落进 L5。
                //
                // 条目标识由**命题 + 证据集合**派生，因此同一条结论**从同一批证据**重复写出
                // 是幂等的：跑一百轮不会攒出一百条同样的记忆。
                //
                // 只从命题派生是不够的——那样"同一个断言、换了一批证据"会撞成同一个标识却
                // 内容不同，于是被 §7.2 的"标识不得复用"规则拒掉。而那其实是一次**独立的
                // 第二次推导**，它本该成为自己的一条记忆：两条各自都能追溯到自己的证据。
                // 这个缺陷是 running_the_same_round_twice_records_the_conclusion_once 抓到的。
                let memory_id = MemoryId::new(format!(
                    "memory:{}",
                    Sha256Hex::of_bytes(derivation_seed(statement, evidence_refs).as_bytes())
                ))?;

                // 幂等：已经有过就不重写。查的是 `memory_including_hidden` 而不是 `memory`
                // ——一条被用户删除的记忆不能因为"又推出来一次"而复活。§12.3 的删除是终局，
                // 而"重新推导"恰恰是最容易绕过它的那条路。
                if self
                    .store
                    .memory_including_hidden(&memory_id)?
                    .is_some()
                {
                    return Ok(AdvanceStep::Claim {
                        statement: statement.clone(),
                        memory_id: memory_id.to_string(),
                        recorded: false,
                    });
                }
                let entry = MemoryEntry::new(
                    memory_id.clone(),
                    MemoryKind::Fact,
                    self.owner.clone(),
                    Some(self.task.clone()),
                    statement.clone(),
                    evidence_refs.clone(),
                    self.claim_provenance(evidence_refs)?,
                    DataClass::Personal,
                    at,
                )?
                .with_unit(self.cluster.unit_id().clone());
                let recorded = self.store.record_memory(&entry)?;
                Ok(AdvanceStep::Claim {
                    statement: statement.clone(),
                    memory_id: memory_id.to_string(),
                    recorded,
                })
            }
            Candidate::RequestAction { intent } => self.request_action(intent, at),
            other => Ok(AdvanceStep::Unsupported {
                candidate_kind: other.kind().as_str().to_string(),
                reason: "这条通路尚未实现".to_string(),
            }),
        }
    }

    /// 走完 §6 第 3–8 步：预测 → 签发许可 → 受理 → 投递 → 回执 → 后置条件核对。
    ///
    /// 许可由 [`PolicyAgent`] 判定，**不是调用方手搓的**。这是本次改动要补的那一环：
    /// 在此之前，许可类型本身能构造、能校验、能被执行代理复核，但没有任何东西决定
    /// "该不该发"——而那正是"系统会不会拒绝不该发生的动作"这个问题的所在。
    fn request_action(
        &mut self,
        intent: &ActionIntent,
        at: WallClock,
    ) -> Result<AdvanceStep, CoreError> {
        // 用**动作自己那个目标**的范围与预算，而不是碰巧轮到的那个目标。
        //
        // 差别不是形式上的：拿当前目标的范围去核对别的目标名下的动作，在两者范围不同时
        // 会得出错误结论——宽的那个会放行本该被拒的动作，窄的那个会拒掉本可执行的动作。
        // `select_filtered` 保证了这里能查到，查不到就说明有别的路径绕过了归属登记。
        let Some(goal_id) = self
            .action_goals
            .get(intent.action_id.as_str())
            .cloned()
        else {
            return Ok(AdvanceStep::Unsupported {
                candidate_kind: "request_action".to_string(),
                reason: "这次动作没有归属的目标，无法核对权限范围".to_string(),
            });
        };
        let Some(goal) = self.goals.goal(&goal_id).cloned() else {
            return Ok(AdvanceStep::Unsupported {
                candidate_kind: "request_action".to_string(),
                reason: format!("目标 {goal_id} 不在栈上，无法核对权限范围"),
            });
        };

        // 预算账挂在目标上，因此引用由目标标识派生。用随机标识的话，"这次动作花了谁的钱"
        // 就答不上来了。
        let budget_ref = BudgetRef::new(format!("budget:{goal_id}"))?;
        let permit_id = PermitId::new(format!("permit:{}", intent.action_id))?;
        let approval = self.find_covering_approval(intent, at)?;

        match self.policy.decide(
            intent,
            &goal.permission_scope,
            approval.as_ref(),
            at,
            permit_id,
            self.owner.clone(),
            budget_ref,
        ) {
            PermitDecision::Refused { reason } => {
                // §6 第 8 步：否决要留在最小审计账里。
                self.store.audit(
                    at,
                    AuditCategory::PermitRefused,
                    intent.action_id.as_str(),
                    "refused",
                    &reason,
                )?;
                // 从待推进队列里取走。留在那里的话，L3 每一轮都会重新提议同一个必然被拒的
                // 动作，环路会空转到额度耗尽——而 §6 第 9 步要的是"计划外动作不继续后台执行"。
                // 范围若以后被放宽，那是一次**新的**委托，应当重新投递。
                self.cluster.release_action(&intent.action_id);
                Ok(AdvanceStep::Refused { reason })
            }
            PermitDecision::NeedsApproval {
                level, reason, ..
            } => {
                self.store.audit(
                    at,
                    AuditCategory::ApprovalRequired,
                    intent.action_id.as_str(),
                    "waiting_approval",
                    &reason,
                )?;
                // §12.1 的人工审批是一个**正常状态**，不是一个失败。目标停在
                // `WaitingApproval` 上等外部输入。报成失败的话，环路会把它当成本轮的挫折
                // 去"绕路"，而绕路正是审批要挡住的东西。
                if self
                    .goals
                    .goal(&goal_id)
                    .is_some_and(|goal| goal.state == GoalState::Active)
                {
                    self.goals
                        .transition(&goal_id, GoalState::WaitingApproval)?;
                }
                Ok(AdvanceStep::NeedsApproval {
                    level: level.as_str().to_string(),
                    reason,
                })
            }
            PermitDecision::Issued(permit) => {
                // 原子消费批准。失败就丢弃许可——它还没有被任何地方受理过，丢弃是安全的；
                // 而放它过去，就等于同一个批准被两次签发同时用掉（§12.1 要求"每动作"）。
                if let Some(approval) = &approval {
                    self.store.consume_approval(&approval.approval_id)?;
                }
                self.store.audit(
                    at,
                    AuditCategory::PermitIssued,
                    intent.action_id.as_str(),
                    "issued",
                    &format!("工具 {}、等级 {}", intent.tool_id, intent.risk.as_str()),
                )?;

                let mut session = Session::new(
                    &mut self.store,
                    &mut self.broker,
                    intent.proposed_by.clone(),
                    self.task.clone(),
                    self.boot,
                )
                .with_policy(goal.permission_scope.clone(), DataClass::Personal);
                let round = session.run_write_round(intent, &permit, at)?;

                // 动作已经走完 §6 第 3–8 步，从待推进队列里取走。留着它会让 L3 每一轮都
                // 重新提议同一个已经执行过的动作——`run_write_round` 走的是会话路径，
                // 不经过能力簇的 `handle_result`，所以这一步在这里显式做。
                self.cluster.release_action(&intent.action_id);

                let (receipt, verdict) = match &round.dispatch {
                    DispatchOutcome::Receipted(receipt) => (
                        format!("{:?}", receipt.status),
                        round.outcome.as_ref().map(|outcome| format!("{:?}", outcome.verdict)),
                    ),
                    DispatchOutcome::Interrupted { .. } => ("interrupted".to_string(), None),
                    DispatchOutcome::Refused { reason } => {
                        (format!("refused: {reason}"), None)
                    }
                };

                Ok(AdvanceStep::Action {
                    action_id: intent.action_id.to_string(),
                    tool_id: intent.tool_id.to_string(),
                    permit_id: permit.permit_id.to_string(),
                    receipt,
                    verdict,
                })
            }
        }
    }

    /// 找一份覆盖这次动作的批准。
    ///
    /// 只取第一份覆盖的。批准之间没有优先级，而"按某个体贴的顺序挑一份"会引入一个不在规格
    /// 里的判据——那种判据在被审计时无法解释"为什么用了这一份而不是那一份"。
    ///
    /// 找不到就返回 `None` 交给策略代理：它区分"一份都没有"与"有但不覆盖这次动作"，
    /// 而那个区分决定了给用户看的是"请批准"还是"请针对这一次动作再批一次"。
    fn find_covering_approval(
        &self,
        intent: &ActionIntent,
        now: WallClock,
    ) -> Result<Option<Approval>, CoreError> {
        for approval in self.store.approvals_for(&self.owner)? {
            if approval.covers(intent, now).is_ok() {
                return Ok(Some(approval));
            }
        }
        Ok(None)
    }

    /// 记下一次人工批准（§12.1）。
    ///
    /// 批准落盘而不是只活在内存里：动作账上会留下 `approval_id`，如果批准本身重启就没了，
    /// 那条引用就是一个悬空锚点，而追溯链恰好断在最需要它回答的那个问题上——"这次写入是谁批的"。
    pub fn grant_approval(&mut self, approval: &Approval, at: WallClock) -> Result<bool, CoreError> {
        let recorded = self.store.record_approval(approval)?;
        self.store.audit(
            at,
            AuditCategory::ApprovalGranted,
            approval.approval_id.as_str(),
            "granted",
            &format!(
                "等级上限 {}、通道 {:?}、可用 {} 次",
                approval.max_action_level.as_str(),
                approval.channel,
                approval.max_uses
            ),
        )?;
        Ok(recorded)
    }

    /// 本主体名下当前可用的批准（未过期且有余量）。
    pub fn usable_approvals(&self, at: WallClock) -> Result<Vec<Approval>, CoreError> {
        Ok(self
            .store
            .approvals_for(&self.owner)?
            .into_iter()
            .filter(|approval| {
                approval.remaining() > 0
                    && approval.expires_at.is_none_or(|expires_at| at < expires_at)
            })
            .collect())
    }

    /// 执行一次保留期清理（§12.3）。
    ///
    /// 按策略的两步走：先让超期内容**立即不可见**，再按 `policy.purge` 决定要不要当场清掉。
    /// 报告如实分开这两件事——用户看到的"删除完成"指的是前者，后者可能还在排队。
    pub fn enforce_retention(
        &mut self,
        policy: &RetentionPolicy,
        at: WallClock,
    ) -> Result<RetentionReport, CoreError> {
        let mut report = expire_retained(&mut self.store, at)?;
        if policy.purge {
            report.merge(purge_retained(&mut self.store, &self.content, policy, at)?);
        }
        Ok(report)
    }

    /// 用户显式删掉一条记忆（§12.3："用户可随时删除"）。
    ///
    /// 只做隐藏那一步。物理清理留给下一次 [`Subject::enforce_retention`]，因为
    /// §12.3 要的是"**异步**清理，并给用户完成状态"——把清理塞进这个调用，界面就会卡在
    /// 一次可能很慢的传播上，而用户只是想让它别再出现。
    pub fn forget(
        &mut self,
        memory_id: &MemoryId,
        at: WallClock,
    ) -> Result<RetentionReport, CoreError> {
        forget_memory(&mut self.store, memory_id, "user_requested", at)
    }

    /// 当前还有多少条记忆等着被清理。
    pub fn memories_awaiting_purge(&self) -> Result<usize, CoreError> {
        Ok(self.store.tombstoned_memory_count()?)
    }

    /// 这次观测应当在哪个权限范围下进行（§6 第 1 步、§12.1）。
    ///
    /// 有可推进的目标就取它的范围，没有就用默认范围。两种情况都要过一遍"这项能力还生效吗"——
    /// **撤回立即生效**这句话的落点就在这里：撤回之后，新的观测不再发生。
    fn observation_scope(&self) -> Result<PermissionScope, CoreError> {
        let scope = self
            .next_open_goal()
            .and_then(|goal_id| self.goals.goal(&goal_id).map(|goal| goal.permission_scope.clone()))
            .unwrap_or_else(|| self.default_scope());

        if !self.policy.is_granted(&scope.capability_policy_ref) {
            return Err(CoreError::CapabilityRevoked {
                capability: scope.capability_policy_ref.to_string(),
            });
        }
        Ok(scope)
    }

    /// 没有具体目标时的权限范围。
    fn default_scope(&self) -> PermissionScope {
        PermissionScope {
            capability_policy_ref: self.policy.default_capability().clone(),
            // 上限取 A1："四处看看"不该悄悄带上改文件的能力。§12.1 的 A1 正好是
            // "读取已选文件、授权窗口"。
            max_action_level: ActionLevel::A1,
        }
    }

    /// 授予一项能力策略，并给出它的范围（§12.1）。
    ///
    /// 范围是**必填**的。给一个默认值（比如"不限定"）会让最省事的那次调用恰好拿到最宽的
    /// 授权，而"省事"和"更宽"之间不该有这种关系。
    pub fn grant_capability(
        &mut self,
        capability: CapabilityPolicyRef,
        scope: GrantScope,
        at: WallClock,
    ) -> Result<bool, CoreError> {
        let describe = if scope.is_unbounded() {
            "不按路径限定".to_string()
        } else {
            scope
                .prefixes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、")
        };
        let was_new = self.policy.grant(capability.clone(), scope);
        self.store.audit(
            at,
            AuditCategory::CapabilityGranted,
            capability.as_str(),
            "granted",
            &format!("授予能力授权，范围：{describe}"),
        )?;
        Ok(was_new)
    }

    /// 撤回一项能力策略，并**让已经由它产生的记忆立即失效**（§12.1、§12.3）。
    ///
    /// 两件事一起做，因为它们回答的是同一个问题的两半："撤回之后还能不能继续"和
    /// "撤回之前看到的东西还算不算数"。只做前者的话，一份通过已撤回授权读到的内容会继续
    /// 被检索、被引用、被写进模型上下文——撤回就成了一句只对将来有效的空话。
    ///
    /// 走的是"**事件 → 证据引用 → 记忆**"这条链，而不是"记忆 → 出处 → 能力"。后者要求每条
    /// 记忆记着自己是在哪个授权下产生的，而那会是一份必须和事件账保持同步的副本——
    /// 一份会分叉的副本。
    ///
    /// 内容对象**不**在这里退休：它们不携带能力归属，而"猜一个可能的归属然后删掉"比不删更糟。
    pub fn revoke_capability(
        &mut self,
        capability: &CapabilityPolicyRef,
        at: WallClock,
    ) -> Result<RevocationReport, CoreError> {
        let was_granted = self.policy.revoke(capability);

        let events = self.store.events_under_capability(capability)?;
        let mut invalidated = 0usize;
        let mut affected: Vec<EvidenceRef> = Vec::new();
        for event_id in &events {
            let reference = EvidenceRef::for_observation(event_id)?;
            invalidated = invalidated.saturating_add(self.store.tombstone_by_evidence(
                &reference,
                "capability_revoked",
                at,
            )?);
            affected.push(reference);
        }

        // §7.2 的第二个后果：**簇手里的那份引用也要失效。**
        //
        // 只做记忆那一半是不够的。记忆是"已经下过的结论"，而簇手里还有"可以用来下结论的
        // 材料"——撤回之后它照样会拿那些材料下出新结论，只是那些结论存不进记忆而已。
        // 而"下出了结论但存不进去"比"下不出结论"难发现得多：界面上一切正常，
        // 环路照常一轮一轮地跑。
        let evidence_retracted = self
            .cluster
            .retract_evidence(&affected, "capability_revoked");

        self.store.audit(
            at,
            AuditCategory::CapabilityRevoked,
            capability.as_str(),
            "revoked",
            &format!(
                "撤回授权：覆盖 {} 个事件、失效 {} 条记忆、撤回 {evidence_retracted} 条在库证据{}",
                events.len(),
                invalidated,
                if was_granted {
                    ""
                } else {
                    "（此前已不在授权内）"
                }
            ),
        )?;

        Ok(RevocationReport {
            was_granted,
            events_covered: events.len(),
            memories_invalidated: invalidated,
            evidence_retracted,
            awaiting_purge: self.store.tombstoned_memory_count()?,
        })
    }

    /// 当前生效的全部授权（§12.1）。
    pub fn granted_capabilities(&self) -> Vec<&CapabilityPolicyRef> {
        self.policy.granted()
    }

    /// 策略代理（只读）。
    pub fn policy(&self) -> &PolicyAgent {
        &self.policy
    }

    /// 策略代理（可变）。用于全局暂停与恢复（§12.1 末段）。
    pub fn policy_mut(&mut self) -> &mut PolicyAgent {
        &mut self.policy
    }

    /// 执行一次状态迁移。目标卡在审批上时用它恢复（§12.1）。
    ///
    /// 只暴露"从等待审批回到进行中"这一条路：把目标从暂停里放出来是一个需要显式做出的
    /// 决定，而把它藏进通用的 `transition` 里，等于给了调用方一条静悄悄绕过审批的路。
    pub fn resume_after_approval(&mut self, goal_id: &GoalId, at: WallClock) -> Result<(), CoreError> {
        let goal = self
            .goals
            .goal(goal_id)
            .ok_or_else(|| CoreError::UnresolvedSubject(goal_id.to_string()))?;
        if goal.state != GoalState::WaitingApproval {
            return Err(CoreError::UnresolvedSubject(format!(
                "{goal_id} 不处于等待审批状态（当前 {}）",
                goal.state.as_str()
            )));
        }
        self.goals.transition(goal_id, GoalState::Active)?;
        self.store.audit(
            at,
            AuditCategory::ApprovalGranted,
            goal_id.as_str(),
            "resumed",
            "收到批准，目标恢复推进",
        )?;
        Ok(())
    }

    /// 一条结论的出处（§7.1）。
    ///
    /// 结论是能力簇从观测里推出来的，所以出处是 `Derived`，指向它引用的第一条可解析来源的
    /// 原始事件。**解析不出来就失败**，而不是编一个出处——§7.1 要求派生物必须指回原始事件，
    /// 一个指向空气的出处比没有出处更糟：它会让审计以为这条链条是完整的。
    ///
    /// `transform` 取 `Classification` 只是因为 [`DerivationKind`] 那一组里没有"推导"这一项，
    /// 它是几个选项中偏离最小的一个。这一点写在这里而不是装作贴切。
    fn claim_provenance(
        &self,
        evidence_refs: &[soca_contracts::EvidenceRef],
    ) -> Result<Provenance, CoreError> {
        for reference in evidence_refs {
            if let Some(source_event_id) = reference.origin_event_id() {
                return Ok(Provenance::Derived {
                    source_event_id,
                    model_version: ModelVersion::new("sha256:cluster-deterministic")?,
                    transform: DerivationKind::Classification,
                });
            }
        }
        Err(CoreError::Contract(ContractError::MissingRefs {
            field: "memory.provenance.source_event_id",
        }))
    }

    /// 与 [`Subject::select`] 相同，但**滤掉本次会话里已经推进过的结论**。
    ///
    /// 公开的 `select` 不做这层过滤：它是一个查看接口，要如实展示簇当前提出了什么，
    /// 包括那些已经记过的。而闭环用的是这一份——否则它会每一轮都选中同一条结论，
    /// 跑满额度也什么都没变。
    fn select_filtered(
        &self,
        policy: &SelectionPolicy,
        risk: ActionLevel,
        at: WallClock,
    ) -> Result<(CandidateSet, Selection), CoreError> {
        let mut candidates = self.cluster.propose(at)?;
        candidates.candidates.retain(|candidate| match candidate {
            Candidate::Claim {
                statement,
                evidence_refs,
            } => !self
                .handled_claims
                .contains(&derivation_seed(statement, evidence_refs)),
            // 动作只属于提出它的那个目标，而且那个目标必须还活着。
            //
            // 这是第二道闸：第一道在目标结束时的 `drop_actions_for`。两处都做，是因为
            // "清掉队列"发生在目标迁移的那一刻，"不提议"发生在之后的每一轮——只做第一道的话，
            // 任何一次漏掉的迁移都会变成一个悄无声息的执行。
            //
            // 认不出归属的动作同样被挡下。**失败关闭**：一个不知道属于哪个任务的动作，
            // 我们没有依据核对它的权限范围，而没有依据不等于没有风险。
            Candidate::RequestAction { intent } => self
                .action_goals
                .get(intent.action_id.as_str())
                .and_then(|goal_id| self.goals.goal(goal_id))
                .is_some_and(|goal| !goal.state.is_terminal()),
            _ => true,
        });

        let review_policy = ReviewPolicy::for_risk(risk, policy.high_risk_from, policy.max_checks);
        let reviews = review_all(&candidates, self.cluster.ledger(), &review_policy);
        let selection = select_candidate(&candidates, reviews, policy, risk)?;
        Ok((candidates, selection))
    }

    /// 记下一条已经推进过的结论，并保持有界。
    fn remember_handled(&mut self, seed: String) {
        if self.handled_claims.contains(&seed) {
            return;
        }
        self.handled_claims.push(seed);
        // 有界：与证据池同一个理由（§17 的长时运行验收要求内存增长符合配额）。淘汰最早的
        // 一条，意味着一条很久以前记过的结论有可能被重新推导一次——那只是重写一遍同样的
        // 记忆，是幂等的，代价可以接受。
        if self.handled_claims.len() > MAX_HANDLED_CLAIMS {
            self.handled_claims.remove(0);
        }
    }

    /// 下一个还能推进的目标。
    ///
    /// 只挑 `Active` 且额度未耗尽的。§6 第 9 步的"结束"主要是从这里发生的：目标全部结束、
    /// 或全部用完额度，闭环就该停，而不是继续空转。
    fn next_open_goal(&self) -> Option<GoalId> {
        self.goals
            .iter()
            .find(|goal| goal.state == GoalState::Active && !goal.budget.is_exhausted())
            .map(|goal| goal.goal_id.clone())
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
            ledger_records: self.cluster.ledger().len(),
            memory_entries: self.store.memory_count(&self.owner)?,
            memories_awaiting_purge: self.store.tombstoned_memory_count()?,
            actions: self.store.action_count()?,
            pending_actions: self.cluster.pending_actions(),
            usable_approvals: self.usable_approvals(at)?.len(),
            model_calls: self.gateway.calls(),
            backend: self.backend.as_str(),
            egress_policy: self.egress.as_str(),
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

    /// 当前模型后端。
    pub fn backend(&self) -> ModelBackend {
        self.backend
    }

    /// 当前个人数据出站策略。默认 `Strict`。
    pub fn egress_policy(&self) -> EgressPolicy {
        self.egress
    }

    /// 换一个模型传输层。
    ///
    /// **必须一次性把新端点相关的三件事全部给出**：后端、云端授权、出站策略。分开设置会让
    /// "换了端点但沿用旧授权"变成一个可能的中间态，而 §8 的要求恰恰是每一次云端授权都要
    /// 对应到具体的端点。这里没有提供只改其中一项的入口。
    ///
    /// 出站策略默认回到 `Strict`：换端点等于换了一个信任边界，旧的批准不继承。
    pub fn set_transport(
        &mut self,
        transport: Box<dyn Transport>,
        backend: ModelBackend,
        remote_authorized: bool,
        model_version: ModelVersion,
    ) -> Result<(), CoreError> {
        let budget = self.gateway.budget();
        self.gateway =
            ModelGateway::new(transport, backend, remote_authorized, budget, model_version)?;
        self.backend = backend;
        self.remote_authorized = remote_authorized;
        self.egress = EgressPolicy::Strict;
        Ok(())
    }

    /// 显式放开个人数据出站。
    ///
    /// 单独一个方法，是因为它是一次**策略变更**而不是一个配置项：调用点应当能被审计代码
    /// 一眼找到。任何时候都只放开 [`DataClass::Personal`] 这一档。
    pub fn grant_personal_egress(&mut self) {
        self.egress = EgressPolicy::AllowPersonal;
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
