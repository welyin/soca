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

#[test]
fn the_console_says_why_each_candidate_was_rejected_and_when_it_could_come_back() {
    // §13.1："最**多 8 个待核验正式候选**。……**拒绝有原因和可重试条件**。"
    //
    // 两者都要能从接口上读到。只给一句散文的话，界面只能把它原样贴出来，
    // 而调度器连"该不该过一会儿再试"都判断不了——那正是这一项要补的东西。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );

    // A2 的门槛是 3 条，手上只有 1 条——所以这一轮应当有候选被拒，而且理由是可操作的。
    let payload = json(&call(
        &mut subject,
        "POST",
        "/api/select",
        &body(json!({"risk": "a2"})),
    ));

    let rejections = payload["rejections"].as_array().expect("要有拒绝台账");
    assert!(
        !rejections.is_empty(),
        "一条证据过不了 A2 的门槛，应当留下拒绝记录：{payload}"
    );

    let below_bar = rejections
        .iter()
        .find(|item| item["retry_when"]["kind"] == "more_evidence")
        .expect("门槛不达标要报成 more_evidence");
    assert_eq!(
        below_bar["retry_when"]["short_by"], 2,
        "3 条门槛、1 条在手，还差 2 条——数字要是可操作的，不是'证据不足'四个字"
    );
    assert!(
        below_bar["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("门槛")),
        "理由要说清是哪一条判定：{below_bar}"
    );

    // 而**每一条候选都有去处**：要么被选中，要么有一条记录。没有第三种。
    let selected_index = if payload["outcome"]["kind"] == "selected" {
        Some(payload["outcome"]["index"].clone())
    } else {
        None
    };
    for candidate in payload["candidates"].as_array().expect("候选列表") {
        let is_selected = selected_index.as_ref() == Some(&candidate["index"]);
        assert!(
            is_selected || candidate["rejected"] == true,
            "候选 {} 既没被选中也没有拒绝记录：{candidate}",
            candidate["index"]
        );
    }
}

// ---------------------------------------------------------------------------
// 策略改进的准入（§13.2）
// ---------------------------------------------------------------------------

#[test]
fn the_console_separates_suggesting_a_strategy_from_enabling_it() {
    // §13.2："系统可**建议**新的单元或拓扑，但……候选策略**必须经过准入**。"
    //
    // 两个接口，两步。合成一步（"提了就等于启用了"）是那句话最容易落空的地方——
    // 而落空之后看不出来：账上有一条像是经过了准入的记录。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);
    let state = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))));
    let version_before = state["strategy_version"].clone();
    assert!(
        version_before.as_str().is_some_and(|v| !v.is_empty()),
        "状态里要能看到当前策略版本：{state}"
    );

    // 攒一条错案：把记下的那条结论纠正掉。
    let corrected = json(&call(
        &mut subject,
        "POST",
        "/api/correct",
        &body(json!({"memory_id": memory_id, "note": "记错了"})),
    ));
    assert_eq!(corrected["retracted"].as_array().expect("数组").len(), 1);

    // 一、建议。**它什么都不改。**
    let suggestion = json(&call(
        &mut subject,
        "POST",
        "/api/learning",
        &body(json!({"risk": "a1"})),
    ));
    assert_eq!(suggestion["outcome"], "suggested", "{suggestion}");
    assert_eq!(
        suggestion["holdout"]["known_wrong"], 1,
        "保留任务集里要有那条错案：{suggestion}"
    );

    let after_suggesting = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))));
    assert_eq!(
        after_suggesting["strategy_version"], version_before,
        "提建议不该动任何东西"
    );

    // 二、启用。候选要**原样带回来**，而不是让后端重新提一遍。
    let candidate = suggestion["candidate"].clone();
    let applied = json(&call(
        &mut subject,
        "POST",
        "/api/learning/apply",
        &body(json!({
            "risk": "a1",
            "version": candidate["version"],
            "policy": candidate["policy"],
            "based_on": candidate["based_on"],
            "rationale": candidate["rationale"],
        })),
    ));
    assert_eq!(applied["admitted"], true, "{applied}");

    let after = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))));
    assert_ne!(after["strategy_version"], version_before, "启用了就该换版本号");
    assert_eq!(applied["strategy_version"], after["strategy_version"]);
    assert!(
        after["strategy"]["base_evidence"].as_u64().expect("数字") >= 2,
        "而门槛真的抬上去了：{after}"
    );
}

