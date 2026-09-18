//! 数据类别、动作等级与权限范围（§8、§12.1、§12.2）。
//!
//! 这些不是"建议"，是确定性规则：架构文档 §12.2 明确要求"确定性规则管边界，LLM 可以辅助
//! 识别风险但不能放宽规则"。所以等级与放行要求写成穷举的 `match`，新增等级时编译器会强迫
//! 作者做出决定。

use serde::{Deserialize, Serialize};

use crate::action::ActionIntent;
use crate::{
    ApprovalId, CapabilityPolicyRef, ContractError, ResourceScope, Sha256Hex, SubjectId, ToolId,
    UserChannel, WallClock,
};

/// 数据类别，决定能否出站（§8）。
///
/// 变体顺序即敏感度顺序（`Public` < `Personal` < `Sensitive` < `Secret`），因此派生
/// `Ord` 之后可以直接比较"哪一类更敏感"。这不是装饰：一份上下文里混了多个类别时，
/// 出站判断必须看**最敏感的那一条**，而不是第一条或平均。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    /// 公开内容。
    Public,
    /// 个人内容（对话、转写、授权文件派生索引）。
    Personal,
    /// 敏感内容（原始音视频、凭据周边、可识别的隐私字段）。
    Sensitive,
    /// 机密内容（密钥材料、审批令牌）。
    Secret,
}

/// 出站判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressVerdict {
    /// 需要单独策略批准并过滤敏感内容（§8）。
    RequiresPolicyApproval,
    /// 直接拒绝。内存不足、本地模型不可用都不是把它发到云端的理由（§8）。
    Denied,
}

impl DataClass {
    /// 该类数据能否出站到云端。
    pub fn cloud_egress(self) -> EgressVerdict {
        match self {
            Self::Public => EgressVerdict::RequiresPolicyApproval,
            Self::Personal | Self::Sensitive | Self::Secret => EgressVerdict::Denied,
        }
    }

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Personal => "personal",
            Self::Sensitive => "sensitive",
            Self::Secret => "secret",
        }
    }
}

/// 出站策略（§8）。
///
/// §8 的默认是"私人数据类别不得出站到云端"，而它的理由写得很具体：
///
/// > 若GPU预算不足，当前选定后端不可准入：只可使用用户已批准的降级模型配置重新规划，或保持
/// > 控制界面并暂停推理。**不得自动将私人上下文发云。**
///
/// 关键在于"自动"二字。用户自己配置了一个端点、并且明确批准之后，把**自己**的数据发过去
/// 是他对自己数据的处置——那不是系统在绕过限制，而是数据所有者在行使决定权。所以本类型把
/// 那条路做成必须显式选择、必须按端点声明、且默认关闭的东西。
///
/// 三条不可逾越的边界写在这里，以免"用户批准"被理解成万能钥匙：
///
/// * 只对 [`DataClass::Personal`] 生效。[`DataClass::Sensitive`] 与 [`DataClass::Secret`]
///   无论在哪种策略下都不出站——原始音视频与密钥材料不该因为一个勾选框就上路。
/// * 只对远端后端生效。本地后端本来就不需要出站判断。
/// * 这是**按端点**的声明。换端点等于换了一个信任边界，旧批准不继承。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressPolicy {
    /// §8 的默认：私人数据一律不出站。
    #[default]
    Strict,
    /// 用户已就当前端点明确批准个人数据出站。**不是默认值。**
    AllowPersonal,
}

impl EgressPolicy {
    /// 该策略下某一类数据能否出站到远端。
    pub fn permits(self, class: DataClass) -> bool {
        match class.cloud_egress() {
            // §8 允许出站、但需要一次单独的策略批准。本策略类型表示的就是那次批准是否已给出。
            EgressVerdict::RequiresPolicyApproval => true,
            EgressVerdict::Denied => {
                // "用户批准"只打开中间那一档，不打开全部。
                self == Self::AllowPersonal && class == DataClass::Personal
            }
        }
    }

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::AllowPersonal => "allow_personal",
        }
    }
}

/// 动作等级（§12.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ActionLevel {
    /// A0：本地计算、查询本系统状态、输出聊天草稿。
    A0,
    /// A1：读取已选文件、授权窗口、开启一次已批准采集会话。
    A1,
    /// A2：在指定目录生成/重命名文件、修改应用数据。
    A2,
    /// A3：对外发送、安装软件、修改全局设置、大量删除、财务或账户动作。
    ///
    /// 首版禁止其中的高风险类别（§12.1）。
    A3,
    /// A4：提权、修改自身政策/凭据、规避审计或扩大传感权限。普通认知循环无此能力。
    A4,
}

/// 放行要求。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRequirement {
    /// A0：已有任务范围与预算即可。
    TaskScopeAndBudget,
    /// A1：范围限定授权，撤回立即生效。
    StandingScopeGrant,
    /// A2：预览、目标版本核对、备份/撤销，或每任务明确批准。
    PreviewOrPerTaskApproval,
    /// A3：每动作人工审批。
    PerActionHumanApproval,
    /// A4：普通认知循环无此能力，只能走用户独立管理流程。
    ForbiddenInCognitiveLoop,
}

