//! 远端传输层的回归测试，跑在一个本机 mock 服务端上。
//!
//! 为什么不用真的调一次 DeepSeek：那需要一个密钥，而把密钥放进测试就意味着它会进仓库、
//! 进 CI 日志、进任何一个曾经 clone 过的人手里。**用一个本机 mock 能覆盖的东西，就不要用
//! 真实凭据去覆盖。**
//!
//! 这里测的四件事都是在真端点上同样成立、而在 mock 上更容易观察的：
//!
//! * 密钥只出现在 `Authorization` 头里，不进请求体；
//! * 证据用下标引用，由本层解析成真实的 `EvidenceRef`，因此模型编不出引用；
//! * 越界下标被明确拒绝；
//! * HTTP 状态码被翻译成正确的重试类别。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};
use soca_contracts::{
    ActionLevel, BeliefSummary, CapabilitySlice, ContextBundle, DataClass, EvidenceRef,
    EvidenceSlice, ModelBudget, OutputSchema, ToolId, WallClock,
};
use soca_model_gateway::{
    CredentialError, ModelCredentials, ModelRequest, RemoteTransport, Transport, TransportError,
};

// ---------------------------------------------------------------------------
// mock 服务端
// ---------------------------------------------------------------------------

/// 读完整条请求：先读头，再按 `Content-Length` 读体。
///
/// **只 `read` 一次是不够的。** TCP 会把请求切成几段，抢在客户端写完请求体之前应答然后关闭
/// 连接，会让客户端拿到 `ECONNRESET`——表现为偶发的"网络错误"，而 mock 自己看起来一切正常。
/// 这个坑在本文件里真的发生过：第一版 mock 只读一次，四个测试随机失败。
fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let mut body_start: Option<usize> = None;
    let mut declared = 0usize;

    loop {
        if body_start.is_none()
            && let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let head = String::from_utf8_lossy(&buffer[..end]);
            for line in head.split("\r\n").skip(1) {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("content-length")
                {
                    declared = value.trim().parse().unwrap_or(0);
                }
            }
            body_start = Some(end + 4);
        }
        if let Some(start) = body_start
            && buffer.len() >= start + declared
        {
            break;
        }
        if buffer.len() > 256 * 1024 {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }

    String::from_utf8_lossy(&buffer).into_owned()
}

/// 起一个只应答一次的本机 HTTP 服务，返回它的地址和"收到的原始请求"的句柄。
fn mock_server(status: u16, body: Value) -> (SocketAddr, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 loopback");
    let addr = listener.local_addr().expect("地址");
    let captured = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&captured);
    let body = body.to_string();

    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let raw = read_full_request(&mut stream);
        if let Ok(mut guard) = sink.lock() {
            *guard = raw;
        }
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            status = status,
            len = body.len(),
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });

    (addr, captured)
}

/// 把一个 OpenAI 形状的响应体包起来。
///
/// 内层 `content` 是一个**字符串**，里面才是模型输出的 JSON——这一层嵌套是最容易写错的地方，
/// 所以让序列化器负责转义而不是手拼。
fn completion(content: &str) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": "deepseek-chat",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
    })
}

// ---------------------------------------------------------------------------
// 请求侧
// ---------------------------------------------------------------------------

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-17T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn bundle_with_one_evidence() -> ContextBundle {
    ContextBundle::new(
        "为已授权目录生成摘要",
        vec![EvidenceSlice {
            evidence_ref: EvidenceRef::new("obs:real-1").expect("固定证据"),
            subject_ref: "file:summary.md".to_string(),
            observed_value: "sha256:aaa".to_string(),
            data_class: DataClass::Public,
        }],
        vec![BeliefSummary {
            statement: "摘要文件已经更新".to_string(),
            evidence_refs: vec![EvidenceRef::new("obs:real-1").expect("固定证据")],
        }],
        Vec::new(),
        CapabilitySlice {
            tool_ids: vec![ToolId::new("fs.read").expect("固定工具")],
            max_action_level: ActionLevel::A1,
        },
        at(600),
        OutputSchema::read_only(),
        Vec::new(),
    )
    .expect("上下文合法")
}

fn request_to() -> ModelRequest {
    ModelRequest {
        context: bundle_with_one_evidence(),
        budget: ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 5_000,
            max_attempts: 1,
        },
        model_profile_ref: soca_contracts::ModelProfileRef::new("profile:reasoning")
            .expect("固定画像"),
    }
}

fn transport_to(addr: SocketAddr, api_key: &str) -> RemoteTransport {
    RemoteTransport::new(
        ModelCredentials::new(format!("http://{addr}"), "deepseek-chat", api_key)
            .expect("loopback 明文端点允许"),
    )
}

// ---------------------------------------------------------------------------
// 凭据
// ---------------------------------------------------------------------------

