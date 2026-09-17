//! 每会话认证与来源检查（§8）。
//!
//! §8 对本地端口有两句话：
//!
//! > 任何本地 HTTP 推理端口只绑定 loopback，带每会话认证和来源检查，**不把"本机地址"当作
//! > 无需授权**。
//!
//! 所以本模块不因为"监听的是 127.0.0.1"就放行任何请求，而是要求三件事同时成立：
//!
//! 1. 监听地址是 loopback（由 `main` 保证，不是在这里检查）；
//! 2. 请求的 `Host` 头指向 loopback 名字——挡住 DNS 重绑定类的手法；
//! 3. 请求带本次会话的 token。
//!
//! **已知局限，写在这里而不是留给读者发现**：页面本身不带 token 校验，token 由服务端注入
//! 页面。因此能打开这个端口的人可以读到 token。这与"能读到同一个用户的文件"是同一个信任
//! 边界——控制台是单用户本机工具，不是多租户服务。若将来要跨用户共享端口，这个设计必须换掉，
//! 而不是加一层"看起来更安全"的包装。

use uuid::Uuid;

use crate::http::Request;

/// 一次控制台会话。
#[derive(Clone, Debug)]
pub struct Session {
    token: String,
}

impl Session {
    /// 生成一个新会话，token 随机。
    pub fn generate() -> Self {
        Self {
            token: Uuid::new_v4().simple().to_string(),
        }
    }

    /// 用指定 token 构造。测试用。
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }

    /// 本次会话的 token。由服务端注入页面。
    pub fn token(&self) -> &str {
        &self.token
    }

    /// 请求是否来自 loopback 名字。
    ///
    /// `Host` 头由客户端提供，所以它挡不住蓄意伪造的请求——它挡的是**浏览器**被诱导去访问
    /// 一个解析到本机的外部域名。那种情况下浏览器发出的 `Host` 是那个外部域名，于是被拒。
    pub fn host_is_loopback(request: &Request) -> bool {
        let Some(host) = request.header("host") else {
            // 没有 Host 头：HTTP/1.1 要求必须有。缺失就当作不可信。
            return false;
        };
        // 去掉端口。IPv6 字面量的方括号里也可能含冒号，所以按最后一个冒号切。
        let name = match host.rsplit_once(':') {
            Some((name, port)) if !name.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                name
            }
            _ => host,
        };
        matches!(
            name.trim_end_matches(']').trim_start_matches('['),
            "127.0.0.1" | "localhost" | "::1"
        )
    }

    /// 请求是否携带本次会话的 token。
    ///
    /// 查询串与请求头两条路都接受：查询串方便浏览器直接点开，请求头方便脚本。两条路都要
    /// 逐字节比较，且**不能**有"前缀匹配"这种宽厚处理——token 比对上的任何宽容都是漏洞。
    pub fn authorizes(&self, request: &Request) -> bool {
        if request.param("token") == Some(self.token.as_str()) {
            return true;
        }
        request.header("x-soca-token") == Some(self.token.as_str())
    }
}