#[test]
fn the_console_refuses_a_strategy_that_loosens_anything() {
    // §13.2："系统……没有自行……**提高权限**的能力。" 反过来说，也没有自行放宽的能力。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);
    call(
        &mut subject,
        "POST",
        "/api/correct",
        &body(json!({"memory_id": memory_id, "note": "记错了"})),
    );

    // 手搓一个"把门槛降到 0"的候选。版本号按内容算——算错了闸会先以另一条理由挡下来，
    // 而那条理由不是这条测试要看的。
    let loosened = soca_contracts::SelectionPolicy {
        base_evidence: 0,
        high_risk_extra: 0,
        high_risk_from: soca_contracts::ActionLevel::A4,
        max_checks: 0,
    };
    let version =
        soca_contracts::strategy_version_of(&loosened).expect("派生版本");

    let applied = json(&call(
        &mut subject,
        "POST",
        "/api/learning/apply",
        &body(json!({
            "risk": "a1",
            "version": version.as_str(),
            "policy": loosened,
            "based_on": [memory_id],
            "rationale": "试试能不能放宽",
        })),
    ));
    assert_eq!(applied["admitted"], false, "{applied}");
    assert_eq!(applied["admission"]["retry_when"]["kind"], "never");
    assert_eq!(
        json(&call(&mut subject, "GET", "/api/state", &body(json!({}))))["strategy"]["max_checks"],
        8,
        "策略一点没变"
    );
}

// ---------------------------------------------------------------------------
// 放弃与未知路径
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 检验与选择
// ---------------------------------------------------------------------------

#[test]
fn the_ladder_runs_the_verifiers_and_reports_what_it_checked() {
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );

    let response = call(
        &mut subject,
        "POST",
        "/api/select",
        &body(json!({"risk": "a1"})),
    );
    assert_eq!(response.status, 200);
    let payload = json(&response);

    assert_eq!(payload["risk"], "A1");
    assert_eq!(payload["required_evidence"], 1, "A1 是低风险档");
    assert_eq!(
        payload["outcome"]["kind"], "selected",
        "有一条关于版本的结论，且证据就在台账里：{payload}"
    );

    // 检验确实跑了，而且报的是它查到的东西。
    let outcomes = payload["reviews"][0]["outcomes"]
        .as_array()
        .expect("有档案");
    // 线上格式一律 snake_case，判定值与 `kind` 用同一套写法。这是接口契约的一部分，
    // 所以在这里钉住——换一种大小写会让所有读这个接口的代码静静地匹配不上。
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome["kind"] == "tool" && outcome["verdict"] == "supported"),
        "结论与自己引用的证据一致：{outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome["kind"] == "independent_source"
                && outcome["verdict"] == "inconclusive"),
        "只有一个观测者，来源核对应当说无法判定：{outcomes:?}"
    );
    assert!(
        !outcomes
            .iter()
            .any(|outcome| outcome["kind"] == "counter_example"),
        "低风险档不搜反例"
    );

    assert!(
        payload["candidates"].as_array().expect("数组").len() >= 2,
        "簇同时提出了结论与观测请求"
    );
}

#[test]
fn a_high_risk_run_searches_for_counter_examples() {
    // 门槛与反例搜索都由同一个风险等级驱动。分头设置会让"高风险"只在一半的地方生效。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );

    let payload = json(&call(
        &mut subject,
        "POST",
        "/api/select",
        &body(json!({"risk": "a3"})),
    ));
    assert_eq!(payload["required_evidence"], 3, "高风险档门槛上升");
    let outcomes = payload["reviews"][0]["outcomes"].as_array().expect("有档案");
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome["kind"] == "counter_example"),
        "高风险档要主动找反方观点：{outcomes:?}"
    );
}

