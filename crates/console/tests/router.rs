//! 控制台路由的回归测试。
//!
//! 路由是纯函数，所以这些测试直接说清"哪一条规则被违反了"，不用起服务再发请求去猜。
//! §8 那条"**不把'本机地址'当作无需授权**"是本节的重点。

use std::collections::BTreeMap;

use serde_json::{json, Value};
use soca_console::{handle, parse_request, ConsoleModel, Request, Response, Session};
use soca_contracts::{
    Candidate, ModelBackend, ModelBudget, ModelOutput, ModelProposal, ModelSelfReport,
    ModelVersion, SubjectId, TokenUsage, WallClock, MODEL_OUTPUT_SCHEMA_VERSION,
};
use soca_core::{ActionBroker, SimulatedOs, Subject};
use soca_core_actors::{DesktopAndFilesCluster, Precondition};
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const TOKEN: &str = "test-token-0123456789";
const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn session() -> Session {
    Session::with_token(TOKEN)
}

/// 一个只会复述它收到的证据的应答源。与 `main` 里那个是同一形状。
fn stub() -> DeterministicTransport {
    DeterministicTransport::from_fn(|request| {
        Ok(ModelOutput {
            schema_version: MODEL_OUTPUT_SCHEMA_VERSION,
            model_version: ModelVersion::new("sha256:stub").expect("固定模型版本"),
            proposals: request
                .context
                .evidence
                .iter()
                .map(|slice| ModelProposal {
                    candidate: Candidate::Claim {
                        statement: format!("{} 的值是 {}", slice.subject_ref, slice.observed_value),
                        evidence_refs: vec![slice.evidence_ref.clone()],
                    },
                    self_report: ModelSelfReport {
                        reported_value: 0.5,
                        model_version: ModelVersion::new("sha256:stub").expect("固定模型版本"),
                        rationale: "桩".to_string(),
                    },
                    rationale: "复述".to_string(),
                })
                .collect(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            claims_finished: false,
        })
    })
}

fn subject() -> Subject {
    let mut broker = ActionBroker::new(SimulatedOs::new());
    broker.os_mut().seed(WATCHED, "sha256:initial");
    let cluster = DesktopAndFilesCluster::new(
        WATCHED,
        vec![Precondition::new("目录已授权", "cap:read-selected-folder")],
    )
    .expect("装配能力簇");

    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        broker,
        cluster,
        SubjectId::new("user:local").expect("固定主体"),
        soca_contracts::BootId::generate(),
        Box::new(stub()),
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new("sha256:stub").expect("固定模型版本"),
    )
    .expect("装配主体")
}

fn request(method: &str, path: &str, body: &str) -> Request {
    let mut headers = BTreeMap::new();
    headers.insert("host".to_string(), "127.0.0.1:4319".to_string());
    headers.insert("x-soca-token".to_string(), TOKEN.to_string());
    headers.insert("content-length".to_string(), body.len().to_string());
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: BTreeMap::new(),
        headers,
        body: body.to_string(),
    }
}

fn json(response: &Response) -> Value {
    serde_json::from_str(&response.body).expect("响应体是 JSON")
}

/// 构造请求体。
///
/// 用手拼字符串写 Windows 路径会踩到一个安静的坑：`"file:D:\资料\x.md"` 里 `\资` 不是合法
/// JSON 转义，于是请求以 400 结束，而失败原因看起来像"接口不对"。让序列化器负责转义。
fn body(value: Value) -> String {
    serde_json::to_string(&value).expect("可序列化")
}

fn call(subject: &mut Subject, method: &str, path: &str, body: &str) -> Response {
    let mut model = ConsoleModel::new();
    handle(
        subject,
        &mut model,
        &session(),
        &request(method, path, body),
        at(0),
    )
}

/// 用一条手工构造的请求走一次路由。给认证与来源检查那几条用。
fn call_request(subject: &mut Subject, request: &Request) -> Response {
    let mut model = ConsoleModel::new();
    handle(subject, &mut model, &session(), request, at(0))
}

