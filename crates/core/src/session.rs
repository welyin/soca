//! 单主体闭环会话（§6 的一次完整认知循环）。
//!
//! §6 把一次循环拆成九步。本模块把它们做成**可以被逐步调用**的方法，而不是一个不可分割的
//! `run()`：
//!
//! | §6 步骤 | 方法 |
//! |---|---|
//! | 1 观测写成事件 | [`Session::observe`] |
//! | 3 动作前记录预测 | [`Session::predict`] |
//! | 4–5 候选结构与证据存在性检查 | [`Session::admit`]（由存储层核对预测存在性与任务归属） |
//! | 6 执行代理检查能力令牌 | [`Session::dispatch`] |
//! | 7 执行回执 + 后置条件核对 | [`Session::settle`] / [`Session::verify`] |
//!
//! 之所以不合并成一步，是因为 §17 的故障注入要求"在意图前/后、执行后回执前、快照提交前/后"
//! 都能停在半路。只有每一步都是独立可调用的，崩溃点才能被精确表达，而不是靠注入假异常。

use soca_contracts::{
    ActionId, ActionIntent, ActionReceipt, ActionLevel, BootId, CapabilityPolicyRef, DataClass,
    DataState, Envelope, EventId, EvidenceRef, ExecutionPermit, Expectation, IdempotencyKey,
    Monotonic, Observation, PermissionScope, Prediction, PredictionRef, Provenance, Sha256Hex,
    SourceId, TaskId, TimeWindow, Uncertainty, UnitId, Verdict, WallClock,
};
use soca_storage::{Admission, AppendOutcome, Store};

use crate::broker::{ActionBroker, BrokerOutcome};
use crate::error::CoreError;
use crate::os::{write_content, write_subject_ref};

/// 观测值中表示"对象不存在"的哨兵。
///
/// 用一个不可能与版本摘要混淆的字面量，而不是空串：空串会被误读成"文件存在但为空"。
/// §11.1 要求"传感器未获许可、断开或质量不足时输出不可用/未知，不伪装成没有声音/没有人
/// 在场"，这里遵循同一条原则。
pub const ABSENT_VALUE: &str = "<absent>";

/// 一次观测的落库结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationRecord {
    /// 提交序。
    pub sequence: i64,
    /// 事件标识。
    pub event_id: EventId,
    /// 观测内容。
    pub observation: Observation,
}

/// 投递结果。
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchOutcome {
    /// 已执行并拿到回执。
    Receipted(Box<ActionReceipt>),
    /// 副作用已发生但结果未知。**不得重发**，必须走恢复流程。
    Interrupted {
        /// 动作标识。
        action_id: String,
    },
    /// 未执行。
    Refused {
        /// 拒绝原因。
        reason: String,
    },
}

impl DispatchOutcome {
    /// 这次投递是否真的产生了副作用。
    ///
    /// `Refused` 与 `Interrupted` 的区别很重要：前者确定世界没变，后者不确定。
    pub fn did_apply(&self) -> bool {
        matches!(self, Self::Receipted(_) | Self::Interrupted { .. })
    }
}

/// 一轮写任务的完整结果。
#[derive(Debug, Clone, PartialEq)]
pub struct RoundReport {
    /// 作用对象。
    pub subject_ref: String,
    /// 动作前观测。
    pub observation_before: ObservationRecord,
    /// 动作前预测。
    pub prediction: Prediction,
    /// 受理结果。
    pub admission: Admission,
    /// 投递结果。
    pub dispatch: DispatchOutcome,
    /// 执行回执。结果未知时为 `None`。
    pub receipt: Option<ActionReceipt>,
    /// 动作后观测。
    pub observation_after: Option<ObservationRecord>,
    /// 后置条件判定。
    pub outcome: Option<soca_contracts::OutcomeVerified>,
}

/// 一次闭环会话。
#[derive(Debug)]
pub struct Session<'a> {
    store: &'a mut Store,
    broker: &'a mut ActionBroker,
    unit: UnitId,
    task: TaskId,
    source: SourceId,
    source_epoch: u32,
    boot: BootId,
    permission_scope: PermissionScope,
    data_class: DataClass,
}

