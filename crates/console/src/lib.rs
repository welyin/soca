//! SoCA 训练台：loopback-only 的 web 控制台（§14、§8）。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`http`] | 一个刚好够用的 HTTP/1.1 子集 |
//! | [`session`] | 每会话令牌与来源检查（§8） |
//! | [`router`] | 请求 → 响应。纯函数式，因此全部行为都能在测试里直接断言 |
//! | [`page`] | 单文件页面，零外部资源 |
//! | [`serve`] | 只绑 loopback 的服务循环 |
//!
//! 两个刻意的选择：
//!
//! 1. **路由是纯函数，socket 层薄到不需要测。** 起一个服务再发 HTTP 请求去猜行为，
//!    会让失败原因藏在网络与解析之间；把行为放在 `handle(subject, session, request, at)`
//!    里，测试就能直接说清是哪一条规则被违反了。
//! 2. **不引入 web 框架。** 只监听 loopback、只服务一个本机用户、不处理 TLS 与长连接——
//!    需要的协议面极小。为此拉进来一串依赖，换到的能力一条都用不上；把协议面收窄到
//!    可审计的几百行更符合 §8 对本地端口的定位。
//!
//! 本 crate 不联网、不读用户文件、不调用模型。页面显示的一切都来自主体的公开状态。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod http;
pub mod page;
pub mod router;
pub mod serve;
pub mod session;

pub use crate::http::{parse_request, HttpError, Request, Response, MAX_BODY_BYTES};
pub use crate::router::handle;
pub use crate::serve::{serve, ServerHandle, BIND_ADDR, MAX_CONNECTIONS};
pub use crate::session::Session;
