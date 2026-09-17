//! 观测、假设、预测与概率校准（§3.2、§6.3、§7.2）。
//!
//! 本模块要挡住两类常见退化：
//!
//! * 把模型自评置信度直接当成概率真值。§3.2 明确禁止，因此
//!   [`ModelSelfReport`] **故意不提供**到 [`CalibratedProbability`] 的转换，
//!   而未校准的概率在 `value` 上必须是 `None`（显式 `unknown`）。
//! * 把"我觉得会变好"写成无法证伪的预测。§6.3 要求动作前记录对象、预计变化、时间窗、
//!   失败条件及不确定性，所以 [`Prediction`] 没有"只有一句话"的构造路径。

use serde::{Deserialize, Serialize};

use crate::validate::assert_disjoint;
use crate::{
    ContractError, EvidenceRef, ModelVersion, PredictionRef, TimeWindow, UnitId,
};

/// 某来源在某时刻的观测。
///
/// 原文存在内容仓，这里只保留可审计的摘要与引用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// 观测对象。
    pub subject: String,
    /// 观测到内容的简短描述。
    pub value: String,
    /// 本条观测自己的证据引用。
    pub evidence_ref: EvidenceRef,
    /// 派生观测必须指向原始观测（§7.1：派生物指向原始事件，不覆盖原始观测）。
    pub derived_from: Vec<EvidenceRef>,
    /// 记录这条观测的单元。
    pub observed_by: UnitId,
}

/// 单元或模型对现象的解释。
///
/// §7.2 要求假设带支持、反对与未知项。本 crate 进一步要求：同一份证据不能同时出现在
/// 支持与反对两侧，那说明冲突没被解决，不能当作"有支持的候选"上交。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    /// 主张内容。
    pub claim: String,
    /// 支持证据。
    pub supporting: Vec<EvidenceRef>,
    /// 反对证据。
    pub against: Vec<EvidenceRef>,
    /// 已知未知项。无法验证收益时必须如实返回不确定（§13.1）。
    pub unknowns: Vec<String>,
    /// 提出假设的单元。
    pub proposed_by: UnitId,
}

impl Hypothesis {
    /// 校验引用完整性。
    pub fn validate(&self) -> Result<(), ContractError> {
        crate::validate::assert_unique(&self.supporting, "hypothesis.supporting")?;
        crate::validate::assert_unique(&self.against, "hypothesis.against")?;
        assert_disjoint(&self.supporting, &self.against, "hypothesis")?;
        Ok(())
    }

    /// 是否既无支持也无未知项。这种主张只是断言，不应进入候选竞争。
    pub fn is_bare_assertion(&self) -> bool {
        self.supporting.is_empty() && self.unknowns.is_empty()
    }
}

/// 校准来源。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CalibrationSource {
    /// 有实测样本的校准。§17 要求报告 Brier/ECE 并标注样本范围。
    Measured {
        /// 样本数。
        sample_count: u32,
        /// Brier 分数。
        brier: f64,
        /// 期望校准误差。
        ece: f64,
        /// 校准方法说明。
        method: String,
    },
    /// 明确标注"未校准"。
    Uncalibrated,
}

/// 带来源、模型版本与时间范围的概率字段。
///
/// §3.2 的原文要求：belief 中的概率字段必须注明预测对象、校准来源、模型版本、时间范围及
/// `unknown` 状态。这四个字段在这里是结构上必需，不是可选注释。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibratedProbability {
    /// 预测对象。
    pub subject: String,
    /// 时间范围。
    pub horizon: TimeWindow,
    /// 给出该数值的模型版本。
    pub model_version: ModelVersion,
    /// 校准来源。
    pub calibration: CalibrationSource,
    /// 概率值。`None` 表示 `unknown`，必须显式存在而不是省略字段。
    pub value: Option<f64>,
}

impl CalibratedProbability {
    /// 校验校准自洽性。
    pub fn validate(&self) -> Result<(), ContractError> {
        match (&self.calibration, self.value) {
            // 未校准就不允许给出数值：这正是"自评置信度不直接转成概率真值"。
            (CalibrationSource::Uncalibrated, Some(_)) => Err(ContractError::UncalibratedProbability),
            (CalibrationSource::Uncalibrated, None) => Ok(()),
            (CalibrationSource::Measured { sample_count, .. }, Some(value)) => {
                if *sample_count == 0 {
                    return Err(ContractError::EmptyCalibrationSamples);
                }
                if !(0.0..=1.0).contains(&value) {
                    return Err(ContractError::ProbabilityOutOfRange {
                        actual: value.to_string(),
                    });
                }
                Ok(())
            }
            (CalibrationSource::Measured { sample_count, .. }, None) => {
                if *sample_count == 0 {
                    return Err(ContractError::EmptyCalibrationSamples);
                }
                Ok(())
            }
        }
    }
}