impl<'a> Session<'a> {
    /// 建立会话。
    ///
    /// 观测事件的权限范围默认封顶 A2（指定目录生成/重命名文件），数据类别默认
    /// `Personal`。两者都可以用 [`Session::with_policy`] 收紧，但**没有放宽的入口**：
    /// §12.2 要求"授权不给子单元自动扩大"。
    pub fn new(
        store: &'a mut Store,
        broker: &'a mut ActionBroker,
        unit: UnitId,
        task: TaskId,
        boot: BootId,
    ) -> Self {
        Self {
            store,
            broker,
            unit,
            task,
            source: SourceId::new("device:simulated-fs").expect("固定来源"),
            source_epoch: 1,
            boot,
            permission_scope: PermissionScope {
                capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                    .expect("固定能力策略"),
                max_action_level: ActionLevel::A2,
            },
            data_class: DataClass::Personal,
        }
    }

    /// 收紧本次会话的权限范围与数据类别。
    pub fn with_policy(mut self, permission_scope: PermissionScope, data_class: DataClass) -> Self {
        self.permission_scope = permission_scope;
        self.data_class = data_class;
        self
    }

    /// 会话所属单元。
    pub fn unit(&self) -> &UnitId {
        &self.unit
    }

    /// 会话所属任务。
    pub fn task(&self) -> &TaskId {
        &self.task
    }

    /// §6.1：读取环境状态并写成事件。
    pub fn observe(
        &mut self,
        subject_ref: &str,
        at: WallClock,
    ) -> Result<ObservationRecord, CoreError> {
        let value = self
            .broker
            .os()
            .version_of(subject_ref)
            .unwrap_or(ABSENT_VALUE)
            .to_string();

        let event_id = EventId::generate();
        let evidence_ref = EvidenceRef::new(format!("obs:{event_id}"))?;
        let observation = Observation {
            subject: subject_ref.to_string(),
            value,
            evidence_ref,
            derived_from: Vec::new(),
            observed_by: self.unit.clone(),
        };

        // 流内序号从库里取，而不是由会话自己记：会话重建后计数器从零重来会撞上唯一约束。
        let sequence = self
            .store
            .next_stream_sequence(&self.source, self.source_epoch, &self.boot)?;

        let envelope = Envelope::new(
            event_id,
            self.source.clone(),
            self.source_epoch,
            self.boot,
            sequence,
            self.task.clone(),
            Vec::new(),
            at,
            Monotonic::new(self.boot, sequence.saturating_mul(1_000_000)),
            // §6.1 / §11.1：传感器观测是数据，不是指令。
            Provenance::Sensor {
                adapter: self.source.clone(),
            },
            soca_contracts::PayloadRef::Inline {
                media_type: soca_contracts::MediaType::new("application/json")?,
                body: serde_json::to_string(&observation)?,
            },
            self.permission_scope.clone(),
            self.data_class,
            None,
            IdempotencyKey::new(format!("idem:{}", event_id))?,
        );

        let outcome: AppendOutcome = self.store.append_event(&envelope, at)?;
        Ok(ObservationRecord {
            sequence: outcome.sequence(),
            event_id,
            observation,
        })
    }

    /// §6.3：记录动作前预测。返回 `false` 表示同一份预测已经记录过。
    pub fn predict(&mut self, prediction: &Prediction, at: WallClock) -> Result<bool, CoreError> {
        Ok(self
            .store
            .record_prediction(&self.unit, &self.task, prediction, at)?)
    }

    /// §6.4–§6.5：提交动作意图。存储层会核对许可、参数摘要与动作前预测。
    pub fn admit(
        &mut self,
        intent: &ActionIntent,
        permit: &ExecutionPermit,
        at: WallClock,
    ) -> Result<Admission, CoreError> {
        Ok(self.store.admit_action(&self.task, intent, permit, at)?)
    }

    /// §6.6：标记投递并交给执行代理。
    pub fn dispatch(
        &mut self,
        intent: &ActionIntent,
        permit: &ExecutionPermit,
        at: WallClock,
    ) -> Result<DispatchOutcome, CoreError> {
        // 先落投递标记，再交接。反过来的话，崩溃会留下"副作用已发生但账上仍是 PREPARED"，
        // 恢复流程会把它当成可安全重投的动作——那正是 §7.3 要避免的重复副作用。
        self.store.mark_dispatched(intent.action_id.as_str(), at)?;

        let trigger = DataState::ExecutionPermit(permit.clone());
        Ok(match self.broker.submit(&trigger, intent, at)? {
            BrokerOutcome::Receipted(receipt) => DispatchOutcome::Receipted(receipt),
            BrokerOutcome::Interrupted { action_id } => DispatchOutcome::Interrupted { action_id },
            BrokerOutcome::Refused { reason } => DispatchOutcome::Refused { reason },
        })
    }

    /// §6.7：记录执行回执。回执不构成后置条件验证。
    pub fn settle(&mut self, receipt: &ActionReceipt, at: WallClock) -> Result<(), CoreError> {
        Ok(self.store.settle_receipt(receipt, at)?)
    }

