//! 一个刚好够用的 HTTP/1.1 子集。
//!
//! 为什么手写而不是引入框架：本控制台只监听 loopback、只服务一个本机用户、不处理 TLS 与
//! 长连接，需要的协议面极小。引入一个完整框架会带来一大串依赖，而它们换来的能力这里一条
//! 都用不上；相比之下，**把协议面收窄到可审计的几百行**更符合 §8 对本地端口的定位
//! （"任何本地 HTTP 推理端口只绑定 loopback，带每会话认证和来源检查"）。
//!
//! 明确不支持的、以及为什么：
//!
//! * **不支持分块传输**。请求体必须带 `Content-Length`；分块编码是"先收一堆不知道多长"的
//!   设计，而这里每一条消息都有明确上限。
//! * **不支持 keep-alive 之外的管线化**。一条连接一次一个请求，处理完就回。
//! * **不支持 URL 编码解析成任意结构**。查询串只按 `k=v&k=v` 拆，值做百分号解码。
//! * **不解析多部分表单**。控制台的输入全是 JSON。

use std::collections::BTreeMap;

/// 请求体上限。§11.4 的同一条原则：先检查长度再分配，超限直接断开。
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// 请求头总长度上限。
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

/// 解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(missing_docs)]
pub enum HttpError {
    #[error("读取请求失败：{0}")]
    Io(String),

    #[error("请求头不完整")]
    IncompleteHeaders,

    #[error("请求行格式非法：{0:?}")]
    MalformedRequestLine(String),

    #[error("请求头格式非法：{0:?}")]
    MalformedHeader(String),

    #[error("Content-Length 非法：{0:?}")]
    MalformedContentLength(String),

    #[error("请求体 {actual} 字节超过上限 {limit} 字节")]
    BodyTooLarge { limit: usize, actual: usize },

    #[error("请求头 {actual} 字节超过上限 {limit} 字节")]
    HeadersTooLarge { limit: usize, actual: usize },

    #[error("请求头不是合法 UTF-8")]
    HeadersNotUtf8,

    #[error("请求体不是合法 UTF-8；拒绝按替换字符解码——静默改写用户内容比拒绝它更糟")]
    BodyNotUtf8,
}

/// 一条已解析的请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// 方法，大写。
    pub method: String,
    /// 路径（不含查询串）。
    pub path: String,
    /// 查询参数。
    pub query: BTreeMap<String, String>,
    /// 请求头。键一律小写，这样 `Host` 与 `host` 不会变成两个头。
    pub headers: BTreeMap<String, String>,
    /// 请求体。
    pub body: String,
}

impl Request {
    /// 读一个查询参数。
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    /// 读一个请求头（键大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// 从原始字节里解析一条请求。
///
/// `bytes` 可以包含不止一条请求，但本实现只解析第一条——控制台不使用管线化。
pub fn parse_request(bytes: &[u8]) -> Result<Request, HttpError> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(HttpError::IncompleteHeaders)?;
    if header_end > MAX_HEADER_BYTES {
        return Err(HttpError::HeadersTooLarge {
            limit: MAX_HEADER_BYTES,
            actual: header_end,
        });
    }

    let head = std::str::from_utf8(&bytes[..header_end]).map_err(|_| HttpError::HeadersNotUtf8)?;

    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        return Err(HttpError::MalformedRequestLine(request_line.to_string()));
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.to_string(), BTreeMap::new()),
    };

    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::MalformedHeader(line.to_string()));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    // 先看长度再决定要不要读体。声明超过上限就直接拒绝，不按声明去分配。
    let body_start = header_end + 4;
    let declared: usize = match headers.get("content-length") {
        Some(raw) => raw
            .parse()
            .map_err(|_| HttpError::MalformedContentLength(raw.clone()))?,
        None => 0,
    };
    if declared > MAX_BODY_BYTES {
        return Err(HttpError::BodyTooLarge {
            limit: MAX_BODY_BYTES,
            actual: declared,
        });
    }

    // 请求体必须真的是 UTF-8。早先这里用的是有损解码，于是客户端编码不对时中文会被静默
    // 改写成 `????`，用户看到的是一个"看起来生效了、其实内容已经变了"的目标。
    // 静默改写用户内容比拒绝它更糟：前者不可察觉，后者至少能被修。
    let body_bytes = bytes.get(body_start..body_start + declared).unwrap_or(&[]);
    let body = std::str::from_utf8(body_bytes)
        .map_err(|_| HttpError::BodyNotUtf8)?
        .to_string();

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn parse_query(raw: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        params.insert(percent_decode(name), percent_decode(value));
    }
    params
}

/// 百分号解码。`+` 按查询串惯例解成空格。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    // 解不开就原样保留。悄悄丢掉一个字符会让"参数没生效"变得无法排查。
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 一条待发送的响应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// 状态码。
    pub status: u16,
    /// 内容类型。
    pub content_type: &'static str,
    /// 响应体。
    pub body: String,
}

impl Response {
    /// JSON 响应。
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        }
    }

    /// HTML 响应。
    pub fn html(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
        }
    }

    /// 纯文本响应。
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
        }
    }

    /// 序列化成待写入 socket 的字节。
    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n\r\n",
            self.status,
            reason,
            self.content_type,
            self.body.len(),
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }
}
