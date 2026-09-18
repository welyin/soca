//! 路由：请求 → 响应。
//!
//! 这一层是**纯函数式的**：给定主体、会话、请求与时刻，产出一个响应。socket 的部分在
//! [`crate::serve`] 里，薄到不需要测。这样做的好处是全部行为都能在测试里直接断言，
//! 而不是靠起一个服务再发 HTTP 请求去猜。

use serde_json::{json, Value};
use soca_contracts::{
    ActionLevel, Approval, ApprovalId, Candidate, CapabilityPolicyRef, DataClass, EventId,
    EvidenceRef, ExplorationQuota, GoalBudget, GoalId, GoalState, GrantScope, MemoryId,
    ModelBackend, ModelReservation, PermissionScope, PlannerPolicy, ResourceEnvelope,
    SelectionPolicy, Sha256Hex,
    StrategyCandidate, StrategyVersion, UserChannel, WallClock,
};
use soca_core::{CoreError, Correction, Resources, RetentionPolicy, Scheduler, Subject};
use soca_model_gateway::{GatewayError, ModelCredentials};
use soca_storage::audit::AuditCategory;

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
        ("POST", "/api/grant") => grant_capability(subject, request, at),
        ("POST", "/api/revoke") => revoke_capability(subject, request, at),
        ("POST", "/api/retention") => enforce_retention(subject, request, at),
        ("POST", "/api/forget") => forget_memory(subject, request, at),
        ("POST", "/api/correct") => correct(subject, request, at),
        // 建议与启用分开两个路径：§13.2 说系统"可建议……但必须经过准入"，
        // 而"提了就等于启用了"正是那句话最容易落空的地方。
        ("POST", "/api/learning") => propose_strategy(subject, request, at),
        ("POST", "/api/learning/apply") => admit_strategy(subject, request, at),
        ("POST", "/api/delegate_write") => delegate_write_goal(subject, request, at),
        ("POST", "/api/write") => request_write(subject, request, at),
        ("POST", "/api/approve") => grant_approval(subject, request, at),
        ("POST", "/api/resume") => resume_goal(subject, request, at),
        ("POST", "/api/body") => body(subject, request, at),
        // 与 `/api/resume` 分开：那一个是"目标等到了批准，放它走"，这一个是
        // "整台机器停/动"。两者的作用域差着一个数量级，共用一个路径会让调用方
        // 以为自己放开的是一个目标，而实际上放开的是全部。
        ("POST", "/api/policy/pause") => pause(subject, request, at),
        ("POST", "/api/policy/resume") => resume(subject, request, at),
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
                // 正文**不放进这个响应**，只报引用与长度。它不是这条消息的一部分——
                // 每轮观测都把全文抄进消息里，正是 §4.1 L2 那句"不无限复制"要挡的事。
                // 要看正文走 `/api/body`，按引用取。
                "body_ref": record.observation.body_ref.as_ref().map(ToString::to_string),
            }),
        ),
        // 授权被撤回不是"服务器出错了"。返回 500 会让界面显示一个故障，而用户需要知道的
        // 是"这个能力已经被收回去了，要恢复得重新授予"（§12.1）。
        Err(CoreError::CapabilityRevoked { capability }) => Response::json(
            403,
            &json!({"error": "capability_revoked", "capability": capability}),
        ),
        // 暂停也不是故障，而且是**暂时**的：423 说的是"现在是锁着的"，与 403
        // 那句"你没有这个权限"不同。两者的处置完全不一样——一个去按恢复，一个去重新授权。
        Err(CoreError::Paused { reason }) => {
            Response::json(423, &json!({"error": "paused", "reason": reason}))
        }
        Err(error) => internal(error.to_string()),
    }
}