    /// §6.8：用新观测判定后置条件。
    pub fn verify(
        &mut self,
        action_id: &ActionId,
        prediction_ref: &PredictionRef,
        expectation: &Expectation,
        observation: &Observation,
        at: WallClock,
    ) -> Result<soca_contracts::OutcomeVerified, CoreError> {
        let verdict = evaluate(expectation, observation);
        let outcome = soca_contracts::OutcomeVerified::new(
            action_id.clone(),
            prediction_ref.clone(),
            verdict,
            vec![observation.evidence_ref.clone()],
        )?;
        self.store.record_outcome(&outcome, at)?;
        Ok(outcome)
    }

    /// 跑完一整轮"写入并核对"的闭环。
    ///
    /// P0 的预测内容由运行时按工具后置条件生成；接入模型之后，预测内容改由单元提出、
    /// 运行时只校验其结构合规（期望必须可检查、必须有失败条件）。这条界限是有意的：
    /// 让确定性代码负责可判定的部分，模型负责它擅长的部分（§8）。
    pub fn run_write_round(
        &mut self,
        intent: &ActionIntent,
        permit: &ExecutionPermit,
        at: WallClock,
    ) -> Result<RoundReport, CoreError> {
        let subject_ref = write_subject_ref(&intent.parameters)
            .ok_or_else(|| CoreError::UnresolvedSubject(intent.action_id.to_string()))?;
        let content = write_content(&intent.parameters)
            .ok_or_else(|| CoreError::UnresolvedSubject(intent.action_id.to_string()))?;
        let expected = Sha256Hex::of_bytes(content.as_bytes()).to_string();

        let observation_before = self.observe(&subject_ref, at)?;

        let prediction = Prediction::new(
            intent.prediction_ref.clone(),
            subject_ref.clone(),
            format!("{subject_ref} 的版本变为 {expected}"),
            Expectation::VersionEquals {
                subject_ref: subject_ref.clone(),
                expected: expected.clone(),
            },
            TimeWindow::new(at, at.plus_seconds(60))?,
            vec![format!("{subject_ref} 的版本不是 {expected}")],
            Uncertainty {
                probability: None,
                notes: vec!["本地确定性写入，不需要概率字段".to_string()],
            },
        )?;
        self.predict(&prediction, at)?;

        let admission = self.admit(intent, permit, at)?;
        if !admission.is_dispatchable() {
            return Err(CoreError::AdmissionDenied {
                action_id: intent.action_id.to_string(),
                reason: admission
                    .denial_reason
                    .clone()
                    .unwrap_or_else(|| "受理方未给出原因".to_string()),
            });
        }

        let dispatch = self.dispatch(intent, permit, at)?;

        let mut report = RoundReport {
            subject_ref,
            observation_before,
            prediction,
            admission,
            dispatch,
            receipt: None,
            observation_after: None,
            outcome: None,
        };

        if let DispatchOutcome::Receipted(receipt) = &report.dispatch {
            let receipt = (**receipt).clone();
            self.settle(&receipt, at)?;
            let observation_after = self.observe(&report.subject_ref, at)?;
            let outcome = self.verify(
                &receipt.action_id,
                &report.prediction.prediction_ref,
                &report.prediction.expectation,
                &observation_after.observation,
                at,
            )?;
            report.observation_after = Some(observation_after);
            report.outcome = Some(outcome);
            report.receipt = Some(receipt);
        }

        Ok(report)
    }
}

/// 用干预测与实测观测判定后置条件（§6.7）。
///
/// 纯函数，不碰存储也不碰环境：这样"判定规则"可以被单独测试，也不会因为读到了别处的状态
/// 而产生不可复现的结果。
pub fn evaluate(expectation: &Expectation, observation: &Observation) -> Verdict {
    // 期望作用对象与观测对象不是同一个 → 无法判定。§6.7 明确允许 `inconclusive`，
    // 而不是勉强给出一个支持/否定。
    if expectation.subject_ref() != observation.subject {
        return Verdict::Inconclusive;
    }
    match expectation {
        Expectation::VersionEquals { expected, .. } => {
            if observation.value == *expected {
                Verdict::Supported
            } else {
                Verdict::Refuted
            }
        }
        Expectation::Absent { .. } => {
            if observation.value == ABSENT_VALUE {
                Verdict::Supported
            } else {
                Verdict::Refuted
            }
        }
        Expectation::Present { .. } => {
            if observation.value == ABSENT_VALUE {
                Verdict::Refuted
            } else {
                Verdict::Supported
            }
        }
    }
}