/// 带持久模型配置的调用。换端点这类操作有状态，不能每次用一个新配置。
fn call_with(
    subject: &mut Subject,
    model: &mut ConsoleModel,
    method: &str,
    path: &str,
    body: &str,
) -> Response {
    handle(
        subject,
        model,
        &session(),
        &request(method, path, body),
        at(0),
    )
}

// ---------------------------------------------------------------------------
// §8：认证与来源检查
// ---------------------------------------------------------------------------

#[test]
fn the_page_is_served_with_the_session_token_injected() {
    let mut subject = subject();
    let response = call(&mut subject, "GET", "/", "");
    assert_eq!(response.status, 200);
    assert!(response.body.contains(TOKEN), "令牌要注入页面");
    assert!(
        response.body.contains("SoCA 控制台"),
        "页面必须能自己打开，否则用户没法拿到令牌"
    );
    assert!(
        !response.body.contains("__SOCA_TOKEN__"),
        "占位符必须被替换掉"
    );
    // 模型配置就在同一个页面上，不需要另开一个界面。
    assert!(response.body.contains("id=\"model-key\""));
    assert!(response.body.contains("id=\"model-allow-personal\""));
    assert!(response.body.contains("api.deepseek.com"));
}

// ---------------------------------------------------------------------------
// 模型端点配置
// ---------------------------------------------------------------------------

#[test]
fn the_model_endpoint_reports_the_offline_stub_by_default() {
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    let response = call_with(&mut subject, &mut model, "GET", "/api/model", "");
    assert_eq!(response.status, 200);
    let payload = json(&response);
    assert_eq!(payload["configured"], false);
    assert_eq!(payload["mode"], "offline_stub");
    assert!(
        payload["note"].as_str().expect("有说明").contains("桩"),
        "默认状态必须说清当前没有接模型"
    );
}

#[test]
fn configuring_deepseek_switches_the_backend_and_never_echoes_the_key() {
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    let secret = "sk-abcdefghijklmnop";
    let response = call_with(
        &mut subject,
        &mut model,
        "POST",
        "/api/model",
        &body(json!({"api_key": secret})),
    );
    assert_eq!(response.status, 200);

    let payload = json(&response);
    assert_eq!(payload["configured"], true);
    assert_eq!(payload["base_url"], "https://api.deepseek.com");
    assert_eq!(payload["model"], "deepseek-chat");
    assert_eq!(
        payload["endpoint"],
        "https://api.deepseek.com/chat/completions"
    );
    assert_eq!(payload["api_key_fingerprint"], "sk-****mnop");
    assert_eq!(payload["allow_private_egress"], false);

    // 密钥绝不能出现在任何返回里。界面能回答"是哪把钥匙"，而这个回答不需要交出钥匙。
    assert!(
        !response.body.contains(secret),
        "响应体不得包含密钥，实际：{}",
        response.body
    );

    let state = json(&call_with(&mut subject, &mut model, "GET", "/api/state", ""));
    assert_eq!(state["backend"], "remote");
    assert_eq!(
        state["egress_policy"], "strict",
        "接上远端本身不放开个人数据"
    );
}

#[test]
fn an_http_endpoint_that_is_not_loopback_is_refused() {
    // 把 Authorization 头发往非 loopback 的明文 HTTP，等于把密钥交给路径上的每一跳。
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    let response = call_with(
        &mut subject,
        &mut model,
        "POST",
        "/api/model",
        &body(json!({"base_url": "http://api.deepseek.com", "api_key": "sk-test"})),
    );
    assert_eq!(response.status, 400);
    assert_eq!(json(&response)["error"], "invalid_credentials");
    assert!(!model.is_configured(), "被拒的配置不留下半个状态");
}

#[test]
fn a_loopback_http_endpoint_is_accepted() {
    // 本机推理服务（llama.cpp / vLLM）用明文 HTTP 是合理的：它不出机器。
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    let response = call_with(
        &mut subject,
        &mut model,
        "POST",
        "/api/model",
        &body(json!({
            "base_url": "http://127.0.0.1:8080/v1",
            "model": "local-model",
            "api_key": "not-needed",
        })),
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        json(&response)["endpoint"],
        "http://127.0.0.1:8080/v1/chat/completions"
    );
}

