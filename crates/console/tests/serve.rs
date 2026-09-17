//! 服务循环的集成测试：真的起一个 socket，真的发一条 HTTP 请求过去。
//!
//! 路由行为已经在 `router.rs` 里测过，所以这里只盯三件只有真 socket 才能暴露的事：
//! 绑定地址是 loopback、请求能穿过读循环、响应能被完整写回。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use soca_console::{serve, Session, BIND_ADDR};
use soca_contracts::{
    ModelBackend, ModelBudget, ModelOutput, ModelVersion, SubjectId, TokenUsage, WallClock,
    MODEL_OUTPUT_SCHEMA_VERSION,
};
use soca_core::{ActionBroker, SimulatedOs, Subject};
use soca_core_actors::{DesktopAndFilesCluster, Precondition};
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";

fn subject() -> Subject {
    let now = WallClock::from_rfc3339("2026-09-17T10:00:00Z").expect("基准时间");
    let mut broker = ActionBroker::new(SimulatedOs::new());
    broker.os_mut().seed(WATCHED, "sha256:initial");
    let cluster = DesktopAndFilesCluster::new(
        WATCHED,
        vec![Precondition::new("目录已授权", "cap:read-selected-folder")],
    )
    .expect("装配能力簇");

    Subject::new(
        Store::open_in_memory(now).expect("内存存储"),
        broker,
        cluster,
        SubjectId::new("user:local").expect("固定主体"),
        soca_contracts::BootId::generate(),
        Box::new(DeterministicTransport::from_fn(|_| {
            Ok(ModelOutput {
                schema_version: MODEL_OUTPUT_SCHEMA_VERSION,
                model_version: ModelVersion::new("sha256:stub").expect("固定模型版本"),
                proposals: Vec::new(),
                usage: TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
                claims_finished: false,
            })
        })),
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

fn raw_get(addr: SocketAddr, path: &str, token: Option<&str>, host: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("连接");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("设置超时");

    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(token) = token {
        request.push_str(&format!("x-soca-token: {token}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("写入");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("读取");
    String::from_utf8_lossy(&response).into_owned()
}

#[test]
fn the_server_binds_loopback_only_and_answers_over_a_real_socket() {
    assert_eq!(BIND_ADDR.to_string(), "127.0.0.1");

    let session = Session::generate();
    let handle = serve(
        Arc::new(Mutex::new(subject())),
        session.clone(),
        0, // 端口 0：让系统分配，避免测试之间抢端口
    )
    .expect("启动");

    let addr = handle.local_addr();
    assert!(addr.ip().is_loopback(), "只绑 loopback（§8）");
    assert_ne!(addr.port(), 0, "系统分配了端口");

    // 带令牌：放行。
    let ok = raw_get(addr, "/api/state", Some(session.token()), "127.0.0.1");
    assert!(ok.starts_with("HTTP/1.1 200 OK"), "实际：{ok}");
    assert!(ok.contains("\"owner\":\"user:local\""));
    assert!(ok.contains("Content-Length:"));

    // 不带令牌：拒绝。
    let denied = raw_get(addr, "/api/state", None, "127.0.0.1");
    assert!(denied.starts_with("HTTP/1.1 401 Unauthorized"), "实际：{denied}");

    // 首页不需要令牌，但要把令牌注入进去。
    let page = raw_get(addr, "/", None, "127.0.0.1");
    assert!(page.starts_with("HTTP/1.1 200 OK"));
    assert!(page.contains(session.token()), "页面里要有令牌");

    handle.stop();
}

#[test]
fn a_foreign_host_header_is_refused_over_a_real_socket() {
    // 浏览器被诱导访问一个解析到本机的域名时，Host 会是那个域名。这条测试模拟的就是它。
    let session = Session::generate();
    let handle = serve(Arc::new(Mutex::new(subject())), session.clone(), 0).expect("启动");
    let addr = handle.local_addr();

    let response = raw_get(addr, "/api/state", Some(session.token()), "evil.example.com");
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden"),
        "实际：{response}"
    );

    handle.stop();
}

#[test]
fn a_post_with_a_body_survives_the_read_loop() {
    // 请求体要穿过"先读头、再按 Content-Length 读体"这条路径。这条路径最容易出的问题是
    // 只读了头就交给解析器，于是 body 永远是空的。
    let session = Session::generate();
    let handle = serve(Arc::new(Mutex::new(subject())), session.clone(), 0).expect("启动");
    let addr = handle.local_addr();

    let payload = r#"{"message":"为已授权目录生成摘要"}"#;
    let mut stream = TcpStream::connect(addr).expect("连接");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("设置超时");
    let request = format!(
        "POST /api/chat HTTP/1.1\r\nHost: 127.0.0.1\r\nx-soca-token: {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        session.token(),
        payload.len()
    );
    stream.write_all(request.as_bytes()).expect("写入");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("读取");
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200 OK"), "实际：{text}");
    assert!(text.contains("goal:1"), "应当返回新建的目标");

    handle.stop();
}
