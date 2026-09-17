//! 契约层拒绝原因。
//!
//! 这里的每一个变体都对应架构文档里一条"要么合法、要么拒绝"的硬规则。拒绝原因本身
//! 也会进入审计账（§6.8：错误的引用、失败、超时、否决均保留在最小审计账中），因此
//! 描述必须是确定性的、可比较的、不包含敏感内容的。

use thiserror::Error;

/// 契约校验失败。
///
/// 所有变体都不携带密钥、路径明文或用户内容，只携带标识、字段名和计数，以便安全地
/// 写入审计记录。
///
/// 每个变体的说明由 `#[error]` 的展示文本承担，因此关闭逐变体的文档要求。
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractError {
    // ---- 版本与字段基本形态 ----
    #[error("schema_version 不匹配：期望 {expected}，实际 {actual}")]
    SchemaVersionMismatch { expected: u16, actual: u16 },

    #[error("字段不能为空：{field}")]
    EmptyField { field: &'static str },

    #[error("字段 {field} 超长：上限 {limit} 字节，实际 {actual} 字节")]
    FieldTooLong {
        field: &'static str,
        limit: usize,
        actual: usize,
    },

    #[error("字段 {field} 含首尾空白，必须由调用方先规整")]
    SurroundingWhitespace { field: &'static str },

    #[error("{kind} 需要前缀 {expected_prefixes:?}，实际为 {actual:?}")]
    MalformedId {
        kind: &'static str,
        expected_prefixes: &'static [&'static str],
        actual: String,
    },

    #[error("{kind} 不是合法 UUID：{actual:?}")]
    MalformedUuid {
        kind: &'static str,
        actual: String,
    },

    #[error("{kind} 不是 64 位小写十六进制摘要：{actual:?}")]
    MalformedDigest {
        kind: &'static str,
        actual: String,
    },

    #[error("{kind} 不是合法 RFC 3339 时间：{actual:?}")]
    MalformedTimestamp {
        kind: &'static str,
        actual: String,
    },

    // ---- 引用完整性（§7.2：引用必须能解析为存在且仍可访问的证据）----
    #[error("{field} 中出现 {count} 处重复引用，例如 {sample}")]
    DuplicateRefs {
        field: &'static str,
        count: usize,
        sample: String,
    },

    #[error("causal_parent_ids 不能引用事件自身：{0}")]
    SelfCausalParent(String),

    #[error("{field} 必须至少包含一个引用")]
    MissingRefs { field: &'static str },

    #[error("一轮提出的候选数 {actual} 超过上限 {limit}（§4.1 L2：黑板有界）")]
    CandidateLimitExceeded { limit: usize, actual: usize },

    #[error("冲突 {subject_ref:?} 只有 {positions} 个立场；一个立场不构成冲突（§4.2）")]
    ConflictNeedsTwoSides { subject_ref: String, positions: usize },

    #[error("冲突 {subject_ref:?} 的立场全部出自同一个单元；冲突指的是子单元之间的分歧（§4.2）")]
    ConflictNeedsDistinctUnits { subject_ref: String },

    #[error("冲突 {subject_ref:?} 的立场数 {actual} 超过上限 {limit}")]
    ConflictTooManyPositions {
        subject_ref: String,
        limit: usize,
        actual: usize,
    },

    // ---- L2 工作空间（§4.1、§6 第 4 步）----
    #[error("黑板主题数 {actual} 超过上限 {limit}（§4.1 L2：各组合边界有小空间，不无限复制）")]
    WorkspaceTopicLimitExceeded { limit: usize, actual: usize },

    #[error("黑板占用 {actual} 字节超过上限 {limit} 字节（§4.1 L2：黑板有界）")]
    WorkspaceByteLimitExceeded { limit: usize, actual: usize },

    #[error("证据 {evidence_ref} 不在黑板上；§6 第 4 步要求 L2 核对证据存在性")]
    EvidenceNotOnWorkspace { evidence_ref: String },

    // ---- §8 上下文编译与模型返回 ----
    #[error("证据 {evidence_ref} 不在本次上下文里；模型不能引用它没看到的东西")]
    EvidenceNotInContext { evidence_ref: String },

    #[error(
        "预测 {prediction_ref} 不在本次上下文的已记录预测里；§6.3 的预测由单元在动作前写下，\
         模型不能现编一个引用"
    )]
    PredictionNotRecorded { prediction_ref: String },

    #[error(
        "远端后端未获授权；§8 要求云端请求走单独策略批准，内存不足或本地模型不可用都不是\
         把私人上下文发到云端的理由"
    )]
    RemoteNotAuthorized,

    #[error("{field} 超出上限：上限 {limit}，实际 {actual}")]
    ContextLimitExceeded {
        field: &'static str,
        limit: usize,
        actual: usize,
    },

    #[error("模型返回的提案数 {actual} 超过上限 {limit}")]
    ProposalLimitExceeded { limit: usize, actual: usize },

    #[error("本次上下文不允许提出 {kind} 类候选（§8 的输出 Schema）")]
    CandidateKindNotAllowed { kind: &'static str },

    // ---- L6 目标栈（§4.1 L6、§2、§4.2）----
    #[error(
        "目标的出处是 {provenance}，不是用户明确通道；§2 明确不承诺自主产生目标，\
         屏幕文字、转写、文档与模型输出都不是指令来源"
    )]
    GoalNotDelegated { provenance: &'static str },

    #[error("目标深度 {actual} 超过上限 {limit}（§4.2：禁止递归无限生成子任务）")]
    GoalDepthExceeded { limit: usize, actual: usize },

    #[error("子目标的权限等级 {child_level} 宽于父目标的 {parent_level}（§12.2：授权不给子单元自动扩大）")]
    GoalPermissionWidened {
        parent_level: &'static str,
        child_level: &'static str,
    },

    #[error("目标额度超限：{field} 上限 {limit}，实际 {actual}")]
    GoalBudgetExceeded {
        field: &'static str,
        limit: usize,
        actual: usize,
    },

    #[error("目标数 {actual} 超过上限 {limit}")]
    GoalLimitExceeded { limit: usize, actual: usize },

    #[error("目标 {goal_id} 处于 {state} 状态，不能推进")]
    GoalNotActive {
        goal_id: String,
        state: &'static str,
    },

    #[error("探索配额已用尽：上限 {limit}，实际 {actual}（§4.1 L6）")]
    ExplorationQuotaExhausted { limit: usize, actual: usize },

    #[error("失败关闭：{0}")]
    FailClosed(&'static str),

    #[error("预测对象 {subject:?} 与期望作用对象 {expectation_subject:?} 不一致")]
    ExpectationSubjectMismatch {
        subject: String,
        expectation_subject: String,
    },

    #[error("游戏协议语义校验失败：{0}")]
    GameProtocol(String),

    #[error("拓扑世代不合法：{0}")]
    InvalidEpoch(&'static str),

    #[error("同一证据同时出现在支持与反对两侧，矛盾未解决：{sample}")]
    ContradictoryEvidence { sample: String },

    // ---- 载荷与参数形态 ----
    #[error("小消息信封 {actual} 字节超过上限 {limit} 字节，大载荷必须只传引用（§10.4）")]
    PayloadTooLarge { limit: usize, actual: usize },

    #[error("字段 {field} 无法编码，按失败关闭处理")]
    EncodingFailed { field: &'static str },

    #[error("字段 {field} 不是本版本可解析的载荷，按失败关闭处理")]
    MalformedPayload { field: &'static str },

    #[error("动作参数必须是结构化 JSON 对象，实际为 {actual}")]
    ParametersNotStructured { actual: &'static str },

    #[error("工具 {tool_id} 属于首版禁止经由认知循环调用的类别（§12.2）")]
    ForbiddenToolInV1 { tool_id: String },

    // ---- 时间（§7.1）----
    #[error("单调时钟不能跨 boot 比较：{left_boot} vs {right_boot}")]
    MonotonicAcrossBoots { left_boot: String, right_boot: String },

    #[error("信封的 received_monotonic.boot_id 与 envelope.boot_id 不一致")]
    BootIdMismatch,

    #[error("时间窗非法：end({end}) 不晚于 start({start})")]
    InvalidTimeWindow { start: String, end: String },

    #[error("信封已过期：expires_at={expires_at}，now={now}")]
    EnvelopeExpired { expires_at: String, now: String },

    // ---- 概率与校准（§3.2）----
    #[error("未经校准的自评分数不能被表述为概率真值")]
    UncalibratedProbability,

    #[error("声称已校准但样本数为 0")]
    EmptyCalibrationSamples,

    #[error("概率越界（必须在 [0,1] 内）：{actual}")]
    ProbabilityOutOfRange { actual: String },

    // ---- 权限（§12.1、§12.2）----
    #[error("权限等级 {level} 不允许经由普通认知循环执行")]
    ForbiddenInCognitiveLoop { level: &'static str },

    #[error("权限等级 {level} 缺少必需的审批 ID")]
    MissingApproval { level: &'static str },

    #[error("执行许可字段非法：{reason}")]
    PermitInvalid { reason: &'static str },

    #[error("执行许可已过期：expires_at={expires_at}")]
    PermitExpired { expires_at: String },

    #[error("执行许可已用尽：max_uses={max_uses}")]
    PermitExhausted { max_uses: u8 },

    #[error("执行许可与动作不匹配：{field}")]
    PermitMismatch { field: &'static str },

    #[error("数据类别 {class} 不允许出站到云端")]
    EgressDenied { class: &'static str },

    // ---- 单元生命周期（§9.2）----
    #[error("状态迁移非法：{from} -> {to}")]
    LifecycleViolation {
        from: &'static str,
        to: &'static str,
    },

    #[error("迁移到 {to} 前必须先把 {count} 个未决动作移交给在线动作账，例如 {sample}")]
    UnresolvedPendingActions {
        to: &'static str,
        count: usize,
        sample: String,
    },
}