#[test]
fn a_non_loopback_http_endpoint_cannot_even_be_constructed() {
    // 把 Authorization 头发往非 loopback 的明文 HTTP，等于把密钥交给路径上的每一跳。
    // 这条检查放在构造函数里，因此不存在"先构造出来再提醒自己别用"的中间态。
    let result = ModelCredentials::new("http://api.deepseek.com", "deepseek-chat", "sk-test");
    assert!(matches!(
        result,
        Err(CredentialError::InsecureEndpoint { .. })
    ));

    // https 与非 loopback 的 http 本机地址都可以。
    assert!(ModelCredentials::new("https://api.deepseek.com", "deepseek-chat", "sk-test").is_ok());
    assert!(
        ModelCredentials::new("http://127.0.0.1:8080", "local", "sk-test").is_ok(),
        "本机推理服务用明文是合理的：它不出机器"
    );
}

#[test]
fn the_debug_representation_carries_a_fingerprint_not_the_key() {
    // 这份 Debug 会出现在日志、断言失败信息与调试器里。
    let credentials =
        ModelCredentials::new("https://api.deepseek.com", "deepseek-chat", "sk-abcdefghijklmnop")
            .expect("合法");
    let rendered = format!("{credentials:?}");
    assert!(!rendered.contains("abcdefghijklmnop"), "实际：{rendered}");
    assert!(rendered.contains("sk-****mnop"));
    assert_eq!(credentials.fingerprint(), "sk-****mnop");
}

#[test]
fn the_endpoint_accepts_all_three_shapes_users_actually_type() {
    for (input, expected) in [
        (
            "https://api.deepseek.com",
            "https://api.deepseek.com/chat/completions",
        ),
        (
            "https://api.deepseek.com/v1",
            "https://api.deepseek.com/v1/chat/completions",
        ),
        (
            "https://api.deepseek.com/chat/completions",
            "https://api.deepseek.com/chat/completions",
        ),
    ] {
        let credentials = ModelCredentials::new(input, "deepseek-chat", "sk-test").expect("合法");
        assert_eq!(credentials.endpoint(), expected, "输入 {input}");
    }
}

// ---------------------------------------------------------------------------
// 一次完整调用
// ---------------------------------------------------------------------------

#[test]
fn a_loopback_endpoint_answers_and_its_proposals_are_parsed() {
    let content = json!({
        "proposals": [{
            "kind": "claim",
            "statement": "summary.md 的版本是 sha256:aaa",
            "evidence_indexes": [0],
            "rationale": "上下文里的观测就是这么写的",
            "self_report": 0.8
        }]
    })
    .to_string();
    let (addr, captured) = mock_server(200, completion(&content));

    let output = transport_to(addr, "sk-abcdefghijklmnop")
        .invoke(&request_to())
        .expect("应当成功");

    assert_eq!(output.proposals.len(), 1);
    // 证据被解析成了**上下文里真实存在的那一条**，而不是模型写下的任何字符串。
    assert_eq!(
        output.proposals[0].candidate.evidence_refs(),
        [EvidenceRef::new("obs:real-1").expect("固定证据")]
    );
    assert_eq!(output.proposals[0].self_report.reported_value, 0.8);
    assert_eq!(output.usage.input_tokens, 100);
    assert_eq!(output.usage.output_tokens, 20);
    assert_eq!(output.model_version.as_str(), "deepseek-chat");

    // 请求侧的检查：密钥只在 Authorization 头里，不在请求体里。
    let raw = captured.lock().expect("可加锁").clone();
    assert!(raw.contains("Authorization: Bearer sk-abcdefghijklmnop"));
    let (_, body) = raw.split_once("\r\n\r\n").expect("有请求体");
    assert!(
        !body.contains("sk-abcdefghijklmnop"),
        "密钥不得出现在请求体里"
    );
    // 证据以引用形式给出，模型看到的就是它能引用的全部。
    assert!(body.contains("obs:real-1"));
    assert!(body.contains("allowed_candidates"));
}

#[test]
fn an_out_of_range_evidence_index_is_refused() {
    // 模型写不出 "obs:phantom"，它只能写下标；越界下标在这里被挡住。
    let content = json!({
        "proposals": [{
            "kind": "claim",
            "statement": "编一个",
            "evidence_indexes": [7],
            "rationale": "我猜的"
        }]
    })
    .to_string();
    let (addr, _) = mock_server(200, completion(&content));

    let result = transport_to(addr, "sk-test").invoke(&request_to());
    assert!(
        matches!(result, Err(TransportError::Malformed { .. })),
        "实际：{result:?}"
    );
}