#[test]
fn the_loop_advances_the_subject_over_several_rounds() {
    // 一次请求跑若干轮 §6 的闭环，而且要真的改变状态——不是把同一轮重复报几遍。
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
    assert!(goal_id.starts_with("goal:"));

    let response = call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 4})),
    );
    assert_eq!(response.status, 200);
    let payload = json(&response);
    let rounds = payload["rounds"].as_array().expect("数组");
    assert!(!rounds.is_empty());

    // 第一轮没有证据，应当去观测。
    assert_eq!(rounds[0]["outcome"]["kind"], "advanced");
    assert_eq!(rounds[0]["outcome"]["step"]["kind"], "observation");
    assert_eq!(rounds[0]["activations"], 1);

    // 观测补上之后，后续某一轮应当把结论写进记忆。
    assert!(
        rounds.iter().any(|round| round["outcome"]["step"]["kind"] == "claim"),
        "跑了几轮之后应当得出一条结论：{rounds:?}"
    );

    let state = &payload["state"];
    assert!(
        state["observed_evidence"].as_u64().expect("数字") >= 1,
        "闭环补到的证据要真的进账"
    );
    assert!(
        state["ledger_records"].as_u64().expect("数字") >= 1,
        "证据要进台账"
    );
    assert_eq!(state["model_calls"], 0, "这一轮闭环没有调用模型");
}

#[test]
fn the_loop_stops_early_when_there_is_nothing_left_to_do() {
    // 没有目标时闭环立刻报结束，而不是空转满请求的轮数——空转会把审计账塞满一样的记录。
    let mut subject = subject();
    let payload = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 8})),
    ));
    let rounds = payload["rounds"].as_array().expect("数组");
    assert_eq!(rounds.len(), 1, "一轮就该停：{rounds:?}");
    assert_eq!(rounds[0]["outcome"]["kind"], "finished");
}

// ---------------------------------------------------------------------------
// 授权与撤回（§12.1）
// ---------------------------------------------------------------------------

#[test]
fn the_console_walks_a_revocation_from_grant_to_invalidation() {
    // §12.1 的"撤回立即生效"有两半，而这个测试把两半一起走完：新的观测停住，
    // 已有的记忆失效——再重新授予，一切恢复。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/chat",
        &body(json!({"message": "核对摘要文件"})),
    );
    call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );
    let looped = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 6})),
    ));
    assert!(
        looped["state"]["memory_entries"].as_u64().expect("数字") >= 1,
        "先攒下一条结论：{looped}"
    );

    let revoked = json(&call(
        &mut subject,
        "POST",
        "/api/revoke",
        &body(json!({"capability": "cap:read-selected-folder"})),
    ));
    assert_eq!(revoked["was_granted"], true);
    assert!(
        revoked["memories_invalidated"].as_u64().expect("数字") >= 1,
        "已有的记忆要失效：{revoked}"
    );
    assert!(
        revoked["granted"].as_array().expect("数组").is_empty(),
        "撤回之后没有生效的授权：{revoked}"
    );

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["memory_entries"], 0, "撤回之后它们不再被检索到");

    // 新的观测停住。403 而不是 500——"授权被收回了"不是"服务器坏了"。
    let refused = call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );
    assert_eq!(refused.status, 403, "{}", refused.body);

    // 授予时必须给出范围（§12.1 的范围限定授权）。
    let missing_scope = call(
        &mut subject,
        "POST",
        "/api/grant",
        &body(json!({"capability": "cap:read-selected-folder"})),
    );
    assert_eq!(missing_scope.status, 400, "范围是必填的：{}", missing_scope.body);

    let granted = json(&call(
        &mut subject,
        "POST",
        "/api/grant",
        &body(json!({
            "capability": "cap:read-selected-folder",
            "prefix": "file:D:\\资料\\摘要"
        })),
    ));
    assert_eq!(granted["was_new"], true);
    assert_eq!(
        call(
            &mut subject,
            "POST",
            "/api/observe",
            &body(json!({"subject": WATCHED, "data_class": "personal"})),
        )
        .status,
        200,
        "重新授予之后恢复"
    );
}

#[test]
fn revoking_an_ungranted_capability_is_idempotent_not_an_error() {
    let mut subject = subject();
    let first = json(&call(
        &mut subject,
        "POST",
        "/api/revoke",
        &body(json!({"capability": "cap:never-given"})),
    ));
    assert_eq!(first["was_granted"], false, "从来没给过，撤回是幂等的");

    let second = json(&call(
        &mut subject,
        "POST",
        "/api/revoke",
        &body(json!({"capability": "cap:never-given"})),
    ));
    assert_eq!(second["was_granted"], false);
    assert_eq!(second["memories_invalidated"], 0);
}

