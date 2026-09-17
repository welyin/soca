//! 路由：请求 → 响应。
//!
//! 这一层是**纯函数式的**：给定主体、会话、请求与时刻，产出一个响应。socket 的部分在
//! [`crate::serve`] 里，薄到不需要测。这样做的好处是全部行为都能在测试里直接断言，
//! 而不是靠起一个服务再发 HTTP 请求去猜。

use serde_json::{json, Value};
use soca_contracts::{
    ActionLevel, Candidate, CapabilityPolicyRef, DataClass, ExplorationQuota, GoalBudget, GoalId,
    PermissionScope, SelectionPolicy, UserChannel, WallClock,
};
use soca_core::{RoundOutcome, Subject};
use soca_model_gateway::{GatewayError, ModelCredentials};

use crate::http::{Request, Response};
use crate::model::ConsoleModel;
use crate::page;
use crate::session::Session;

/// 控制台新建目标时给的额度。
///
/// 刻意写死而不是让请求指定：界面上的一个输入框不应该能决定主体能用多少资源。
/// §12.2"授权不给子单元自动扩大"的同一条思路——额度的来源越少，越说清是谁定的。
const CHAT_ACTIONS: u32 = 16;
const CHAT_ACTIVATIONS: u32 = 32;
const CHAT_TOKENS: u32 = 8192;
const CHAT_EXPLORATIONS: u32 = 4;

/// 处理一条请求。
pub fn handle(
    subject: &mut Subject,
    model: &mut ConsoleModel,
    session: &Session,
    request: &Request,
    at: WallClock,
) -> Response {
    // 页面本身不校验 token（token 由服务端注入页面），其余一律校验。
    if request.path == "/" {
        return Response::html(page::render(session.token()));
    }

    if !Session::host_is_loopback(request) {
        return Response::text(403, "只接受来自 loopback 的请求（§8 的来源检查）");
    }
    if !session.authorizes(request) {
        return Response::text(401, "缺少或错误的会话令牌");
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/state") => match subject.public_state(at) {
            Ok(state) => Response::json(200, &json!(state)),
            Err(error) => internal(error.to_string()),
        },
        ("GET", "/api/model") => Response::json(200, &model.summary()),
        ("POST", "/api/model") => connect_model(subject, model, request),
        ("POST", "/api/model/reset") => reset_model(subject, model),
        ("POST", "/api/loop") => run_loop(subject, request, at),
        ("POST", "/api/select") => select_ladder(subject, request, at),
        ("POST", "/api/chat") => chat(subject, request, at),
        ("POST", "/api/observe") => observe(subject, request, at),
        ("POST", "/api/consult") => consult(subject, request, at),
        ("POST", "/api/abandon") => abandon(subject, request, at),
        ("GET", _) | ("POST", _) => Response::text(404, "没有这个接口"),
        _ => Response::text(405, "只接受 GET 与 POST"),
    }
}

/// 配置远端模型端点。
///
/// 这个接口**收**密钥但**不回**密钥：返回的是 [`ConsoleModel::summary`]，只有指纹。
fn connect_model(subject: &mut Subject, model: &mut ConsoleModel, request: &Request) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };

    // 缺省值指向 DeepSeek：用户只需要填密钥。换一家兼容端点填 base_url 即可。
    let base_url = payload
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(ModelCredentials::DEEPSEEK_BASE_URL);
    let model_name = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(ModelCredentials::DEEPSEEK_MODEL);
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let allow_personal = payload
        .get("allow_private_egress")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let credentials = match ModelCredentials::new(base_url, model_name, api_key) {
        Ok(credentials) => credentials,
        Err(error) => {
            // 凭据构造的错误信息里只有端点与类别名，没有密钥内容。
            return Response::json(
                400,
                &json!({"error": "invalid_credentials", "detail": error.to_string()}),
            );
        }
    };

    match model.apply(subject, credentials, allow_personal) {
        Ok(()) => Response::json(200, &model.summary()),
        Err(error) => Response::json(
            500,
            &json!({"error": "apply_failed", "detail": error.to_string()}),
        ),
    }
}

fn reset_model(subject: &mut Subject, model: &mut ConsoleModel) -> Response {
    match model.reset(subject) {
        Ok(()) => Response::json(200, &model.summary()),
        Err(error) => Response::json(
            500,
            &json!({"error": "reset_failed", "detail": error.to_string()}),
        ),
    }
}

fn internal(detail: String) -> Response {
    // 错误细节不外泄到界面上：§13 要求引擎异常日志只进私有评估域，公开返回错误码。
    // 这里保留一句话的原因是这个控制台只有本机一个用户，看到自己的错误不是泄露。
    Response::json(
        500,
        &json!({"error": "internal_error", "detail": detail}),
    )
}

