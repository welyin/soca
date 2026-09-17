//! 公共消息信封（§7.1）。
//!
//! 所有进入系统的消息都走这个信封，字段集合在 schema v1 冻结。加字段就必须提升
//! [`crate::SCHEMA_VERSION`]，这正是 P0 门槛"冻结最小消息版本"的含义。

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::validate::assert_unique;
use crate::{
    ActionId, BlobRef, BootId, ContractError, DataClass, EgressVerdict, EventId, IdempotencyKey,
    MediaType, ModelVersion, Monotonic, PermissionScope, SCHEMA_VERSION, Sha256Hex, SourceId,
    TaskId, ToolId, WallClock,
};

/// 小消息信封的字节上限（§10.4）。超过它就说明应该在内容仓里落对象、只传引用。
pub const MAX_SMALL_MESSAGE_ENVELOPE_BYTES: usize = 64 * 1024;

/// 用户输入通道。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserChannel {
    /// 桌面聊天框。
    Chat,
    /// 按键说话语音。
    PushToTalk,
    /// 图形审批界面。
    ApprovalUi,
    /// 设备控制开关（暂停、静音、撤销采集）。
    DeviceControl,
}

/// 模型派生变换的种类。派生物必须指向原始事件与模型版本（§7.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationKind {
    /// 语音转写。
    Asr,
    /// 光学字符识别。
    Ocr,
    /// 视觉语言模型描述。
    Vlm,
    /// 摘要。
    Summarization,
    /// 翻译。
    Translation,
    /// 分类或打标。
    Classification,
}

/// 消息来源。原始内容与来源分开存（§6.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Provenance {
    /// 用户在明确通道上的直接输入。
    User {
        /// 具体通道。
        channel: UserChannel,
    },
    /// 设备适配器观测。
    Sensor {
        /// 适配器标识。
        adapter: SourceId,
    },
    /// 模型或确定性算法对既有事件的派生。不覆盖原始观测。
    Derived {
        /// 被派生的原始事件。
        source_event_id: EventId,
        /// 做这次派生的模型版本。确定性算法使用其算法版本。
        model_version: ModelVersion,
        /// 变换种类。
        transform: DerivationKind,
    },
    /// 工具执行结果作为新观测回流。
    Tool {
        /// 工具标识。
        tool_id: ToolId,
        /// 对应的动作标识。
        action_id: ActionId,
    },
}

impl Provenance {
    /// 这条消息的内容是否具有"指令权限"。
    ///
    /// 只有用户在明确通道上的直接输入才可以驱动意图。屏幕文字、麦克风转写、文档内容、
    /// 工具输出都属于**数据**，即使内容看起来像系统指令（§6.1 恶意页面/文档不具有指令
    /// 权限；§11.1 屏幕或麦克风得到的文字不能替代桌面明确授权；§14 不以可能误识别的语音
    /// 自动批准高风险操作）。
    pub fn is_instruction_authority(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// 派生消息的原始事件引用。
    pub fn original_event(&self) -> Option<&EventId> {
        match self {
            Self::Derived { source_event_id, .. } => Some(source_event_id),
            _ => None,
        }
    }
}

/// 载荷引用。小消息可以内联，大载荷只传引用（§10.4）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PayloadRef {
    /// 内联载荷。
    Inline {
        /// 媒体类型。
        media_type: MediaType,
        /// 载荷正文。
        body: String,
    },
    /// 内容仓引用。
    Blob {
        /// 对象引用。
        blob_ref: BlobRef,
        /// 媒体类型。
        media_type: MediaType,
        /// 字节数，用于计账。
        bytes: u64,
        /// 内容校验和（§9.3：先校验和耐久化，再提交数据库引用）。
        sha256: Sha256Hex,
    },
}