// ---------------------------------------------------------------------------
// 保留期与删除（§12.3）
// ---------------------------------------------------------------------------

/// 让环路记下一条结论，并把它写进记忆。返回那条记忆的标识。
fn record_a_memory(subject: &mut Subject) -> String {
    call(
        subject,
        "POST",
        "/api/chat",
        &body(json!({"message": "核对摘要文件"})),
    );
    call(
        subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );
    let report = json(&call(
        subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 6})),
    ));
    report["rounds"]
        .as_array()
        .expect("数组")
        .iter()
        .find_map(|round| round["outcome"]["step"]["memory_id"].as_str())
        .expect("环路应当记下一条结论")
        .to_string()
}

#[test]
fn the_console_reports_hiding_and_purging_as_different_states() {
    // §12.3 的"先写 tombstone 使查询立即不可见，**再异步清理**，并给用户完成状态"。
    // 界面必须能分开回答"看不见了"与"清干净了"——合成一个勾，用户就无从知道内容是不是真的走了。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);

    let forgotten = json(&call(
        &mut subject,
        "POST",
        "/api/forget",
        &body(json!({"memory_id": memory_id})),
    ));
    assert_eq!(forgotten["tombstoned"], 1);
    assert_eq!(forgotten["awaiting_purge"], 1);

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["memory_entries"], 0, "删完立刻不可检索");
    assert_eq!(state["memories_awaiting_purge"], 1, "但内容还在原处等着");

    // 重复删除是幂等的成功。用户看到失败提示会以为没删掉，然后点第二次——
    // 而此时把它报成错误，只会把他困在一个"删不掉"的界面上。
    let again = json(&call(
        &mut subject,
        "POST",
        "/api/forget",
        &body(json!({"memory_id": memory_id})),
    ));
    assert_eq!(again["tombstoned"], 0, "不该再报一次删除");
    assert_eq!(again["awaiting_purge"], 1);

    let purged = json(&call(&mut subject, "POST", "/api/retention", &body(json!({}))));
    assert_eq!(purged["purged"], 1);
    assert_eq!(purged["awaiting_purge"], 0);

    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["memories_awaiting_purge"], 0);
}

#[test]
fn forgetting_something_already_purged_says_not_found_rather_than_pretending() {
    // 清理之后那条记录连审计入口也读不到了。这时再删只能是"找不到"——系统无从判断它
    // 是被删过还是从来不存在，而把这两种情况都说成"已删除"，就是在替一个它不掌握的事实作证。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);
    call(
        &mut subject,
        "POST",
        "/api/forget",
        &body(json!({"memory_id": memory_id.clone()})),
    );
    call(&mut subject, "POST", "/api/retention", &body(json!({})));

    let response = call(
        &mut subject,
        "POST",
        "/api/forget",
        &body(json!({"memory_id": memory_id})),
    );
    assert_eq!(response.status, 404);
}

#[test]
fn a_retention_run_on_a_clean_subject_reports_nothing_to_do() {
    let mut subject = subject();
    let report = json(&call(&mut subject, "POST", "/api/retention", &body(json!({}))));
    assert_eq!(report["tombstoned"], 0);
    assert_eq!(report["purged"], 0);
    assert_eq!(report["awaiting_purge"], 0);
    assert_eq!(report["audit_pruned"], 0);
}

// ---------------------------------------------------------------------------
// 执行与审批
// ---------------------------------------------------------------------------

const TARGET: &str = "file:D:\\资料\\摘要\\out.md";

/// 一轮报告里出现过的推进步骤。
fn step_kinds(payload: &Value) -> Vec<String> {
    payload["rounds"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|round| round["outcome"]["step"]["kind"].as_str())
        .map(ToString::to_string)
        .collect()
}