fn body_json(request: &Request) -> Result<Value, Response> {
    if request.body.trim().is_empty() {
        return Err(Response::text(400, "请求体必须是 JSON"));
    }
    serde_json::from_str(&request.body).map_err(|_| Response::text(400, "请求体不是合法 JSON"))
}

fn chat(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(message) = payload.get("message").and_then(Value::as_str) else {
        return Response::text(400, "缺少 message 字段");
    };
    if message.trim().is_empty() {
        return Response::text(400, "消息不能为空");
    }

    // 用户消息走 Chat 通道——这是 §4.1 L6 那一层唯一接受的出处。
    let scope = PermissionScope {
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略"),
        max_action_level: ActionLevel::A1,
    };
    let budget = GoalBudget::new(
        CHAT_ACTIONS,
        CHAT_ACTIVATIONS,
        CHAT_TOKENS,
        3_600_000,
    )
    .expect("固定额度合法");

    let goal_id = match subject.delegate(
        message.trim(),
        UserChannel::Chat,
        scope,
        budget,
        ExplorationQuota::new(CHAT_EXPLORATIONS),
        at,
        None,
    ) {
        Ok(id) => id,
        Err(error) => return internal(error.to_string()),
    };
    if let Err(error) = subject.accept(&goal_id, at) {
        return internal(error.to_string());
    }

    Response::json(
        200,
        &json!({
            "goal_id": goal_id.to_string(),
            "state": "active",
            "note": "目标已受理。它现在可以用本条消息之外的任何方式推进——\
                     提出候选、申请观测、或者请求一次模型咨询。",
        }),
    )
}

