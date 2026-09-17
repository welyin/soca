//! 模型端点凭据（§3.2、§8、§12.3）。
//!
//! 三条本类型负责的性质，每一条都靠结构而不是靠约定：
//!
//! 1. **不可序列化。** 本类型不实现 `Serialize`。而事件账、审计记录、目标栈、上下文包
//!    全都要求类型可序列化——所以"密钥进不了那些地方"是一件编译期事实，而不是一条
//!    需要人记得遵守的提醒。§3.2 要求"快照中不存密钥"，这是它最省事的实现方式：
//!    让密钥根本放不进去。
//! 2. **`Debug` 打码。** 手写实现只输出指纹。日志、panic 信息、`{:?}` 里拿不到密钥本身。
//! 3. **指纹可显示。** 界面需要回答"我配的是哪一把钥匙"，而回答这个问题不需要交出钥匙。
//!
//! 另外一条与安全直接相关的检查：**明文 HTTP 只允许指向 loopback。** 把 `Authorization`
//! 头发往一个非 loopback 的 `http://` 地址，等于把密钥明文交给路径上的每一跳。§8 说本地
//! 端口"只绑定 loopback"，这里把同一句话用到出站方向。

use std::fmt;

/// 凭据构造失败的原因。
///
/// 所有变体都**不携带密钥内容**，只携带端点与类别名，因此可以安全地写进日志。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(missing_docs)]
pub enum CredentialError {
    #[error("端点地址为空")]
    EmptyEndpoint,

    #[error("端点地址必须以 http:// 或 https:// 开头：{base_url}")]
    UnsupportedScheme { base_url: String },

    #[error(
        "拒绝把密钥以明文 HTTP 发往非 loopback 地址 {base_url}；\
         那等于把密钥交给路径上的每一跳。请用 https://，或指向本机推理服务"
    )]
    InsecureEndpoint { base_url: String },

    #[error("模型名为空")]
    EmptyModel,

    #[error("API 密钥为空")]
    EmptyApiKey,
}

/// 一个模型端点的凭据。
#[derive(Clone, PartialEq, Eq)]
pub struct ModelCredentials {
    base_url: String,
    model: String,
    api_key: String,
    allow_private_egress: bool,
}

impl fmt::Debug for ModelCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 只有指纹，没有密钥。这份 Debug 会出现在日志、断言失败信息与调试器里。
        f.debug_struct("ModelCredentials")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.fingerprint())
            .field("allow_private_egress", &self.allow_private_egress)
            .finish()
    }
}

impl ModelCredentials {
    /// 构造并校验。
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, CredentialError> {
        let base_url = base_url.into().trim().trim_end_matches('/').to_string();
        let model = model.into().trim().to_string();
        let api_key = api_key.into().trim().to_string();

        if base_url.is_empty() {
            return Err(CredentialError::EmptyEndpoint);
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(CredentialError::UnsupportedScheme { base_url });
        }
        if base_url.starts_with("http://") && !is_loopback_endpoint(&base_url) {
            return Err(CredentialError::InsecureEndpoint { base_url });
        }
        if model.is_empty() {
            return Err(CredentialError::EmptyModel);
        }
        if api_key.is_empty() {
            return Err(CredentialError::EmptyApiKey);
        }

        Ok(Self {
            base_url,
            model,
            api_key,
            // 默认不允许把非 public 的观测发出去。§8 那句"私人数据类别不得出站到云端"
            // 是默认值，不是可以通过省略参数绕开的东西。
            allow_private_egress: false,
        })
    }

    /// DeepSeek 的默认端点。它兼容 OpenAI 的 chat completions 协议。
    pub const DEEPSEEK_BASE_URL: &'static str = "https://api.deepseek.com";

    /// DeepSeek 的默认模型。
    pub const DEEPSEEK_MODEL: &'static str = "deepseek-chat";

    /// 端点地址。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 模型名。
    pub fn model(&self) -> &str {
        &self.model
    }

    /// 完整的 chat completions 地址。
    ///
    /// 用户可能填 `https://api.deepseek.com`、`https://api.deepseek.com/v1`，也可能直接把
    /// 完整路径填进来。三种都接受，避免"少写一段 /v1 就静默失败"这种最难查的错误。
    pub fn endpoint(&self) -> String {
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    /// 密钥指纹。**不含密钥本身**，用于界面与日志回答"这是哪一把钥匙"。
    pub fn fingerprint(&self) -> String {
        let key = self.api_key.as_str();
        if key.len() <= 8 {
            return "****".to_string();
        }
        let head: String = key.chars().take(3).collect();
        let tail: String = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}****{tail}")
    }

    /// 是否允许把非 public 的观测发往这个端点。
    pub fn allow_private_egress(&self) -> bool {
        self.allow_private_egress
    }

    /// 显式放开非 public 观测的出站。默认关闭。
    ///
    /// 这是用户的知情决定，而不是系统的默认行为。控制器会在开启时记一条审计。
    #[must_use]
    pub fn with_private_egress(mut self, allow: bool) -> Self {
        self.allow_private_egress = allow;
        self
    }

    /// 取出密钥。**只在构造请求头时调用。**
    ///
    /// 单独一个方法而不是公开字段，是为了让"哪里用了密钥"在代码里可被检索——
    /// 一处调用点比一个到处可读的字段好审计。
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }
}

/// 端点是否指向本机。
fn is_loopback_endpoint(base_url: &str) -> bool {
    let without_scheme = base_url
        .strip_prefix("http://")
        .or_else(|| base_url.strip_prefix("https://"))
        .unwrap_or(base_url);
    // 去掉可能存在的 userinfo 段。
    let host_and_rest = without_scheme.rsplit_once('@').map_or(without_scheme, |(_, r)| r);
    let host = host_and_rest.split('/').next().unwrap_or(host_and_rest);
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(
        host.trim_end_matches(']').trim_start_matches('['),
        "127.0.0.1" | "localhost" | "::1"
    )
}