#[test]
fn the_console_walks_a_write_from_delegation_to_verified_execution() {
    // 端到端把 §12.1／§12.2 那条路走一遍：委托 A2 范围 → 投递写入 → 停下等审批 →
    // 批准 → 恢复 → 真的执行并核对。
    let mut subject = subject();

    let goal = json(&call(
        &mut subject,
        "POST",
        "/api/delegate_write",
        &body(json!({"message": "写摘要"})),
    ));
    assert_eq!(goal["max_action_level"], "A2");

    let written = json(&call(
        &mut subject,
        "POST",
        "/api/write",
        &body(json!({"subject_ref": TARGET, "content": "摘要内容"})),
    ));
    assert_eq!(written["pending_actions"], 1);

    // 没有批准：动作应当停下来等——既不是被执行，也不是被拒绝。
    let before = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a2", "rounds": 6})),
    ));
    let kinds = step_kinds(&before);
    assert!(
        kinds.contains(&"needs_approval".to_string()),
        "没有批准时应当停下来等：{kinds:?}"
    );
    assert!(!kinds.contains(&"action".to_string()), "没有批准时不该执行");
    assert_eq!(before["state"]["pending_actions"], 1, "动作还在队列里等着");

    // 批准 → 恢复 → 再跑。
    call(
        &mut subject,
        "POST",
        "/api/approve",
        &body(json!({"level": "a2", "max_uses": 1})),
    );
    let resumed = call(&mut subject, "POST", "/api/resume", &body(json!({})));
    assert_eq!(resumed.status, 200, "{}", resumed.body);

    let after = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a2", "rounds": 6})),
    ));
    let kinds = step_kinds(&after);
    assert!(
        kinds.contains(&"action".to_string()),
        "批准之后应当真的执行：{kinds:?}"
    );
    assert_eq!(after["state"]["pending_actions"], 0, "执行完的动作要出队");
    assert_eq!(
        after["state"]["usable_approvals"], 0,
        "一次性批准用完就不再可用"
    );
}

#[test]
fn resuming_without_a_waiting_goal_is_a_conflict_not_a_silent_success() {
    let mut subject = subject();
    let response = call(&mut subject, "POST", "/api/resume", &body(json!({})));
    assert_eq!(response.status, 409);
}

#[test]
fn approving_does_not_by_itself_execute_anything() {
    // 批准是一个**输入**，不是一个动作。它只让后续的闭环有可能推进；把它做成"批准即执行"，
    // 就等于让审批界面同时充当执行代理——而 §12.2 要求副作用只有一个出口。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/approve",
        &body(json!({"level": "a2", "max_uses": 1})),
    );
    let state = json(&call(&mut subject, "GET", "/api/state", ""));
    assert_eq!(state["actions"], 0, "只是批准了一次，账上不该有动作");
}

#[test]
fn the_console_records_a_correction_as_an_event_and_takes_the_memory_out() {
    // §14："用户说'记错了'**生成纠错事件**并失效相关派生记忆，**不只在下一条回复中口头道歉**。"
    //
    // 两半都要落地。这条测试一半看事件账（前半句），一半看状态（后半句）。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);

    let corrected = json(&call(
        &mut subject,
        "POST",
        "/api/correct",
        &body(json!({"memory_id": memory_id, "note": "那个日期我看错了"})),
    ));
    assert_eq!(
        corrected["retracted"].as_array().expect("数组").len(),
        1,
        "指名的那一条要撤掉：{corrected}"
    );
    assert_eq!(corrected["derived"], 0, "指名的就是它自己");
    assert!(
        corrected["event_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "纠错要留下一条事件：{corrected}"
    );

    // 后半句：状态真的变了。
    let state = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))));
    assert_eq!(state["memory_entries"], 0, "撤了就不该还在：{state}");
}

#[test]
fn correcting_without_naming_anything_is_refused() {
    // 两种指名方式对应**能证明的范围不一样**，所以不合并成一个字段，也不给默认值。
    let mut subject = subject();
    let refused = call(
        &mut subject,
        "POST",
        "/api/correct",
        &body(json!({"note": "你错了"})),
    );
    assert_eq!(refused.status, 400);
    assert!(
        refused.body.contains("memory_id") && refused.body.contains("event_id"),
        "要说清两种填法各是什么意思：{}",
        refused.body
    );
}