impl ActionLevel {
    /// 全部等级，按从低到高排列。
    pub const ALL: [Self; 5] = [Self::A0, Self::A1, Self::A2, Self::A3, Self::A4];

    /// 放行要求。
    pub fn approval_requirement(self) -> ApprovalRequirement {
        match self {
            Self::A0 => ApprovalRequirement::TaskScopeAndBudget,
            Self::A1 => ApprovalRequirement::StandingScopeGrant,
            Self::A2 => ApprovalRequirement::PreviewOrPerTaskApproval,
            Self::A3 => ApprovalRequirement::PerActionHumanApproval,
            Self::A4 => ApprovalRequirement::ForbiddenInCognitiveLoop,
        }
    }

    /// 该等级是否允许由普通认知循环发起。
    pub fn is_allowed_in_cognitive_loop(self) -> bool {
        !matches!(self, Self::A4)
    }

    /// 该等级是否要求一次可追溯到具体审批 ID 的人工批准。
    ///
    /// A0 与 A1 的授权由范围令牌承载（`capability_policy_ref`），不需要逐动作审批 ID；
    /// A2 与 A3 必须有。
    pub fn requires_approval_id(self) -> bool {
        matches!(self, Self::A2 | Self::A3)
    }

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A0 => "A0",
            Self::A1 => "A1",
            Self::A2 => "A2",
            Self::A3 => "A3",
            Self::A4 => "A4",
        }
    }
}

/// 信封携带的权限范围：这份数据允许被用于什么等级的动作。
///
/// 它不授予任何新能力，只是给"这份数据最多能推动多重的动作"设一个上限（§12.2：授权不给
/// 子单元自动扩大）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionScope {
    /// 该范围绑定的能力策略版本。
    pub capability_policy_ref: CapabilityPolicyRef,
    /// 允许的最高动作等级。
    pub max_action_level: ActionLevel,
}

impl PermissionScope {
    /// 判断某个等级是否落在本范围内。
    pub fn permits(&self, level: ActionLevel) -> Result<(), ContractError> {
        if level > self.max_action_level {
            return Err(ContractError::ForbiddenInCognitiveLoop {
                level: level.as_str(),
            });
        }
        Ok(())
    }
}

/// 一次明确的人工批准（§12.1、§12.2）。
///
/// §12.1 对 A2 要求"预览、目标版本核对、备份/撤销**或**每任务明确批准"，对 A3 要求
/// "**每动作**人工审批"。两句话落在同一个问题上：**这次批准覆盖的是哪一次具体动作。**
///
/// 所以本类型不是一个"用户点了同意"的布尔值，而是一组绑定：工具、资源范围、参数摘要、
/// 等级上限、有效期、可用次数、来源通道。§12.2 要求能力令牌绑定"用户/主体、具体工具、
/// 资源范围、参数摘要、允许次数、TTL、预算、策略版本及审批 ID"——令牌身上其中一半的绑定
/// 来自这里，而令牌自己无法凭空获得它们。
///
/// 一个布尔值做不到这件事。用户批准"把这份摘要写进这个目录"，与用户批准"随便写点什么到
/// 某个地方"是两回事；如果批准只是一个 `true`，判据就被挪到了别处，而判据放在哪里，
/// 哪里就是真正的边界。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// 审批标识。它会被写进许可，因此必须能追溯到这一次批准。
    pub approval_id: ApprovalId,
    /// 批准者。
    pub subject_id: SubjectId,
    /// 批准的动作等级上限。
    pub max_action_level: ActionLevel,
    /// 只批准这个工具。`None` 表示不限定工具。
    pub tool_id: Option<ToolId>,
    /// 只批准这个资源范围。`None` 表示不限定范围。
    pub object_scope: Option<ResourceScope>,
    /// 只批准这一份具体参数。`None` 表示不限定参数。
    pub parameters_digest: Option<Sha256Hex>,
    /// 批准时刻。
    pub granted_at: WallClock,
    /// 失效时刻。`None` 表示不自动过期。
    pub expires_at: Option<WallClock>,
    /// 批准的来源通道。
    ///
    /// §12.1 要的是"**人工**审批"，§14 进一步规定"不以可能误识别的语音自动批准高风险
    /// 操作"。所以通道必须记下来并参与判定（见 [`Approval::covers`]），而不是只写进日志。
    pub channel: UserChannel,
    /// 允许被几次动作消费。
    pub max_uses: u8,
    /// 已消费次数。
    pub used: u8,
}