#[test]
fn an_empty_api_key_is_refused() {
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    let response = call_with(
        &mut subject,
        &mut model,
        "POST",
        "/api/model",
        &body(json!({"api_key": "   "})),
    );
    assert_eq!(response.status, 400);
    assert_eq!(json(&response)["error"], "invalid_credentials");
}

#[test]
fn granting_personal_egress_is_visible_in_both_the_summary_and_the_state() {
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    let response = call_with(
        &mut subject,
        &mut model,
        "POST",
        "/api/model",
        &body(json!({"api_key": "sk-test", "allow_private_egress": true})),
    );
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["allow_private_egress"], true);

    let state = json(&call_with(&mut subject, &mut model, "GET", "/api/state", ""));
    assert_eq!(
        state["egress_policy"], "allow_personal",
        "放开之后界面必须显示出来，否则用户不知道自己现在处于什么状态"
    );
}

#[test]
fn disconnecting_returns_to_the_offline_stub() {
    let mut subject = subject();
    let mut model = ConsoleModel::new();
    call_with(
        &mut subject,
        &mut model,
        "POST",
        "/api/model",
        &body(json!({"api_key": "sk-test", "allow_private_egress": true})),
    );
    assert!(model.is_configured());

    let response = call_with(&mut subject, &mut model, "POST", "/api/model/reset", "{}");
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["configured"], false);

    let state = json(&call_with(&mut subject, &mut model, "GET", "/api/state", ""));
    assert_eq!(state["backend"], "cpu");
    assert_eq!(
        state["egress_policy"], "strict",
        "断开之后出站策略要回到默认，不能把上一次的批准留着"
    );
}

#[test]
fn the_model_endpoint_requires_the_same_authentication_as_everything_else() {
    let mut subject = subject();
    let mut bare = request("GET", "/api/model", "");
    bare.headers.remove("x-soca-token");
    assert_eq!(call_request(&mut subject, &bare).status, 401);
}

#[test]
fn an_api_call_without_a_token_is_refused() {
    // §8："不把'本机地址'当作无需授权"。监听在 127.0.0.1 上不是放行的理由。
    let mut subject = subject();
    let mut bare = request("GET", "/api/state", "");
    bare.headers.remove("x-soca-token");
    assert_eq!(call_request(&mut subject, &bare).status, 401);
}

#[test]
fn an_api_call_with_a_wrong_token_is_refused() {
    let mut subject = subject();
    let mut wrong = request("GET", "/api/state", "");
    wrong
        .headers
        .insert("x-soca-token".to_string(), "wrong".to_string());
    assert_eq!(call_request(&mut subject, &wrong).status, 401);

    // 前缀匹配也要被拒：token 比对上的任何宽容都是漏洞。
    let mut prefix = request("GET", "/api/state", "");
    prefix
        .headers
        .insert("x-soca-token".to_string(), TOKEN[..8].to_string());
    assert_eq!(call_request(&mut subject, &prefix).status, 401);
}

#[test]
fn a_request_from_a_non_loopback_host_is_refused() {
    // 挡住的是**浏览器**被诱导去访问一个解析到本机的外部域名：那种情况下浏览器发出的
    // Host 是那个外部域名，于是被拒。
    let mut subject = subject();
    let mut foreign = request("GET", "/api/state", "");
    foreign
        .headers
        .insert("host".to_string(), "evil.example.com".to_string());
    assert_eq!(call_request(&mut subject, &foreign).status, 403);

    // 没有 Host 头：HTTP/1.1 要求必须有，缺失即不可信。
    let mut hostless = request("GET", "/api/state", "");
    hostless.headers.remove("host");
    assert_eq!(call_request(&mut subject, &hostless).status, 403);
}