/// 公共消息信封（§7.1 的十六个字段）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// 事件标识。
    pub event_id: EventId,
    /// 契约版本。
    pub schema_version: u16,
    /// 来源标识。
    pub source_id: SourceId,
    /// 来源世代。适配器重建或流重建后递增，旧序列终止（§11.2）。
    pub source_epoch: u32,
    /// 进程启动标识。
    pub boot_id: BootId,
    /// 同源同 epoch 内的序列号。
    pub source_sequence: u64,
    /// 所属任务。
    pub task_id: TaskId,
    /// 因果父事件。跨设备、跨机的先后关系靠它，而不是靠壁钟。
    pub causal_parent_ids: Vec<EventId>,
    /// 观测时刻。
    pub observed_at_utc: WallClock,
    /// 接收时刻的单调读数。
    pub received_monotonic: Monotonic,
    /// 来源与派生关系。
    pub provenance: Provenance,
    /// 载荷引用。
    pub payload_ref: PayloadRef,
    /// 这份数据允许推动的最高动作等级。
    pub permission_scope: PermissionScope,
    /// 数据类别。
    pub data_class: DataClass,
    /// 失效时刻。`None` 表示不解自过期；用户命令、授权撤回、动作回执和审计提交不得因为
    /// 过期被静默丢弃（§10.4）。
    pub expires_at: Option<WallClock>,
    /// 幂等键。
    pub idempotency_key: IdempotencyKey,
}

impl Envelope {
    /// 按当前 schema 版本构造信封。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: EventId,
        source_id: SourceId,
        source_epoch: u32,
        boot_id: BootId,
        source_sequence: u64,
        task_id: TaskId,
        causal_parent_ids: Vec<EventId>,
        observed_at_utc: WallClock,
        received_monotonic: Monotonic,
        provenance: Provenance,
        payload_ref: PayloadRef,
        permission_scope: PermissionScope,
        data_class: DataClass,
        expires_at: Option<WallClock>,
        idempotency_key: IdempotencyKey,
    ) -> Self {
        Self {
            event_id,
            schema_version: SCHEMA_VERSION,
            source_id,
            source_epoch,
            boot_id,
            source_sequence,
            task_id,
            causal_parent_ids,
            observed_at_utc,
            received_monotonic,
            provenance,
            payload_ref,
            permission_scope,
            data_class,
            expires_at,
            idempotency_key,
        }
    }

    /// 校验信封自洽性。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ContractError::SchemaVersionMismatch {
                expected: SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }

        // 单调读数的 boot 必须与信封一致，否则该读数的比较毫无意义。
        if self.received_monotonic.boot_id != self.boot_id {
            return Err(ContractError::BootIdMismatch);
        }

        assert_unique(&self.causal_parent_ids, "causal_parent_ids")?;
        if self.causal_parent_ids.contains(&self.event_id) {
            return Err(ContractError::SelfCausalParent(self.event_id.to_string()));
        }

        if let Some(expires_at) = self.expires_at
            && expires_at <= self.observed_at_utc
        {
            return Err(ContractError::InvalidTimeWindow {
                start: self.observed_at_utc.to_string(),
                end: expires_at.to_string(),
            });
        }

        // 大载荷只传引用；内联载荷受小消息上限约束。
        if matches!(self.payload_ref, PayloadRef::Inline { .. }) {
            let encoded = serde_json::to_vec(self).map_err(|_| ContractError::EncodingFailed {
                field: "envelope",
            })?;
            if encoded.len() > MAX_SMALL_MESSAGE_ENVELOPE_BYTES {
                return Err(ContractError::PayloadTooLarge {
                    limit: MAX_SMALL_MESSAGE_ENVELOPE_BYTES,
                    actual: encoded.len(),
                });
            }
        }

        Ok(())
    }

    /// 是否在某时刻已过期。
    pub fn is_expired_at(&self, at: WallClock) -> bool {
        self.expires_at.is_some_and(|expires_at| at >= expires_at)
    }

    /// 与另一事件的顺序关系。
    ///
    /// 只有同源、同世代、同 boot 才有确定顺序。其余情况返回 `None`，调用方必须改用因果引用
    /// （§7.1：跨设备保留时钟不确定性，不按一个壁钟字段假定全局先后）。
    pub fn ordering_with(&self, other: &Self) -> Option<Ordering> {
        let same_stream = self.source_id == other.source_id
            && self.source_epoch == other.source_epoch
            && self.boot_id == other.boot_id;
        if !same_stream {
            return None;
        }
        Some(self.source_sequence.cmp(&other.source_sequence))
    }

    /// 校验这份数据是否允许推动指定等级的动作。
    pub fn authorize_level(&self, level: crate::ActionLevel) -> Result<(), ContractError> {
        self.permission_scope.permits(level)
    }

    /// 出站判定。
    ///
    /// 返回 `Denied` 时永远不得出站；返回 `RequiresPolicyApproval` 也不等于放行，仍需一次
    /// 单独的策略批准（§8）。本方法只回答"类别是否允许"，不代替审批。
    pub fn cloud_egress_verdict(&self) -> EgressVerdict {
        self.data_class.cloud_egress()
    }
}
