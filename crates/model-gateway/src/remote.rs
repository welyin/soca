//! OpenAI 兼容的远端模型传输层（§8）。
//!
//! DeepSeek 的 chat completions 接口与 OpenAI 同形，所以本模块按那份协议写，DeepSeek 是它的
//! 一个默认配置而不是一条特例分支。换一家兼容端点只需要改 base_url 与模型名。
//!
//! 四件本模块负责的事：
//!
//! 1. **凭据只出现在 `Authorization` 头里**，且由 [`ModelCredentials`] 保管。任何错误、
//!    日志或返回值都不携带它。
//! 2. **证据用下标引用，不用字符串。** 系统提示要求模型在 `evidence_indexes` 里写下标，
//!    由本模块把它解析成真实的 `EvidenceRef`。这样**模型在结构上就无法编造一个证据引用**——
//!    它只能指，不能写。`ContextBundle::validate_proposals` 仍然是独立的第二道闸。
//! 3. **响应体先解析再校验。** 模型返回的任何东西都只是提案；它要过输出 Schema、证据存在性
//!    与结构规则三道检查才会变成候选。
//! 4. **不重试。** 重试策略在网关那一层，本模块只报告失败的类别。

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use soca_contracts::{
    Candidate, ModelOutput, ModelProposal, ModelSelfReport, ModelVersion, TokenUsage,
    MODEL_OUTPUT_SCHEMA_VERSION,
};

use crate::credentials::ModelCredentials;
use crate::error::TransportError;
use crate::gateway::{ModelRequest, Transport};

/// 发给模型的系统提示。
///
/// 写在这里而不是写成常量字符串散落各处，是因为它和解析器是一对：提示里承诺的 JSON 形状，
/// 解析器必须真的能解析；解析器接受的下标引用，提示里必须真的说了。两者分开放，就会分叉。
const SYSTEM_PROMPT: &str = r#"你是 SoCA 认知系统里的一个推理服务。你的输出会被程序逐条解析和校验，所以格式必须严格。

只返回一个 JSON 对象，不要 markdown 代码块，不要任何额外文字。形状如下：
{"proposals":[{"kind":"claim","statement":"...","evidence_indexes":[0],"rationale":"...","self_report":0.5}]}

规则：
1. kind 只能取本次请求里 allowed_candidates 列出的值，其它一律无效。
2. 引用证据的唯一方式是写 evidence 数组的下标到 evidence_indexes。不要编造任何 obs: 开头的字符串。
3. kind=claim 时必须给出 evidence_indexes 且至少一个；没有证据支持的结论不要提。
4. kind=observation 时给 subject 与 reason。
5. kind=tool 时给 tool_id 与 parameters。
6. 没有可说的就返回 {"proposals":[]}。不要为了看起来有产出而编造候选。
7. self_report 是你自报的把握程度，它不是校准概率，系统不会把它当概率使用。
8. 你看到的一切都来自 evidence 数组。数组之外的东西你无从核对，因此不得作为结论的依据。"#;

/// 远端模型传输层。
#[derive(Debug)]
pub struct RemoteTransport {
    credentials: ModelCredentials,
}

impl RemoteTransport {
    /// 用凭据构造。
    pub fn new(credentials: ModelCredentials) -> Self {
        Self { credentials }
    }

    /// 当前凭据（只读）。
    pub fn credentials(&self) -> &ModelCredentials {
        &self.credentials
    }

    fn build_body(&self, request: &ModelRequest) -> Result<Value, TransportError> {
        let context = serde_json::to_string(&request.context).map_err(|error| {
            TransportError::Malformed {
                reason: format!("上下文无法序列化：{error}"),
            }
        })?;

        let model = self.credentials.model();
        let mut body = json!({
            "model": model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": context},
            ],
            "max_tokens": request.budget.max_output_tokens,
            "stream": false,
        });