/// 按引用取回一次观测的正文（§9.3）。
///
/// 单独一个接口，是因为正文**不是**随每条消息一起走的东西：信封只带引用，要用的时候才去取。
/// 而"取不回"本身是有信息量的——证据被撤回、内容被按保留期清理，都表现在这里。
fn body(subject: &mut Subject, request: &Request, _at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(raw) = payload.get("evidence_ref").and_then(Value::as_str) else {
        return Response::text(400, "缺少 evidence_ref 字段");
    };
    let Ok(reference) = EvidenceRef::new(raw) else {
        return Response::text(400, "evidence_ref 形状不合法（只接受 obs:/tool-result:/receipt:）");
    };

    match subject.observed_body(&reference) {
        // `None` 不是错误。三种取不回（已撤回、本来就没有正文、按保留期清理过）在这里
        // 都是"没有"，而把它们报成 500 会让界面显示一个故障，掩盖掉真正的原因。
        Ok(body) => Response::json(
            200,
            &json!({
                "evidence_ref": raw,
                "available": body.is_some(),
                "chars": body.as_ref().map(|text| text.chars().count()),
                "body": body,
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
                    // §6 第 3 步：草稿要成为候选。这两个数与 `proposals` 的长度一起看，
                    // 才知道"模型提的话有没有真的参与竞争"。
                    "queued_candidates": consultation.queued_candidates,
                    "unbacked_proposals": consultation.unbacked_proposals,
                    "goal": consultation.context.goal,
                    "evidence_given": consultation.context.evidence.len(),
                    // 模型**实际看到**的证据，含正文。
                    //
                    // 这一份里的正文与 `/api/body` 那份是同一个东西的两种取法，用途不同：
                    // 那个是"按引用取回内容"，这个是"看模型看到了什么"。后者是把上下文摊开
                    // 给人看——而"正文只出现在 evidence 这一位上"正是 §11.1 那条边界的形状，
                    // 摊开才看得出它有没有被守住。
                    "evidence": consultation
                        .context
                        .evidence
                        .iter()
                        .map(|slice| {
                            json!({
                                "evidence_ref": slice.evidence_ref.to_string(),
                                "subject_ref": slice.subject_ref,
                                "observed_value": slice.observed_value,
                                "data_class": slice.data_class.as_str(),
                                "body": slice.body,
                            })
                        })
                        .collect::<Vec<_>>(),
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
        // 同 observe：暂停不是故障，而且是暂时的。
        Err(soca_core::CoreError::Paused { reason }) => {
            Response::json(423, &json!({"error": "paused", "reason": reason}))
        }
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

/// 能力策略名。控制台的委托都用它，所以它也是默认值。
const DEFAULT_CAPABILITY: &str = "cap:read-selected-folder";

/// 授予一项能力策略（§12.1）。
fn grant_capability(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let payload = body_json(request).unwrap_or(Value::Null);
    let name = payload
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_CAPABILITY);
    let capability = match CapabilityPolicyRef::new(name) {
        Ok(capability) => capability,
        Err(error) => return Response::text(400, error.to_string()),
    };

    // 范围是**必填**的。给一个默认值会让最省事的那次调用恰好拿到最宽的授权，而"省事"和
    // "更宽"之间不该有这种关系——§12.1 那句"范围限定授权"里的范围，正是这里要填的东西。
    let scope = match payload.get("prefix").and_then(Value::as_str) {
        Some(prefix) => match GrantScope::under(prefix) {
            Ok(scope) => scope,
            Err(error) => return Response::text(400, error.to_string()),
        },
        None if payload.get("anywhere").and_then(Value::as_bool) == Some(true) => {
            GrantScope::anywhere()
        }
        None => {
            return Response::text(
                400,
                "缺少 prefix；要授予不按路径限定的授权，显式传 anywhere: true",
            );
        }
    };
    let described = if scope.is_unbounded() {
        "不按路径限定".to_string()
    } else {
        scope
            .prefixes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("、")
    };

    match subject.grant_capability(capability, scope, at) {
        Ok(was_new) => Response::json(
            200,
            &json!({
                "capability": name,
                "scope": described,
                "was_new": was_new,
                "granted": subject
                    .granted_capabilities()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 撤回一项能力策略，并让已经由它产生的记忆立即失效（§12.1、§12.3）。
///
/// 接口一次做两件事，因为它们**必须**一起发生：只停将来、不清过去的话，一份通过已撤回授权
/// 读到的内容会继续被检索、被引用。把它们拆成两个接口，就是把"忘了清过去"变成一个
/// 可以发生的调用顺序。
fn revoke_capability(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let payload = body_json(request).unwrap_or(Value::Null);
    let name = payload
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_CAPABILITY);
    let capability = match CapabilityPolicyRef::new(name) {
        Ok(capability) => capability,
        Err(error) => return Response::text(400, error.to_string()),
    };

    match subject.revoke_capability(&capability, at) {
        Ok(report) => Response::json(
            200,
            &json!({
                "capability": name,
                "was_granted": report.was_granted,
                "events_covered": report.events_covered,
                "memories_invalidated": report.memories_invalidated,
                // 撤回的第二个后果（§7.2）：簇手里"还能拿来下结论的材料"也失效了。
                // 与上一条分开报，因为只有上一条时看起来像已经做完了。
                "evidence_retracted": report.evidence_retracted,
                // 第三份名单：不再可能进入模型上下文的那几条。
                "context_evidence_removed": report.context_evidence_removed,
                "awaiting_purge": report.awaiting_purge,
                "granted": subject
                    .granted_capabilities()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 执行一次保留期清理（§12.3）。
///
/// 返回里把"隐藏"与"清理"分开报。§12.3 要的是"先写 tombstone 使查询立即不可见，**再异步
/// 清理**，并给用户完成状态"——把两个数合成一个"已完成"，用户就无从知道内容是不是真的走了。
fn enforce_retention(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let payload = body_json(request).unwrap_or(Value::Null);
    let policy = RetentionPolicy {
        audit_retention_days: payload
            .get("audit_retention_days")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, 3_650) as i64,
        purge: payload.get("purge").and_then(Value::as_bool).unwrap_or(true),
        // 默认 0：退休之后当场清掉。非零值留出一段"看不见了但还拿得回来"的窗口，
        // 而那个窗口只对内容对象有意义——记忆的隐藏与清理之间没有恢复入口。
        content_purge_grace_days: payload
            .get("content_purge_grace_days")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(3_650) as i64,
        prune_audit: payload
            .get("prune_audit")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    };

    match subject.enforce_retention(&policy, at) {
        Ok(report) => Response::json(
            200,
            &json!({
                "tombstoned": report.tombstoned.len(),
                "purged": report.purged,
                // 内容对象那一侧单独报：§12.3 的"对话与转写"与记忆走的是同一条两步路，
                // 但它们是不同的东西，合并成一个数字之后，界面就答不出"走掉的是哪一类"。
                "content_retired": report.retired_content.len(),
                "content_purged": report.content_purged,
                "content_bytes_freed": report.content_bytes_freed,
                "audit_pruned": report.audit_pruned,
                "awaiting_purge": report.awaiting_purge,
                "content_awaiting_purge": report.content_awaiting_purge,
                "details": report.tombstoned,
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 用户说"记错了"（§14）。
///
/// 与 `/api/forget` 分开，不是因为撤掉的东西不同——两者都会让记忆不可见——而是因为
/// **留下的东西**不同：纠错会在事件账上留下一条带用户原话的记录（通道是 `correction`），
/// 而删除只留一句"用户删过"。用户下次问"这条为什么不见了"，前者答得出来。
fn correct(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let note = payload
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("用户指出这条记错了")
        .to_string();

    // 两种指名方式对应**能证明的范围不一样**，所以不合并成一个字段：
    // 指名一条记忆能确定的只有那一条；指名一条原始事件，由它派生的那些是查得出来的。
    let correction = if let Some(memory_id) = payload.get("memory_id").and_then(Value::as_str) {
        Correction::Conclusion {
            memory_id: memory_id.to_string(),
        }
    } else if let Some(raw) = payload.get("event_id").and_then(Value::as_str) {
        // 界面上看到的是证据引用（`obs:<uuid>`），而派生关系挂在**原始事件**上。
        // 这个转换属于这一层：`obs:` 是给人和界面看的写法，`EventId` 是域内的标识。
        // 让域层去认前缀，等于把展示格式塞进契约。
        let event_id = match EvidenceRef::new(raw).ok().and_then(|r| r.origin_event_id()) {
            Some(id) => id,
            None => match EventId::parse(raw) {
                Ok(id) => id,
                Err(error) => return Response::text(400, error.to_string()),
            },
        };
        Correction::Source {
            event_id: event_id.to_string(),
        }
    } else {
        return Response::text(
            400,
            "缺少 memory_id 或 event_id：纠错必须指名一条记忆或一条原始事件",
        );
    };

    match subject.correct(&correction, &note, at) {
        Ok(report) => Response::json(
            200,
            &json!({
                "event_id": report.event_id,
                "target": report.target,
                "retracted": report.retracted,
                // 与 `retracted.len()` 分开报：用户指名一条、系统撤掉五条时，
                // 那四条是**系统自己判断**该撤的，必须让它自己说出来。
                "derived": report.derived,
                "awaiting_purge": report.awaiting_purge,
                "note": "纠错本身也进了事件账——只在对话框里回一句「好的」的话，账上什么也没发生",
            }),
        ),
        // 指名了一条已经不存在的记忆会是 404：那是用户指错了东西，值得说出来。
        // 而那次纠错**已经记进事件账了**——顺序是先记再改。
        Err(error) => Response::text(404, error.to_string()),
    }
}

/// 提一个策略改进的候选（§13.2 的"系统可**建议**"）。
///
/// 单独一条路径，而且**它什么都不改**。"建议"与"启用"分开成两个接口，是因为 §13.2 把
/// 这两件事分得很清楚（"系统可建议……但必须经过准入"），而把它们并成一个接口
/// ——"提了就等于启用了"——正是那句话最容易落空的地方。
fn propose_strategy(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let risk = match parse_level(payload.get("risk").and_then(Value::as_str)) {
        Ok(level) => level,
        Err(message) => return Response::text(400, message),
    };

    let holdout = match subject.holdout(at) {
        Ok(holdout) => holdout,
        Err(error) => return internal(error.to_string()),
    };

    match subject.propose_strategy(risk, at) {
        Ok(Some(candidate)) => Response::json(
            200,
            &json!({
                "outcome": "suggested",
                "candidate": candidate,
                // 保留任务集的**规模**要一起报：它决定了这次建议有多少依据。空集时
                // 闸会拒绝任何候选，而用户看到的应当是这个数字，不是一句"被拒了"。
                "holdout": {
                    "known_right": holdout.known_right.len(),
                    "known_wrong": holdout.known_wrong.len(),
                },
                "note": "这只是建议。它要送进 /api/learning/apply 才算数——\
                         而那道闸对这一个提议与对别处的提议一视同仁。",
            }),
        ),
        Ok(None) => Response::json(
            200,
            &json!({
                "outcome": "nothing_to_suggest",
                "holdout": {
                    "known_right": holdout.known_right.len(),
                    "known_wrong": holdout.known_wrong.len(),
                },
                "note": "没有可提的：没有错案，或者当前门槛已经拦得住手上最严重的那一条。\
                         「没有可提的」不是失败——提一个空改动会让版本号每次都变，\
                         而审计上看起来系统在不停地学。",
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 把一个策略候选送进准入闸（§13.2 的"必须经过准入"）。
///
/// 候选**取自请求体**，而不是后端重新提一遍。取后者的话，"用户批准的那一份"与
/// "实际启用的那一份"之间就多了一个可以不一致的环节——而这个环节出事时看不出来：
/// 两边都是系统自己算的，版本号也一样。
fn admit_strategy(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let risk = match parse_level(payload.get("risk").and_then(Value::as_str)) {
        Ok(level) => level,
        Err(message) => return Response::text(400, message),
    };

    let Some(policy) = payload.get("policy") else {
        return Response::text(400, "缺少 policy：候选要原样带回来，不能只报一个版本号");
    };
    let Ok(policy) = serde_json::from_value::<SelectionPolicy>(policy.clone()) else {
        return Response::text(400, "policy 不是一份合法的选择策略");
    };
    let Some(raw_version) = payload.get("version").and_then(Value::as_str) else {
        return Response::text(400, "缺少 version");
    };
    let Ok(version) = StrategyVersion::new(raw_version) else {
        return Response::text(400, "version 不是合法的策略版本标识");
    };
    let based_on: Vec<String> = payload
        .get("based_on")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let rationale = payload
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("由界面提交")
        .to_string();

    let candidate = StrategyCandidate {
        version,
        policy,
        rationale,
        based_on,
    };

    match subject.admit_strategy(&candidate, risk, at) {
        Ok(admission) => Response::json(
            200,
            &json!({
                "admitted": admission.is_admitted(),
                // 判定与它跑出来的报告一起给。只给"准／不准"的话，被拒时用户不知道
                // 是"会误伤"还是"没解决它声称的问题"——而这两件事要做的事完全不同。
                "admission": admission,
                "strategy_version": subject.strategy_version(),
                "strategy": subject.strategy(),
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 用户显式删掉一条记忆（§12.3 的"用户可随时删除"）。
fn forget_memory(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(raw) = payload.get("memory_id").and_then(Value::as_str) else {
        return Response::text(400, "缺少 memory_id");
    };
    let memory_id = match MemoryId::new(raw) {
        Ok(id) => id,
        Err(error) => return Response::text(400, error.to_string()),
    };

    match subject.forget(&memory_id, at) {
        Ok(report) => Response::json(
            200,
            &json!({
                // 0 表示这条此前已经删过。它不是失败——用户看到"删掉了"的提示之后又点了一次，
                // 报错会把他困在一个"删不掉"的界面上，而东西早就不见了。
                "tombstoned": report.tombstoned.len(),
                "awaiting_purge": report.awaiting_purge,
            }),
        ),
        Err(error) => Response::text(404, error.to_string()),
    }
}

/// 委托一个范围内含 A2（写入）的目标。
///
/// 单独一个接口、单独一个名字，而不是让 `/api/chat` 收一个 `level` 参数。**放宽范围必须是
/// 一次显式动作。** §12.2 要求"授权不给子单元自动扩大"；如果放宽与否只是一个请求字段，
/// 那么从"读一读"到"改文件"之间就没有任何东西需要经过人的手——而两次点击之间那次点击，
/// 正是这个设计里唯一一次由人做出的范围决定。
fn delegate_write_goal(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let payload = body_json(request).unwrap_or(Value::Null);
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("在已授权目录里写入文件");

    let scope = PermissionScope {
        capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略"),
        // §12.1："在指定目录生成/重命名文件"就是 A2。这是本次委托的核心内容，也是它
        // 为什么不复用 `/api/chat` 的原因。
        max_action_level: ActionLevel::A2,
    };
    let budget = match GoalBudget::new(CHAT_ACTIONS, CHAT_ACTIVATIONS, CHAT_TOKENS, 3_600_000) {
        Ok(budget) => budget,
        Err(error) => return internal(error.to_string()),
    };

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
            "max_action_level": ActionLevel::A2.as_str(),
        }),
    )
}

/// 投递一次写入（§12.1 的 A2）。
///
/// 它只**投递**：动作要过 L3、策略代理与执行代理三道关才可能真的发生。接口本身不签发
/// 任何东西——把"想要做"与"可以做"合到一个接口里，等于让提交方顺便给自己发许可。
fn request_write(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let Some(subject_ref) = payload.get("subject_ref").and_then(Value::as_str) else {
        return Response::text(400, "缺少 subject_ref");
    };
    let content = payload.get("content").and_then(Value::as_str).unwrap_or_default();

    match subject.request_write(subject_ref, content, at) {
        Ok(action_id) => Response::json(
            200,
            &json!({
                "action_id": action_id.to_string(),
                "pending_actions": subject.pending_actions(),
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 记下一次人工批准（§12.1）。
///
/// 通道由请求指定，默认 `approval_ui`。默认值必须是图形审批界面而不是语音：§14 明确
/// "不以可能误识别的语音**自动**批准高风险操作"，把一个不指定通道的请求默认成语音，
/// 就等于让 A3 的默认路径落在被禁止的那一条上。
fn grant_approval(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let level = match parse_level(payload.get("level").and_then(Value::as_str)) {
        Ok(level) => level,
        Err(message) => return Response::text(400, message),
    };
    let max_uses = payload
        .get("max_uses")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, u8::MAX as u64) as u8;
    let channel = match payload.get("channel").and_then(Value::as_str) {
        None | Some("approval_ui") => UserChannel::ApprovalUi,
        Some("chat") => UserChannel::Chat,
        Some("push_to_talk") => UserChannel::PushToTalk,
        Some(_) => return Response::text(400, "channel 只能是 approval_ui/chat/push_to_talk"),
    };

    // 每次批准一个新标识。把它做成幂等的（比如按等级派生）会让用户第二次点"批准"变成
    // 无效操作——而用户点第二次的语义是"再批一次"，不是"重复提交同一件事"。
    let approval_id = match ApprovalId::new(format!(
        "approval:{}",
        Sha256Hex::of_bytes(format!("{}|{level:?}|{max_uses}|{at}", subject.owner()).as_bytes())
    )) {
        Ok(id) => id,
        Err(error) => return internal(error.to_string()),
    };

    let approval = match Approval::new(
        approval_id,
        subject.owner().clone(),
        level,
        channel,
        at,
        None,
        max_uses,
    ) {
        Ok(approval) => approval,
        Err(error) => return Response::text(400, error.to_string()),
    };

    match subject.grant_approval(&approval, at) {
        Ok(recorded) => Response::json(
            200,
            &json!({
                "approval_id": approval.approval_id.to_string(),
                "level": level.as_str(),
                "channel": channel.as_str(),
                "max_uses": max_uses,
                "recorded": recorded,
            }),
        ),
        Err(error) => internal(error.to_string()),
    }
}

/// 把停在审批上的目标放回进行中（§12.1）。
///
/// 不给 `goal_id` 时对**第一个**等待审批的目标操作。这比"批量恢复"窄，也比它安全：
/// 一次恢复一个，用户看得见每一步。
fn resume_goal(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let payload = body_json(request).unwrap_or(Value::Null);
    let requested = payload.get("goal_id").and_then(Value::as_str);

    let goal_id = match requested {
        Some(raw) => match GoalId::new(raw) {
            Ok(id) => id,
            Err(error) => return Response::text(400, error.to_string()),
        },
        None => match subject
            .goals()
            .iter()
            .find(|goal| goal.state == GoalState::WaitingApproval)
            .map(|goal| goal.goal_id.clone())
        {
            Some(id) => id,
            None => return Response::text(409, "没有等待审批的目标"),
        },
    };

    match subject.resume_after_approval(&goal_id, at) {
        Ok(()) => Response::json(200, &json!({"goal_id": goal_id.to_string(), "state": "active"})),
        Err(error) => Response::text(409, error.to_string()),
    }
}

// 这里原来有一份自己的"通道名 → 字符串"映射。删掉它：契约层本来就有
// [`UserChannel::as_str`]，而两份映射迟早在加新通道时分叉——分叉的那一天，界面显示的通道名
// 与事件账里记的不是同一个，于是"这条批准是从哪个通道来的"这个问题会有两个答案。


/// 连续跑若干轮 §6 的闭环。
///
/// 轮数上限 32：一个 HTTP 请求不该能把主体占住任意长的时间。真正需要长跑的场景要的是
/// 一个后台调度器（§4.1 L4 的调度器与预算仲裁），而不是一个更长的请求——把它塞进请求里，
/// 界面会一直转圈，而你也无从知道它跑到哪一轮了。
fn run_loop(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let Ok(payload) = body_json(request) else {
        return Response::text(400, "请求体不是合法 JSON");
    };
    let risk = match parse_level(payload.get("risk").and_then(Value::as_str)) {
        Ok(level) => level,
        Err(message) => return Response::text(400, message),
    };
    let rounds = payload
        .get("rounds")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 32) as i64;

    // 交给调度器，而不是在这里自己数轮数。§4.1 L4 那一格里的判断——暂停优先、空转退避、
    // 有界——只有一份实现，否则界面上的行为与程序里的行为会各有一套。
    //
    // 每一轮的细节不进这个响应：它们在事件账与审计账里（§9.1"事件账记来源"）。
    // 在这里再抄一份，就成了第三份"发生了什么"，而三份迟早对不上。
    let scheduler = Scheduler {
        max_rounds: rounds as u32,
        idle_limit: payload
            .get("idle_limit")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 16) as u32,
    };

    // §17 的"弹性"：包络由调用方给（"**人为**降低可用内存"），规划器（§10.1）判可行性。
    let resources = match resources_from(&payload) {
        Ok(resources) => resources,
        Err(message) => return Response::text(400, message),
    };

    match scheduler.run(subject, &resources, &SelectionPolicy::default(), risk, at) {
        Ok(report) => Response::json(
            200,
            &json!({
                "risk": risk.as_str(),
                // 把看到的资源状况一并报出去：只报"停了"的话，操作员得自己去猜
                // 是暂停、是资源、还是跑完了。
                "resources": resources,
                "schedule": report.outcome,
                "rounds": report.rounds,
                "state": subject.public_state(at).ok(),
            }),
        ),
        Err(error) => Response::json(
            500,
            &json!({"error": "round_failed", "detail": error.to_string()}),
        ),
    }
}

/// 从请求体里读出一个资源包络，并算出资源状况（§10.1、§17 的"弹性"）。
///
/// §17 那一行说的是"**人为**降低可用内存、GPU OOM、磁盘忙时……"——所以这个输入本来就该由
/// 调用方给，而不是由程序去读硬件：本版没有读真实硬件（§19 末段），而"人为造一份包络"
/// 恰恰是那一行要求的验收方式。
///
/// 没给包络时返回 [`Resources::Unknown`]。**这是刻意的默认**：§17 要的是压力下停住，
/// 而"没人说有多少资源"不是一种压力。让缺配置表现为"永远不跑"的话，一个忘了填表的界面
/// 会看起来像挂了。
fn resources_from(payload: &Value) -> Result<Resources, String> {
    let Some(envelope) = payload.get("envelope") else {
        return Ok(Resources::Unknown);
    };

    // 一次压力事件：不等伸缩滞后（§5.3 的"不能等 10 秒伸缩滞后才处理实际 OOM"）。
    if envelope.get("emergency").and_then(Value::as_bool) == Some(true) {
        return Ok(Resources::Emergency {
            reason: envelope
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("人为制造的压力事件")
                .to_string(),
        });
    }

    let ram_limit_mib = envelope
        .get("ram_limit_mib")
        .and_then(Value::as_u64)
        .ok_or("envelope 缺少 ram_limit_mib")?;
    let cpu_slots = envelope
        .get("cpu_slots")
        .and_then(Value::as_u64)
        .ok_or("envelope 缺少 cpu_slots")?;
    let typed = ResourceEnvelope {
        ram_limit_mib,
        cpu_slots: cpu_slots.min(u64::from(u32::MAX)) as u32,
        // API 不可用时为 0，不猜测（§5 的原文）。
        gpu_allocatable_mib: envelope
            .get("gpu_allocatable_mib")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        telemetry_age_seconds: envelope
            .get("telemetry_age_seconds")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    };

    // 模型画像也由调用方给：**判"能不能跑"要用到模型的 RAM 与 VRAM 占用**，
    // 拿一个默认值去算，会让"GPU OOM"这一类压力永远造不出来。
    let mut model = ModelReservation::default();
    if let Some(declared) = envelope.get("model") {
        model.ram_mib = declared
            .get("ram_mib")
            .and_then(Value::as_u64)
            .unwrap_or(model.ram_mib);
        model.vram_mib = declared
            .get("vram_mib")
            .and_then(Value::as_u64)
            .unwrap_or(model.vram_mib);
        // 后端也必须能声明。少了它，"GPU OOM"这一类压力从产品上就**造不出来**——
        // 契约层会以"只有 GPU 后端可以预约 VRAM"拒掉那份输入，而那条错误看起来像是
        // 参数写错了，不像是"我们要测的那件事暂时测不了"。
        model.backend = match declared.get("backend").and_then(Value::as_str) {
            None => model.backend,
            Some("cpu") => ModelBackend::Cpu,
            Some("gpu") => ModelBackend::Gpu,
            Some("remote") => ModelBackend::Remote,
            Some(other) => return Err(format!("model.backend 只能是 cpu/gpu/remote，收到 {other}")),
        };
        model.remote_authorized = declared
            .get("remote_authorized")
            .and_then(Value::as_bool)
            .unwrap_or(model.remote_authorized);
    }

    // 需求叶数取最小档——本版是单主体，需求本身不增长（§2："不因空闲 RAM 多就生成无任务角色"）。
    // 于是这里的压力只来自**包络**，而那正是 §17 要造的东西。
    let plan = soca_core_topology::plan_topology(
        &typed,
        &model,
        u64::from(soca_contracts::LEAF_PROFILES[0]),
        &PlannerPolicy::default(),
    )
    .map_err(|error| error.to_string())?;

    Ok(Resources::from_plan(&plan))
}

/// §12.1 的全局暂停。
fn pause(subject: &mut Subject, request: &Request, at: WallClock) -> Response {
    let payload = body_json(request).unwrap_or(Value::Null);
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("用户按下暂停")
        .to_string();
    subject.policy_mut().pause(reason.clone());
    // §12.2 要求"审计写失败时默认拒绝新副作用"。这一条**故意**反过来：暂停是**更严**的那一步，
    // 而停住永远是安全的那一侧——写不进审计，也要先停下。把顺序倒过来（先审计、失败就不停）
    // 会让"怎样才能让这台机器停不下来"有了一个答案：把审计写坏。
    let _ = subject.audit(at, AuditCategory::PolicyChanged, "subject", "paused", &reason);
    Response::json(
        200,
        &json!({
            "paused": true,
            "reason": reason,
            // 这三件事**同时**停了。只报"已暂停"而不说停到了哪一层，用户会以为
            // 只是不能再写文件，而实际上采集和模型调用也停了。
            "stopped": ["新许可", "新采集（观测）", "模型调用"],
        }),
    )
}

/// 恢复。§12.1 的暂停是可逆的。
fn resume(subject: &mut Subject, _request: &Request, at: WallClock) -> Response {
    let was = subject.policy().pause_reason().map(str::to_string);
    // 与暂停相反：恢复是**更宽松**的那一步，所以审计必须写在它前面。§12.2 那句
    // "审计写失败时默认拒绝新副作用"的落点正在这里——放开一个已经停住的东西之前，
    // 得先有一条记录说明是谁、在什么时候放的。
    if let Err(error) =
        subject.audit(at, AuditCategory::PolicyChanged, "subject", "resumed", "恢复运行")
    {
        return internal(error.to_string());
    }
    subject.policy_mut().resume();
    Response::json(
        200,
        &json!({
            "paused": false,
            "was": was,
            // 恢复**不等于**把暂停期间错过的补回来。§12.1 的原话是"已发生副作用只能核对、
            // 补偿或由用户处理，不能承诺倒转现实"——恢复只是允许新的动作。
            "note": "恢复只作用于之后；暂停期间该发生而没发生的事不会自动补做",
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
    let risk = match parse_level(payload.get("risk").and_then(Value::as_str)) {
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
                    // §13.1："拒绝有原因和**可重试条件**。" 两者都挂在**这条候选自己**身上，
                    // 而不是只留一句汇总——否则"证据差一条"与"名额满了"在界面上长得一样，
                    // 而用户该做的事完全不同。
                    let rejection = selection.rejection_for(index);
                    json!({
                        "index": index,
                        "kind": candidate.kind().as_str(),
                        "summary": summarize(candidate),
                        "evidence_count": candidate.evidence_refs().len(),
                        "rejected": rejection.is_some(),
                        "rejection_reason": rejection.map(|item| item.reason.clone()),
                        "retry_when": rejection.map(|item| item.retry_when),
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
                    // 每一条候选都在"被选中"或"被拒"之一里，没有第三种去处。
                    "rejections": selection.rejections,
                    "has_retryable": selection.has_retryable_rejection(),
                    "unresolved": candidates.unresolved.iter().map(|item| item.question.clone()).collect::<Vec<_>>(),
                    "conflicts": candidates.conflicts.iter().map(|item| item.subject_ref.clone()).collect::<Vec<_>>(),
                }),
            )
        }
        Err(error) => internal(error.to_string()),
    }
}

fn parse_level(raw: Option<&str>) -> Result<ActionLevel, &'static str> {
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