/// 模型自己说的"我很确定"。
///
/// 这是一个**记录用**类型，不是概率类型。它没有到 [`CalibratedProbability`] 的转换路径，
/// 唯一出口是 [`ModelSelfReport::into_uncalibrated`]，而那个转换会把数值丢掉并标记
/// `Uncalibrated` + `None`。自评分数本身进入模型调用日志，不进入 belief。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelfReport {
    /// 模型自报的数值。
    pub reported_value: f64,
    /// 模型版本。
    pub model_version: ModelVersion,
    /// 模型给出的理由摘要。
    pub rationale: String,
}

impl ModelSelfReport {
    /// 转成显式未校准的概率字段：数值被丢弃，状态为 `unknown`。
    pub fn into_uncalibrated(self, subject: String, horizon: TimeWindow) -> CalibratedProbability {
        CalibratedProbability {
            subject,
            horizon,
            model_version: self.model_version,
            calibration: CalibrationSource::Uncalibrated,
            value: None,
        }
    }
}

/// 不确定性描述。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Uncertainty {
    /// 可选的、有校准来源的概率。
    pub probability: Option<CalibratedProbability>,
    /// 其他不确定性说明（例如"传感器质量不足"）。
    pub notes: Vec<String>,
}

impl Uncertainty {
    /// 无任何量化信息、也无说明的"不确定性"等于什么都没说。
    pub fn is_empty(&self) -> bool {
        self.probability.is_none() && self.notes.is_empty()
    }
}

/// 机器可检查的期望（§7.2："动作前可检查的预期结果"）。
///
/// §7.2 用"可检查"限定预测，§6.3 又要求记录对象、预计变化、时间窗、失败条件。散文部分给人
/// 读，本类型给确定性检查器读——两者都必须存在：
///
/// * 只有散文，后验结果无法判定，`Verdict` 只能靠人拍脑袋；
/// * 只有结构化字段，审计记录读起来像机器码，用户无法据此追责。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expectation {
    /// 指定对象在动作后的版本应等于给定值。
    VersionEquals {
        /// 对象引用。核验时与观测的 `subject` 逐字比较。
        subject_ref: String,
        /// 期望的版本标识。
        expected: String,
    },
    /// 指定对象在动作后应不存在。
    Absent {
        /// 对象引用。
        subject_ref: String,
    },
}

impl Expectation {
    /// 期望指向的对象引用。
    pub fn subject_ref(&self) -> &str {
        match self {
            Self::VersionEquals { subject_ref, .. } | Self::Absent { subject_ref } => subject_ref,
        }
    }
}

/// 动作前可检查的预期结果（§6.3）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prediction {
    /// 预测自身的引用。动作意图必须指向它。
    pub prediction_ref: PredictionRef,
    /// 预测对象。
    pub subject: String,
    /// 预计变化。给人读的散文，不参与判定。
    pub expected_change: String,
    /// 机器可检查的期望。参与判定，不给人读。
    pub expectation: Expectation,
    /// 预期成立的时间窗。
    pub window: TimeWindow,
    /// 失败条件。至少一条，否则无法证伪。
    pub failure_conditions: Vec<String>,
    /// 不确定性。
    pub uncertainty: Uncertainty,
}

impl Prediction {
    /// 构造并校验。
    ///
    /// 拒绝没有失败条件的"预测"：§17 的闭环正确性验收要求先记录预测、动作后逐条后验检查，
    /// 没有失败条件就无法判定后验结果。
    ///
    /// `expectation` 也是必需参数：没有机器可检查的期望，后验判定就只能靠猜。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        prediction_ref: PredictionRef,
        subject: String,
        expected_change: String,
        expectation: Expectation,
        window: TimeWindow,
        failure_conditions: Vec<String>,
        uncertainty: Uncertainty,
    ) -> Result<Self, ContractError> {
        if failure_conditions.is_empty() {
            return Err(ContractError::MissingRefs {
                field: "prediction.failure_conditions",
            });
        }
        // 期望必须作用在同一个对象上，否则"预测 A、检查 B"这种错位会静默通过。
        if expectation.subject_ref() != subject {
            return Err(ContractError::ExpectationSubjectMismatch {
                subject,
                expectation_subject: expectation.subject_ref().to_string(),
            });
        }
        if let Some(probability) = &uncertainty.probability {
            probability.validate()?;
        }
        Ok(Self {
            prediction_ref,
            subject,
            expected_change,
            expectation,
            window,
            failure_conditions,
            uncertainty,
        })
    }
}