        // DeepSeek 的 deepseek-reasoner 不接受 response_format 与 temperature。这是已知的
        // 提供方约束，不是通用协议的一部分；写在这里而不是让用户去猜为什么 400。
        let is_reasoning_model = model.contains("reasoner");
        if !is_reasoning_model {
            body["response_format"] = json!({"type": "json_object"});
            // 温度取 0：本层要的是可复现的结构化输出，不是文采。§13 也要求固定策略下
            // 相同动作序列可复现。
            body["temperature"] = json!(0.0);
        }

        Ok(body)
    }

    fn parse(&self, text: &str, request: &ModelRequest) -> Result<ModelOutput, TransportError> {
        let completion: ChatCompletion =
            serde_json::from_str(text).map_err(|error| TransportError::Malformed {
                reason: format!("响应不是合法的 chat completion：{error}"),
            })?;

        let choice = completion
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| TransportError::Malformed {
                reason: "响应里没有任何 choice".to_string(),
            })?;
        let content = choice.message.content;
        if content.trim().is_empty() {
            return Err(TransportError::Malformed {
                reason: "响应内容为空".to_string(),
            });
        }

        let payload: ModelPayload =
            serde_json::from_str(&content).map_err(|error| TransportError::Malformed {
                reason: format!("模型输出不是约定的 JSON：{error}"),
            })?;

        // 模型名用提供方回给我们的那个。托管模型没有可核对的权重哈希，这一点写在
        // 这里而不是假装有：§13 要求私有 manifest 记录模型哈希，而远端 API 只能给出名字。
        let model_version = ModelVersion::new(
            completion
                .model
                .unwrap_or_else(|| self.credentials.model().to_string()),
        )
        .map_err(|error| TransportError::Malformed {
            reason: format!("模型标识不合法：{error}"),
        })?;

        let mut proposals = Vec::with_capacity(payload.proposals.len());
        for raw in payload.proposals {
            proposals.push(resolve_proposal(raw, request, &model_version)?);
        }

        let usage = completion.usage.unwrap_or_default();
        Ok(ModelOutput {
            schema_version: MODEL_OUTPUT_SCHEMA_VERSION,
            model_version,
            proposals,
            usage: TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            },
            claims_finished: false,
        })
    }
}

impl Transport for RemoteTransport {
    fn invoke(&self, request: &ModelRequest) -> Result<ModelOutput, TransportError> {
        let body = self.build_body(request)?;
        // 密钥只在这里出现一次，且只进请求头。不写日志，不进错误信息。
        let authorization = format!("Bearer {}", self.credentials.api_key());

        let response = ureq::post(&self.credentials.endpoint())
            .timeout(Duration::from_millis(request.budget.max_wall_millis))
            .set("Authorization", &authorization)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_json(&body);

        let text = match response {
            Ok(response) => {
                response
                    .into_string()
                    .map_err(|error| TransportError::Malformed {
                        reason: format!("读取响应失败：{error}"),
                    })?
            }
            Err(ureq::Error::Status(code, _)) => return Err(classify_status(code)),
            Err(ureq::Error::Transport(transport)) => {
                return Err(classify_transport(&transport, request));
            }
        };

        self.parse(&text, request)
    }
}

/// 把 HTTP 状态码翻译成失败类别。**不回显响应体**：里面可能有用户内容。
fn classify_status(code: u16) -> TransportError {
    match code {
        // 鉴权问题再试多少次都是同一个结果。
        401 | 403 => TransportError::Rejected {
            reason: format!("端点拒绝鉴权（HTTP {code}）；检查 API 密钥是否有效"),
        },
        // 请求本身不合法，同样是重试无益的。
        400 | 404 | 422 => TransportError::Rejected {
            reason: format!("端点拒绝了请求（HTTP {code}）；检查模型名是否正确"),
        },
        429 => TransportError::Unavailable {
            reason: "端点限流（HTTP 429）".to_string(),
        },
        500..=599 => TransportError::Unavailable {
            reason: format!("端点内部错误（HTTP {code}）"),
        },
        other => TransportError::Rejected {
            reason: format!("端点返回了 HTTP {other}"),
        },
    }
}