#[test]
fn a_loopback_host_with_a_port_is_accepted() {
    for host in ["localhost:4319", "127.0.0.1", "[::1]:4319"] {
        let mut request = request("GET", "/api/state", "");
        request.headers.insert("host".to_string(), host.to_string());
        assert!(
            Session::host_is_loopback(&request),
            "{host} 应当被认作 loopback"
        );
    }
}

// ---------------------------------------------------------------------------
// 委托
// ---------------------------------------------------------------------------

#[test]
fn a_chat_message_becomes_a_goal() {
    let mut subject = subject();
    let response = call(
        &mut subject,
        "POST",
        "/api/chat",
        r#"{"message":"为已授权目录生成摘要"}"#,
    );
    assert_eq!(response.status, 200);
    let payload = json(&response);
    assert_eq!(payload["state"], "active");
    assert!(payload["goal_id"].as_str().expect("有标识").starts_with("goal:"));

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["goals"].as_array().expect("数组").len(), 1);
    assert_eq!(state["goals"][0]["state"], "active");
}

#[test]
fn an_empty_message_is_refused_before_a_goal_is_created() {
    let mut subject = subject();
    assert_eq!(
        call(&mut subject, "POST", "/api/chat", r#"{"message":"   "}"#).status,
        400
    );
    assert_eq!(
        call(&mut subject, "POST", "/api/chat", r#"{"message":123}"#).status,
        400
    );
    assert_eq!(
        call(&mut subject, "POST", "/api/chat", "not json").status,
        400
    );
    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(
        state["goals"].as_array().expect("数组").len(),
        0,
        "被拒的委托不留下半个目标"
    );
}

// ---------------------------------------------------------------------------
// 观测
// ---------------------------------------------------------------------------

#[test]
fn observing_reports_the_value_and_an_evidence_ref() {
    let mut subject = subject();
    let response = call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );
    assert_eq!(response.status, 200);
    let payload = json(&response);
    assert_eq!(payload["subject"], WATCHED);
    // 观测到的是**内容的摘要**，不是内容本身。界面显示版本号而不是正文，这也是
    // §12.3 的意思：索引引用源版本，不复制源内容。
    assert_eq!(
        payload["value"],
        soca_contracts::Sha256Hex::of_bytes(b"sha256:initial").to_string()
    );
    assert!(
        payload["evidence_ref"]
            .as_str()
            .expect("有引用")
            .starts_with("obs:")
    );

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["observed_evidence"], 1);
    assert_eq!(state["workspace_topics"], 1);
}

#[test]
fn an_invalid_data_class_is_refused() {
    let mut subject = subject();
    let response = call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "top-secret"})),
    );
    assert_eq!(response.status, 400);
}

// ---------------------------------------------------------------------------
// 咨询
// ---------------------------------------------------------------------------

#[test]
fn consulting_returns_candidates_along_with_the_context_that_was_given() {
    let mut subject = subject();
    let goal_id = json(&call(
        &mut subject,
        "POST",
        "/api/chat",
        r#"{"message":"核对摘要文件"}"#,
    ))["goal_id"]
        .as_str()
        .expect("有标识")
        .to_string();

    call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );

    let response = call(
        &mut subject,
        "POST",
        "/api/consult",
        &body(json!({"goal_id": goal_id})),
    );
    assert_eq!(response.status, 200);
    let payload = json(&response);

    assert_eq!(payload["evidence_given"], 1, "上下文里带着那条观测");
    assert_eq!(payload["attempts"], 1);
    assert_eq!(payload["goal"], "核对摘要文件");
    let proposals = payload["proposals"].as_array().expect("数组");
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0]["kind"], "claim");
    assert!(
        proposals[0]["cited_evidence"]
            .as_array()
            .expect("数组")
            .len()
            == 1,
        "提案必须能回引它引用的证据"
    );
    assert_eq!(
        proposals[0]["self_report"], 0.5,
        "自评数值原样带出，并附上一句它是什么"
    );

    assert_eq!(
        payload["allowed_candidates"]
            .as_array()
            .expect("数组")
            .len(),
        2,
        "本版是只读 schema：允许结论与申请观测，不允许动作"
    );

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["model_calls"], 1);
    assert_eq!(state["actions"], 0, "咨询不产生动作");
}