#[test]
fn a_claim_without_any_evidence_index_is_still_parseable_and_fails_later() {
    // 本层不替候选集合做结构校验——那是 `validate_proposals` 的职责，而它离模型边界更近。
    // 这里只确认"没有证据的结论"不会被本层悄悄补上一条证据。
    let content = json!({
        "proposals": [{"kind": "claim", "statement": "无凭无据", "evidence_indexes": [], "rationale": "?"}]
    })
    .to_string();
    let (addr, _) = mock_server(200, completion(&content));

    let output = transport_to(addr, "sk-test")
        .invoke(&request_to())
        .expect("解析本身成功");
    assert!(
        output.proposals[0].candidate.evidence_refs().is_empty(),
        "本层不得替模型补证据"
    );

    let context = bundle_with_one_evidence();
    assert!(
        context.validate_proposals(&output.proposals).is_err(),
        "缺证据的结论必须在下一道闸被拦住"
    );
}

#[test]
fn an_action_proposal_is_refused_with_a_clear_reason() {
    // 动作意图需要一次执行许可签发，而那条通路还没有界面与审批流程。明确拒绝，而不是
    // 悄悄把它降级成别的候选——降级会让"我提了但没执行"变得无法解释。
    let content = json!({
        "proposals": [{"kind": "action", "statement": "改文件", "rationale": "顺手"}]
    })
    .to_string();
    let (addr, _) = mock_server(200, completion(&content));

    let result = transport_to(addr, "sk-test").invoke(&request_to());
    match result {
        Err(TransportError::Malformed { reason }) => {
            assert!(reason.contains("执行许可"), "实际：{reason}");
        }
        other => panic!("应当被拒，实际：{other:?}"),
    }
}

#[test]
fn model_output_that_is_not_json_is_refused() {
    // 不开 json 模式的端点、或者被截断的响应，都会走到这里。
    let (addr, _) = mock_server(200, completion("这不是 JSON"));
    let result = transport_to(addr, "sk-test").invoke(&request_to());
    assert!(matches!(result, Err(TransportError::Malformed { .. })));
}

// ---------------------------------------------------------------------------
// 失败分类：它直接决定网关要不要重试
// ---------------------------------------------------------------------------

#[test]
fn a_401_is_a_non_retryable_rejection() {
    let (addr, _) = mock_server(401, json!({"error": "invalid api key"}));
    let result = transport_to(addr, "sk-wrong").invoke(&request_to());
    match result {
        Err(error) => {
            assert!(!error.is_retryable(), "鉴权错了再试多少次都一样");
            assert_eq!(error.as_str(), "rejected");
            // 错误信息里不能带密钥。
            assert!(!error.to_string().contains("sk-wrong"));
        }
        Ok(_) => panic!("401 不该成功"),
    }
}

#[test]
fn a_429_is_retryable() {
    let (addr, _) = mock_server(429, json!({"error": "rate limited"}));
    let result = transport_to(addr, "sk-test").invoke(&request_to());
    match result {
        Err(error) => {
            assert!(error.is_retryable(), "限流等一会儿可能就好了");
            assert_eq!(error.as_str(), "unavailable");
        }
        Ok(_) => panic!("429 不该成功"),
    }
}

#[test]
fn a_400_is_a_non_retryable_rejection() {
    // 模型名写错会走到这里。重试无益，而且每次都白等一个超时。
    let (addr, _) = mock_server(400, json!({"error": "model not found"}));
    let result = transport_to(addr, "sk-test").invoke(&request_to());
    match result {
        Err(error) => assert!(!error.is_retryable()),
        Ok(_) => panic!("400 不该成功"),
    }
}

#[test]
fn an_unreachable_endpoint_is_reported_as_unavailable() {
    // 绑一个端口再立刻释放：地址合法但没人监听。
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
        listener.local_addr().expect("地址")
    };
    let transport = RemoteTransport::new(
        ModelCredentials::new(format!("http://{addr}"), "deepseek-chat", "sk-test").expect("合法"),
    );
    let result = transport.invoke(&request_to());
    match result {
        Err(error) => {
            assert!(error.is_retryable(), "连不上可能是暂时性的");
            assert!(!error.to_string().contains("sk-test"));
        }
        Ok(_) => panic!("连不上不该成功"),
    }
}

#[test]
fn the_request_body_carries_the_compiled_context() {
    // §8：模型只能依据上下文作答。这条测试盯的是"上下文真的发出去了"——
    // 少了它，模型收到一个空上下文时会开始编，而全部校验都会通过（因为它编的结论不带证据，
    // 会停在下一道闸上，但那时已经浪费了一次往返）。
    let (addr, captured) = mock_server(200, completion(r#"{"proposals":[]}"#));
    let transport = transport_to(addr, "sk-test");
    transport.invoke(&request_to()).expect("成功");

    let raw = captured.lock().expect("可加锁").clone();
    assert!(raw.starts_with("POST /chat/completions HTTP/1.1"), "实际：{raw}");
    let (_, body) = raw.split_once("\r\n\r\n").expect("有请求体");
    assert!(body.contains("为已授权目录生成摘要"), "目标要在上下文里");
    assert!(body.contains("obs:real-1"), "证据要在上下文里");
    assert!(
        body.contains("response_format"),
        "开了 json 模式，避免模型回一段散文"
    );
}