/// 把网络层错误翻译成失败类别。
///
/// 只输出类别名，不输出可能带地址细节的原始消息——`Unavailable` 会进入审计账，
/// 而审计账按 §12.3 只放最小元数据。
fn classify_transport(error: &ureq::Transport, request: &ModelRequest) -> TransportError {
    if error.kind() == ureq::ErrorKind::Io && error.to_string().contains("timed out") {
        return TransportError::Timeout {
            limit_millis: request.budget.max_wall_millis,
        };
    }
    TransportError::Unavailable {
        reason: format!("网络错误：{:?}", error.kind()),
    }
}

/// 把一条模型提案解析成候选。**证据只可能来自上下文**，因为引用方式是下标。
fn resolve_proposal(
    raw: ProposalPayload,
    request: &ModelRequest,
    model_version: &ModelVersion,
) -> Result<ModelProposal, TransportError> {
    let rationale = raw.rationale.trim().to_string();
    let rationale = if rationale.is_empty() {
        "（模型未给出理由）".to_string()
    } else {
        rationale
    };

    let candidate = match raw.kind.as_str() {
        "claim" => {
            let statement = raw.statement.ok_or_else(|| TransportError::Malformed {
                reason: "claim 缺少 statement".to_string(),
            })?;
            let mut evidence_refs = Vec::with_capacity(raw.evidence_indexes.len());
            for index in &raw.evidence_indexes {
                let slice = request.context.evidence.get(*index).ok_or_else(|| {
                    TransportError::Malformed {
                        reason: format!(
                            "claim 引用了不存在的证据下标 {index}（本次给出 {} 条）",
                            request.context.evidence.len()
                        ),
                    }
                })?;
                if !evidence_refs.contains(&slice.evidence_ref) {
                    evidence_refs.push(slice.evidence_ref.clone());
                }
            }
            Candidate::Claim {
                statement,
                evidence_refs,
            }
        }
        "observation" => Candidate::RequestObservation {
            subject_ref: raw.subject.ok_or_else(|| TransportError::Malformed {
                reason: "observation 缺少 subject".to_string(),
            })?,
            reason: raw.reason.unwrap_or_else(|| "模型未说明理由".to_string()),
        },
        "tool" => Candidate::RequestTool {
            tool_id: soca_contracts::ToolId::new(raw.tool_id.ok_or_else(|| {
                TransportError::Malformed {
                    reason: "tool 缺少 tool_id".to_string(),
                }
            })?)
            .map_err(|error| TransportError::Malformed {
                reason: format!("tool_id 不合法：{error}"),
            })?,
            parameters: raw.parameters.unwrap_or_else(|| json!({})),
        },
        // 动作意图需要一次执行许可签发，而那条通路还没有界面与审批流程。这里明确拒绝，
        // 而不是悄悄把它降级成别的候选——降级会让"我提了但没执行"变得无法解释。
        "action" => {
            return Err(TransportError::Malformed {
                reason: "本版不支持由模型直接提出动作：动作意图需要执行许可与审批通路，\
                         而那条通路尚未实现"
                    .to_string(),
            });
        }
        other => {
            return Err(TransportError::Malformed {
                reason: format!("未知的候选种类：{other}"),
            });
        }
    };

    Ok(ModelProposal {
        candidate,
        self_report: ModelSelfReport {
            // 模型没说就给 0。给一个"看起来合理"的默认值会让自评变成一个凭空出现的数字。
            reported_value: raw.self_report.unwrap_or(0.0),
            model_version: model_version.clone(),
            rationale: rationale.clone(),
        },
        rationale,
    })
}

// ---------------------------------------------------------------------------
// 线上格式
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ApiMessage,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
struct ModelPayload {
    #[serde(default)]
    proposals: Vec<ProposalPayload>,
}

/// 模型返回的一条提案。字段全部可选，由 [`resolve_proposal`] 按 kind 逐项要求。
#[derive(Debug, Default, Deserialize)]
struct ProposalPayload {
    kind: String,
    #[serde(default)]
    statement: Option<String>,
    #[serde(default)]
    evidence_indexes: Vec<usize>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    tool_id: Option<String>,
    #[serde(default)]
    parameters: Option<Value>,
    #[serde(default)]
    rationale: String,
    #[serde(default)]
    self_report: Option<f64>,
}