#[test]
fn consulting_a_missing_goal_is_a_404() {
    let mut subject = subject();
    let response = call(
        &mut subject,
        "POST",
        "/api/consult",
        r#"{"goal_id":"goal:never-delegated"}"#,
    );
    assert_eq!(response.status, 404);
}

#[test]
fn a_malformed_goal_id_is_refused_before_anything_happens() {
    let mut subject = subject();
    let response = call(
        &mut subject,
        "POST",
        "/api/consult",
        r#"{"goal_id":"task:1"}"#,
    );
    assert_eq!(response.status, 400);
}

// ---------------------------------------------------------------------------
// 放弃与未知路径
// ---------------------------------------------------------------------------

#[test]
fn abandoning_a_goal_closes_it() {
    let mut subject = subject();
    let goal_id = json(&call(
        &mut subject,
        "POST",
        "/api/chat",
        r#"{"message":"稍后再做"}"#,
    ))["goal_id"]
        .as_str()
        .expect("有标识")
        .to_string();

    let response = call(
        &mut subject,
        "POST",
        "/api/abandon",
        &format!(r#"{{"goal_id":"{goal_id}"}}"#),
    );
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["abandoned"], 1);

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["goals"][0]["state"], "abandoned");
}

#[test]
fn unknown_paths_and_methods_are_refused() {
    let mut subject = subject();
    assert_eq!(call(&mut subject, "GET", "/api/nope", "").status, 404);
    assert_eq!(call(&mut subject, "POST", "/api/nope", "{}").status, 404);
    assert_eq!(call(&mut subject, "DELETE", "/api/state", "").status, 405);
}

// ---------------------------------------------------------------------------
// HTTP 解析
// ---------------------------------------------------------------------------

#[test]
fn the_request_parser_splits_line_headers_and_body() {
    let raw = b"POST /api/chat?token=abc HTTP/1.1\r\nHost: 127.0.0.1:4319\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
    let request = parse_request(raw).expect("可解析");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/chat");
    assert_eq!(request.param("token"), Some("abc"));
    assert_eq!(request.header("HOST"), Some("127.0.0.1:4319"));
    assert_eq!(request.body, "{\"a\":1}");
}

#[test]
fn the_request_parser_refuses_a_body_that_declares_too_much() {
    // §11.4："先检查长度再分配，超限关闭该请求。"按声明去分配，等于让对端决定我们分配多少。
    let raw = format!(
        "POST /api/chat HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
        soca_console::MAX_BODY_BYTES + 1
    );
    assert!(parse_request(raw.as_bytes()).is_err());
}

#[test]
fn a_body_that_is_not_utf8_is_refused_rather_than_mangled() {
    // 有损解码会把中文改写成 `????`，用户看到的是一个"看起来生效了、其实内容已经变了"的
    // 目标。静默改写用户内容比拒绝它更糟：前者不可察觉，后者至少能被修。
    let mut raw =
        b"POST /api/chat HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\n".to_vec();
    raw.extend_from_slice(&[0xff, 0xfe, 0x41, 0x42]);
    assert_eq!(
        parse_request(&raw),
        Err(soca_console::HttpError::BodyNotUtf8)
    );
}

#[test]
fn the_request_parser_percent_decodes_query_values() {
    let raw = b"GET /?token=a%2Bb&message=%E4%BD%A0%E5%A5%BD HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    let request = parse_request(raw).expect("可解析");
    assert_eq!(request.param("token"), Some("a+b"));
    assert_eq!(request.param("message"), Some("你好"));
}

#[test]
fn a_response_carries_a_content_length_and_says_it_does_not_store() {
    let response = Response::text(401, "no");
    let raw = String::from_utf8(response.to_bytes()).expect("可转字符串");
    assert!(raw.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    assert!(raw.contains("Content-Length: 2\r\n"));
    assert!(raw.contains("Cache-Control: no-store"));
    assert!(raw.ends_with("\r\n\r\nno"));
}