impl Approval {
    /// 构造一次批准。
    ///
    /// 逐条拒绝：可用次数为 0（那是一次"批准了什么也不许做"）、失效时刻不晚于批准时刻。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        approval_id: ApprovalId,
        subject_id: SubjectId,
        max_action_level: ActionLevel,
        channel: UserChannel,
        granted_at: WallClock,
        expires_at: Option<WallClock>,
        max_uses: u8,
    ) -> Result<Self, ContractError> {
        if max_uses == 0 {
            return Err(ContractError::ApprovalInvalid {
                reason: "max_uses 必须至少为 1",
            });
        }
        if let Some(expires_at) = expires_at
            && expires_at <= granted_at
        {
            return Err(ContractError::ApprovalInvalid {
                reason: "expires_at 必须晚于 granted_at",
            });
        }
        Ok(Self {
            approval_id,
            subject_id,
            max_action_level,
            tool_id: None,
            object_scope: None,
            parameters_digest: None,
            granted_at,
            expires_at,
            channel,
            max_uses,
            used: 0,
        })
    }

    /// 收窄到具体工具。
    pub fn for_tool(mut self, tool_id: ToolId) -> Self {
        self.tool_id = Some(tool_id);
        self
    }

    /// 收窄到具体资源范围。
    pub fn for_scope(mut self, object_scope: ResourceScope) -> Self {
        self.object_scope = Some(object_scope);
        self
    }

    /// 收窄到具体参数。绑定参数摘要之后，改一个字节就失效（§12.2）。
    pub fn for_parameters(mut self, parameters_digest: Sha256Hex) -> Self {
        self.parameters_digest = Some(parameters_digest);
        self
    }

    /// 直接收窄到某一次具体动作的绑定。
    ///
    /// 这是 A3 的"每动作人工审批"应当用的构造方式：用户看到的是具体的一次动作，
    /// 批准的也应当是具体的那一次。
    pub fn for_intent(self, intent: &ActionIntent) -> Self {
        self.for_tool(intent.tool_id.clone())
            .for_scope(intent.object_scope.clone())
            .for_parameters(intent.parameters_digest())
    }

    /// 还剩几次可用。
    pub fn remaining(&self) -> u8 {
        self.max_uses.saturating_sub(self.used)
    }

    /// 这次批准是否覆盖这次动作。
    ///
    /// 逐条判定，**任何一条不符即不覆盖**：
    ///
    /// 1. 动作等级不高于批准的上限；
    /// 2. 未过期；
    /// 3. 还有剩余次数；
    /// 4. 工具、资源范围、参数摘要与批准时绑定的那一次一致（`None` 表示该项未收窄）；
    /// 5. A3 及以上的批准不来自语音通道（§14）。
    ///
    /// 第 4 条是全部设计的落点：`object_scope` 与 `parameters_digest` 一旦绑定，
    /// 换一个目录或改一个字节都不再被覆盖，于是"批准过一次"不能被扩成"以后都行"。
    pub fn covers(&self, intent: &ActionIntent, now: WallClock) -> Result<(), ContractError> {
        if intent.risk > self.max_action_level {
            return Err(ContractError::ApprovalDoesNotCover {
                approval_id: self.approval_id.to_string(),
                field: "max_action_level",
            });
        }
        // §14：语音可能被误识别，而 A3 是"每动作人工审批"这一档，不能靠它放行。
        // A2 不在此列，是因为 A2 的放行要求里除了审批还有"预览、目标版本核对、备份/撤销"
        // 这几条确定性手段，而 A3 没有别的兜底。
        if intent.risk >= ActionLevel::A3 && self.channel == UserChannel::PushToTalk {
            return Err(ContractError::ApprovalDoesNotCover {
                approval_id: self.approval_id.to_string(),
                field: "channel（A3 不接受语音批准，§14）",
            });
        }
        if let Some(expires_at) = self.expires_at
            && now >= expires_at
        {
            return Err(ContractError::ApprovalExpired {
                approval_id: self.approval_id.to_string(),
                expires_at: expires_at.to_string(),
            });
        }
        if self.remaining() == 0 {
            return Err(ContractError::ApprovalExhausted {
                approval_id: self.approval_id.to_string(),
                max_uses: self.max_uses,
            });
        }
        if let Some(tool_id) = &self.tool_id
            && tool_id != &intent.tool_id
        {
            return Err(ContractError::ApprovalDoesNotCover {
                approval_id: self.approval_id.to_string(),
                field: "tool_id",
            });
        }
        if let Some(object_scope) = &self.object_scope
            && object_scope != &intent.object_scope
        {
            return Err(ContractError::ApprovalDoesNotCover {
                approval_id: self.approval_id.to_string(),
                field: "object_scope",
            });
        }
        if let Some(digest) = &self.parameters_digest
            && digest != &intent.parameters_digest()
        {
            return Err(ContractError::ApprovalDoesNotCover {
                approval_id: self.approval_id.to_string(),
                field: "parameters_digest",
            });
        }
        Ok(())
    }

    /// 消费一次。
    ///
    /// 调用方必须先 [`Approval::covers`] 成功再调用本方法。两个方法分开，是因为"批准消费"
    /// 必须发生在签发真的成功之后——先扣再用的话，一次签发失败会白白吃掉一次批准。
    pub fn consume(&mut self) -> Result<(), ContractError> {
        if self.remaining() == 0 {
            return Err(ContractError::ApprovalExhausted {
                approval_id: self.approval_id.to_string(),
                max_uses: self.max_uses,
            });
        }
        self.used = self.used.saturating_add(1);
        Ok(())
    }
}
