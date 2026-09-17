//! 数据类别、动作等级与权限范围（§8、§12.1、§12.2）。
//!
//! 这些不是"建议"，是确定性规则：架构文档 §12.2 明确要求"确定性规则管边界，LLM 可以辅助
//! 识别风险但不能放宽规则"。所以等级与放行要求写成穷举的 `match`，新增等级时编译器会强迫
//! 作者做出决定。

use serde::{Deserialize, Serialize};

use crate::{CapabilityPolicyRef, ContractError};

/// 数据类别，决定能否出站（§8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