#[test]
fn correcting_by_a_source_that_nothing_was_derived_from_retracts_nothing() {
    // 按原始事件纠错撤的是**由它派生的**那些。少了这条对照，"撤掉派生的"与
    // "把所有记忆都撤掉"看起来是一样的。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);

    let untouched = json(&call(
        &mut subject,
        "POST",
        "/api/correct",
        &body(json!({
            "event_id": "obs:00000000-0000-4000-8000-0000000000ff",
            "note": "这条观测是错的"
        })),
    ));
    assert_eq!(untouched["derived"], 0, "没有结论派生自它：{untouched}");
    assert_eq!(untouched["retracted"].as_array().expect("数组").len(), 0);

    let state = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))));
    assert_eq!(
        state["memory_entries"], 1,
        "那条结论不该被牵连——撤的是派生的，不是所有记忆：{state}"
    );
    assert!(
        !memory_id.is_empty(),
        "而按记忆指名仍然是指名得到那一条的"
    );
}

#[test]
fn the_console_can_migrate_to_a_new_topology_epoch() {
    // §17 的"拓扑数量"那一行，从界面上走一遍：迁移有 **epoch**、状态恢复和回滚。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/chat",
        &body(json!({"message": "整理摘要"})),
    );
    // 迁移要搬状态，所以单元得先醒着。
    call(&mut subject, "POST", "/api/unit/wake", &body(json!({})));

    let scaled = json(&call(
        &mut subject,
        "POST",
        "/api/scale",
        &body(json!({
            "envelope": {"ram_limit_mib": 65_536, "cpu_slots": 4}
        })),
    ));
    assert_eq!(scaled["run"]["kind"], "committed", "{scaled}");
    assert_eq!(scaled["run"]["old_epoch"], 1);
    assert_eq!(scaled["run"]["new_epoch"], 2);
    assert_eq!(scaled["route_epoch"], 2, "世代要真的换了：{scaled}");
    assert_eq!(scaled["unit_awake"], true, "搬过去之后单元要醒着");

    // 再迁一次：**世代只能往上走**。
    let again = json(&call(
        &mut subject,
        "POST",
        "/api/scale",
        &body(json!({"envelope": {"ram_limit_mib": 8_192, "cpu_slots": 4}})),
    ));
    assert_eq!(again["run"]["new_epoch"], 3, "{again}");

    // 而资源不足的计划**在开事务之前**就被挡住了。
    let refused = call(
        &mut subject,
        "POST",
        "/api/scale",
        &body(json!({"envelope": {"ram_limit_mib": 64, "cpu_slots": 1}})),
    );
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(
        refused.body.contains("暂停"),
        "理由要说清是计划的问题：{}",
        refused.body
    );
    assert_eq!(
        json(&call(&mut subject, "GET", "/api/state", &body(json!({}))))["unit_awake"],
        true,
        "拒绝不该把单元弄冷"
    );
}

#[test]
fn a_migration_without_a_target_is_refused() {
    // 迁移要有一个**目标**，而目标由一份资源包络算出来。不带包络地"迁移"没有意义，
    // 而给它一个默认值（"就用现在这份"）会造出一种什么都不改的迁移——
    // 它照样会把世代推上去，于是账上多了一次假的扩容。
    let mut subject = subject();
    call(&mut subject, "POST", "/api/unit/wake", &body(json!({})));

    let refused = call(&mut subject, "POST", "/api/scale", &body(json!({})));
    assert_eq!(refused.status, 400);
    assert!(
        refused.body.contains("envelope"),
        "要说清缺什么：{}",
        refused.body
    );
}