fn observe(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(target) = payload.get("subject").and_then(Value::as_str) else {
        return Response::text(400, "缺少 subject 字段");
    };
    let data_class = match parse_data_class(payload.get("data_class").and_then(Value::as_str)) {
        Ok(class) => class,
        Err(message) => return Response::text(400, message),
    };

    match subject.observe(target, data_class, at) {
        Ok(record) => Response::json(
            200,
            &json!({
                "subject": record.observation.subject,
                "value": record.observation.value,
                "evidence_ref": record.observation.evidence_ref.to_string(),
                "sequence": record.sequence,
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

fn consult(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(raw) = payload.get("goal_id").and_then(Value::as_str) else {
        return Response::text(400, "缺少 goal_id 字段");
    };
    let Ok(goal_id) = GoalId::new(raw) else {
        return Response::text(400, "goal_id 必须以 goal: 开头");
    };

    // 只读 schema：控制台这一版不允许模型通过界面提动作。动作要经过 L4 与执行许可，
    // 而那条通路还没有界面。
    let schema = soca_contracts::OutputSchema::read_only();
    match subject.consult_model(&goal_id, schema, at) {
        Ok(consultation) => {
            let proposals: Vec<Value> = consultation
                .output
                .proposals
                .iter()
                .map(|proposal| {
                    json!({
                        "kind": proposal.candidate.kind().as_str(),
                        "summary": summarize(&proposal.candidate),
                        "rationale": proposal.rationale,
                        "self_report": proposal.self_report.reported_value,
                        "self_report_note": "模型自报数值，不是校准概率（§3.2）",
                        "cited_evidence": proposal
                            .candidate
                            .evidence_refs()
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();

            Response::json(
                200,
                &json!({
                    "goal_id": consultation.goal_id.to_string(),
                    "attempts": consultation.attempts,
                    "goal": consultation.context.goal,
                    "evidence_given": consultation.context.evidence.len(),
                    "past_outcomes_given": consultation.context.past_outcomes.len(),
                    "allowed_candidates": consultation
                        .context
                        .output_schema
                        .allowed
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>(),
                    "input_tokens": consultation.output.usage.input_tokens,
                    "output_tokens": consultation.output.usage.output_tokens,
                    "proposals": proposals,
                }),
            )
        }
        Err(soca_core::CoreError::GoalNotFound { .. }) => Response::text(404, "没有这个目标"),
        Err(soca_core::CoreError::Contract(error)) => {
            Response::json(409, &json!({"error": "refused", "detail": error.to_string()}))
        }
        Err(soca_core::CoreError::Gateway(GatewayError::Contract(error))) => {
            // §8 的边界被触发时的返回：**不是**"内部错误"，而是一次明确的拒绝。
            // 把它归成 500 会让"模型引用了不存在的证据"看起来像程序坏了。
            Response::json(
                409,
                &json!({"error": "model_output_refused", "detail": error.to_string()}),
            )
        }
        Err(error) => internal(error.to_string()),
    }
}

/// 连续跑若干轮 §6 的闭环。
///
/// 轮数上限 32：一个 HTTP 请求不该能把主体占住任意长的时间。真正需要长跑的场景要的是
/// 一个后台调度器（§4.1 L4 的调度器与预算仲裁），而不是一个更长的请求——把它塞进请求里，
/// 界面会一直转圈，而你也无从知道它跑到哪一轮了。
fn run_loop(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let risk = match parse_risk(payload.get("risk").and_then(Value::as_str)) {
        Ok(level) => level,
        Err(message) => return Response::text(400, message),
    };
    let rounds = payload
        .get("rounds")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 32) as i64;

    let policy = SelectionPolicy::default();
    let mut reports = Vec::new();
    for index in 0..rounds {
        // 每一轮的时刻往后挪一秒。同一时刻连推多轮会让审计账里的时间序看起来像同一件事，
        // 而"先观测、后得出结论"这个顺序正是靠时间序读出来的。
        let report = match subject.run_round(&policy, risk, at.plus_seconds(index + 1)) {
            Ok(report) => report,
            Err(error) => {
                return Response::json(
                    500,
                    &json!({"error": "round_failed", "detail": error.to_string()}),
                );
            }
        };
        let finished = matches!(report.outcome, RoundOutcome::Finished { .. });
        reports.push(json!({
            "round": report.round,
            "outcome": report.outcome,
            "selected": report.selected,
            "activations": report.activations,
        }));
        if finished {
            break;
        }
    }

    Response::json(
        200,
        &json!({
            "risk": risk.as_str(),
            "rounds": reports,
            "state": subject.public_state(at).ok(),
        }),
    )
}

/// §4.1 L3、§6 第 4–5 步：在当前候选上做检验，然后选一条推进。
///
/// 把检验与选择一起暴露成一个接口，与 [`Subject::select`] 只提供一个入口是同一个理由：
/// 允许分开调用就等于允许跳过检验直接选，那样证据门槛只剩一个数字，没有任何东西在它前面
/// 核对结论。检查了什么，从返回的 `reviews` 里逐条看得到。
fn select_ladder(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let risk = match parse_risk(payload.get("risk").and_then(Value::as_str)) {
        Ok(level) => level,
        Err(message) => return Response::text(400, message),
    };

    let policy = SelectionPolicy::default();
    match subject.select(&policy, risk, at) {
        Ok((candidates, selection)) => {
            let listed: Vec<Value> = candidates
                .candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| {
                    json!({
                        "index": index,
                        "kind": candidate.kind().as_str(),
                        "summary": summarize(candidate),
                        "evidence_count": candidate.evidence_refs().len(),
                    })
                })
                .collect();

            Response::json(
                200,
                &json!({
                    "risk": risk.as_str(),
                    "required_evidence": selection.required_evidence,
                    "rationale": selection.rationale,
                    "outcome": selection.outcome,
                    "candidates": listed,
                    "reviews": selection.reviews,
                    "unresolved": candidates.unresolved.iter().map(|item| item.question.clone()).collect::<Vec<_>>(),
                    "conflicts": candidates.conflicts.iter().map(|item| item.subject_ref.clone()).collect::<Vec<_>>(),
                }),
            )
        }
        Err(error) => internal(error.to_string()),
    }
}

fn parse_risk(raw: Option<&str>) -> Result<ActionLevel, &'static str> {
    match raw.unwrap_or("a1").to_ascii_lowercase().as_str() {
        "a0" => Ok(ActionLevel::A0),
        "a1" => Ok(ActionLevel::A1),
        "a2" => Ok(ActionLevel::A2),
        "a3" => Ok(ActionLevel::A3),
        "a4" => Ok(ActionLevel::A4),
        _ => Err("risk 只能是 a0/a1/a2/a3/a4"),
    }
}

fn abandon(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(raw) = payload.get("goal_id").and_then(Value::as_str) else {
        return Response::text(400, "缺少 goal_id 字段");
    };
    let Ok(goal_id) = GoalId::new(raw) else {
        return Response::text(400, "goal_id 必须以 goal: 开头");
    };

    match subject.abandon(&goal_id, at) {
        Ok(count) => Response::json(200, &json!({"abandoned": count})),
        Err(error) => Response::text(404, error.to_string()),
    }
}

fn parse_data_class(raw: Option<&str>) -> Result<DataClass, &'static str> {
    match raw.unwrap_or("personal") {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "sensitive" => Ok(DataClass::Sensitive),
        "secret" => Ok(DataClass::Secret),
        _ => Err("data_class 只能是 public/personal/sensitive/secret"),
    }
}

fn summarize(candidate: &Candidate) -> String {
    match candidate {
        Candidate::Claim { statement, .. } => statement.clone(),
        Candidate::RequestObservation { subject_ref, reason } => {
            format!("申请观测 {subject_ref}：{reason}")
        }
        Candidate::RequestTool { tool_id, .. } => format!("请求调用工具 {tool_id}"),
        Candidate::RequestAction { intent } => format!("提请动作 {}", intent.action_id),
    }
}