#[test]
fn the_console_can_sleep_and_wake_a_unit_and_measures_the_restore_separately() {
    // §17 的"冷恢复"那一行，从界面上走一遍。
    //
    // 要的是**可测量**：恢复耗时与状态字节数单独报出来，而它们不和别的计时混在一起。
    // （那一行的数值是"首轮建议门槛，不是已经达到的成绩"，所以这里不断言它跑得多快。）
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/chat",
        &body(json!({"message": "整理摘要"})),
    );
    call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );

    // 先唤醒：醒着才干得了活，也才降得了温。
    let woken = json(&call(
        &mut subject,
        "POST",
        "/api/unit/wake",
        &body(json!({})),
    ));
    assert_eq!(woken["outcome"]["kind"], "ready", "{woken}");

    // 睡下去。
    let slept = json(&call(
        &mut subject,
        "POST",
        "/api/unit/sleep",
        &body(json!({})),
    ));
    assert_eq!(slept["state"], "COLD", "{slept}");

    let state = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))));
    assert_eq!(state["unit_awake"], false, "{state}");
    assert_eq!(state["unit_state"], "COLD", "{state}");

    // 再醒过来。
    let again = json(&call(
        &mut subject,
        "POST",
        "/api/unit/wake",
        &body(json!({})),
    ));
    assert_eq!(again["outcome"]["kind"], "ready", "{again}");
    assert!(
        again["restore_ms"]["peak"].as_u64().is_some(),
        "恢复耗时要被单独报出来：{again}"
    );
    let bytes = again["state_bytes"]["current"].as_u64().expect("数字");
    assert!(bytes > 0, "一份单元状态不可能是 0 字节：{again}");
    assert!(
        bytes < 64 * 1024,
        "§17 的门槛是 64 KiB，这一份是 {bytes} 字节"
    );
}

#[test]
fn the_console_reports_peaks_not_just_current_values() {
    // §17 那半句："记录**峰值**私有提交及工作集，**不只看平均值**。"
    //
    // 两件事一起测：**当前与峰值成对出现**，以及**值掉回去之后峰值还在**。
    // 少了后者，这份数据与"当前值"没有区别——而 §17 要的正是那个差别。
    let mut subject = subject();
    let memory_id = record_a_memory(&mut subject);

    let before = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))))["resources"]["metrics"]
        ["memories"]
        .clone();
    let peak = before["peak"].as_u64().expect("数字");
    assert!(peak >= 1, "干过活就该有峰值：{before}");
    assert!(
        before["peak_at"].as_str().is_some(),
        "峰值出现的时刻要记着：{before}"
    );

    // 删掉那条结论：当前值掉回去。
    call(
        &mut subject,
        "POST",
        "/api/forget",
        &body(json!({"memory_id": memory_id})),
    );

    let after = json(&call(&mut subject, "GET", "/api/state", &body(json!({}))))["resources"]["metrics"]
        ["memories"]
        .clone();
    assert_eq!(after["current"], 0, "当前值该掉到 0：{after}");
    assert_eq!(after["peak"], peak, "而峰值要留在原处：{after}");

    // 开一段新窗口：峰值从**当前值**起算，不是 0。
    let reset = json(&call(
        &mut subject,
        "POST",
        "/api/resources/reset",
        &body(json!({})),
    ));
    let fresh = reset["resources"]["metrics"]["memories"].clone();
    assert_eq!(fresh["peak"], 0, "当前本来就是 0，所以新窗口的峰值是 0");
    assert!(
        reset["resources"]["metrics"]["events"]["peak"]
            .as_u64()
            .expect("数字")
            >= 1,
        "别的计量照旧：{reset}"
    );
}

#[test]
fn the_console_stops_the_loop_when_told_the_machine_is_out_of_resources() {
    // §17 的"弹性"那一行："**人为**降低可用内存、GPU OOM、磁盘忙时，**停止后台扩容**并
    // 保持取消/审批可响应。"
    //
    // "人为"两个字点明了这一行的验收方式：包络由调用方造，而不是程序去读硬件——本版不读
    // 真实硬件（§19 末段），所以"造一份苛刻的包络"是唯一能测的形式。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/chat",
        &body(json!({"message": "整理摘要"})),
    );

    // 一、健康的包络：照常跑。
    let healthy = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({
            "risk": "a1",
            "rounds": 2,
            "envelope": {"ram_limit_mib": 4096, "cpu_slots": 4}
        })),
    ));
    assert_eq!(healthy["resources"]["kind"], "running", "{healthy}");
    assert_ne!(healthy["schedule"]["kind"], "resource_pressure");

    // 二、把上限降到连控制预算都不够——**一轮都不该跑**。
    let starved = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({
            "risk": "a1",
            "rounds": 2,
            "envelope": {"ram_limit_mib": 64, "cpu_slots": 1}
        })),
    ));
    assert_eq!(
        starved["schedule"]["kind"], "resource_pressure",
        "该停下：{starved}"
    );
    assert_eq!(starved["schedule"]["rounds"], 0, "一轮都不该跑");
    assert!(
        starved["schedule"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("CONTROL_RESERVE")),
        "理由要来自规划器：{starved}"
    );

    // 三、而**控制通道不排在资源队列后面**：压力之下暂停照样能按。
    let paused = call(
        &mut subject,
        "POST",
        "/api/policy/pause",
        &body(json!({"reason": "机器扛不住了"})),
    );
    assert_eq!(paused.status, 200, "{}", paused.body);

    // 四、没有包络时按"可以跑"处理——**那是刻意的默认**，所以钉一条。
    //
    // 权限的默认朝拒绝，因为放行一次不该放行的动作不可逆；调度器的默认朝放行，因为
    // "没给包络"是配置缺失，不是一种压力。让缺配置表现为"永远不跑"的话，一个忘了填表的
    // 界面会看起来像挂了——那种故障最难归因。
    call(
        &mut subject,
        "POST",
        "/api/policy/resume",
        &body(json!({})),
    );
    let unknown = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 1})),
    ));
    assert_eq!(unknown["resources"]["kind"], "unknown", "{unknown}");
    assert_ne!(unknown["schedule"]["kind"], "resource_pressure");
}

#[test]
fn the_loop_reports_why_the_scheduler_stopped() {
    // "跑一段"的返回值要说明**为什么停**。否则界面上只有"跑了 N 轮"，而"没有目标了"
    // 与"连续空转"与"被暂停"三件事看起来一模一样——而它们接下来该做的事完全不同。
    let mut subject = subject();
    let payload = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 4})),
    ));
    assert_eq!(payload["schedule"]["kind"], "finished", "没有目标时一轮就停");
    assert_eq!(payload["schedule"]["rounds"], 1);

    call(
        &mut subject,
        "POST",
        "/api/policy/pause",
        &body(json!({"reason": "演示"})),
    );
    let payload = json(&call(
        &mut subject,
        "POST",
        "/api/loop",
        &body(json!({"risk": "a1", "rounds": 4})),
    ));
    assert_eq!(payload["schedule"]["kind"], "paused");
    assert_eq!(payload["schedule"]["rounds"], 0, "一暂停就一轮也不跑");
}

#[test]
fn pausing_stops_collection_and_egress_and_resuming_lifts_it() {
    // §12.1：暂停要停**三件事**，而恢复只作用于之后。
    let mut subject = subject();
    call(
        &mut subject,
        "POST",
        "/api/chat",
        r#"{"message":"核对摘要"}"#,
    );

    let paused = json(&call(
        &mut subject,
        "POST",
        "/api/policy/pause",
        &body(json!({"reason": "演示"})),
    ));
    assert_eq!(paused["paused"], true);
    assert_eq!(
        paused["stopped"].as_array().expect("数组").len(),
        3,
        "许可、采集、模型调用三件事一起停：{paused}"
    );

    // 采集那一条：423 说的是"现在是锁着的"，与 403 那句"你没有这个权限"不是一回事——
    // 一个去按恢复，一个去重新授权。
    let locked = call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );
    assert_eq!(locked.status, 423, "实际：{}", locked.body);

    // 外发那一条。
    let goal_id = json(&call(
        &mut subject,
        "POST",
        "/api/chat",
        r#"{"message":"再核对一次"}"#,
    ))["goal_id"]
        .as_str()
        .expect("有标识")
        .to_string();
    let blocked = call(
        &mut subject,
        "POST",
        "/api/consult",
        &body(json!({"goal_id": goal_id})),
    );
    assert_eq!(blocked.status, 423, "实际：{}", blocked.body);

    let resumed = json(&call(
        &mut subject,
        "POST",
        "/api/policy/resume",
        &body(json!({})),
    ));
    assert_eq!(resumed["paused"], false);
    assert_eq!(resumed["was"], "演示", "要说得出之前是谁停的");

    let ok = call(
        &mut subject,
        "POST",
        "/api/observe",
        &body(json!({"subject": WATCHED, "data_class": "personal"})),
    );
    assert_eq!(ok.status, 200, "恢复之后又能观测：{}", ok.body);
}

#[test]
fn an_invalid_risk_level_is_refused() {
    let mut subject = subject();
    assert_eq!(
        call(&mut subject, "POST", "/api/select", &body(json!({"risk": "a9"}))).status,
        400
    );
}

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
