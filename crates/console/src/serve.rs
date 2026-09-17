//! loopback-only 的 HTTP 服务（§8）。
//!
//! 三条本模块负责的性质：
//!
//! 1. **只绑 loopback。** 绑不上其他地址不是配置问题，是接口问题——本模块不提供那个参数。
//! 2. **单写者。** 主体挂在互斥量后面，一次只处理一条请求。这与存储层的单写者约束同源：
//!    并发的两个请求同时给目标扣额度，会让额度记账失去意义。
//! 3. **并发有界。** 每条连接一个线程，但总数有上限。一个本机控制台不需要支撑连接风暴，
//!    而没有上限的 `spawn` 就是一个可以被本地进程打满的入口。

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use soca_contracts::WallClock;
use soca_core::Subject;

use crate::http::{parse_request, HttpError, Response, MAX_BODY_BYTES, MAX_HEADER_BYTES};
use crate::model::ConsoleModel;
use crate::router;
use crate::session::Session;

/// 绑定地址。只监听 loopback。
pub const BIND_ADDR: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// 同时处理的连接上限。
pub const MAX_CONNECTIONS: usize = 8;

/// 运行中的服务句柄。
#[derive(Debug)]
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
}

impl ServerHandle {
    /// 实际监听的地址。端口传 0 时这里给出系统分配的那个。
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// 请求停止。已经接受的连接会跑完。
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// 控制台地址。
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// 启动服务。
pub fn serve(
    subject: Arc<Mutex<Subject>>,
    model: Arc<Mutex<ConsoleModel>>,
    session: Session,
    port: u16,
) -> std::io::Result<ServerHandle> {
    let listener = TcpListener::bind(SocketAddr::from((BIND_ADDR, port)))?;
    let addr = listener.local_addr()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let live = Arc::new(AtomicUsize::new(0));

    let flag = Arc::clone(&shutdown);
    let counter = Arc::clone(&live);
    thread::spawn(move || {
        for incoming in listener.incoming() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = incoming else { continue };

            if counter.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                counter.fetch_sub(1, Ordering::SeqCst);
                // 超限的连接直接关掉，不排队。排队会让"服务还活着吗"变得难以判断。
                drop(stream);
                continue;
            }

            let subject = Arc::clone(&subject);
            let model = Arc::clone(&model);
            let session = session.clone();
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                handle_connection(stream, &subject, &model, &session);
                counter.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    Ok(ServerHandle { addr, shutdown })
}

fn handle_connection(
    mut stream: TcpStream,
    subject: &Mutex<Subject>,
    model: &Mutex<ConsoleModel>,
    session: &Session,
) {
    let response = match read_request(&mut stream) {
        Ok(request) => {
            // 时刻按挂钟取。控制台是活的服务，"现在"就是现在——重放与可复现由测试里的
            // 固定时刻负责，不靠服务器把时间冻住。
            let at = WallClock::now();
            let mut subject = match subject.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut model = match model.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            router::handle(&mut subject, &mut model, session, &request, at)
        }
        Err(HttpError::IncompleteHeaders) => Response::text(400, "请求不完整"),
        Err(HttpError::BodyTooLarge { limit, actual }) => {
            Response::text(413, format!("请求体 {actual} 字节超过上限 {limit} 字节"))
        }
        Err(error) => Response::text(400, error.to_string()),
    };

    let _ = stream.write_all(&response.to_bytes());
    let _ = stream.flush();
}

/// 从连接里读出一条完整请求。
fn read_request(stream: &mut TcpStream) -> Result<crate::http::Request, HttpError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let mut body_start: Option<usize> = None;
    let mut declared = 0usize;

    loop {
        if body_start.is_none() {
            if let Some(end) = find_headers_end(&buffer) {
                if end > MAX_HEADER_BYTES {
                    return Err(HttpError::HeadersTooLarge {
                        limit: MAX_HEADER_BYTES,
                        actual: end,
                    });
                }
                declared = declared_length(&buffer[..end])?;
                body_start = Some(end + 4);
            } else if buffer.len() > MAX_HEADER_BYTES {
                return Err(HttpError::HeadersTooLarge {
                    limit: MAX_HEADER_BYTES,
                    actual: buffer.len(),
                });
            }
        }

        if let Some(start) = body_start
            && buffer.len() >= start + declared
        {
            break;
        }

        if buffer.len() > MAX_HEADER_BYTES + MAX_BODY_BYTES {
            return Err(HttpError::BodyTooLarge {
                limit: MAX_BODY_BYTES,
                actual: buffer.len(),
            });
        }

        let read = stream
            .read(&mut chunk)
            .map_err(|error| HttpError::Io(error.to_string()))?;
        if read == 0 {
            // 对端关了连接。头都没齐就当作不完整；齐了就按已有的内容解析。
            if body_start.is_none() {
                return Err(HttpError::IncompleteHeaders);
            }
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    parse_request(&buffer)
}

fn find_headers_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn declared_length(head: &[u8]) -> Result<usize, HttpError> {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let value = value.trim().to_string();
            return value
                .parse()
                .map_err(|_| HttpError::MalformedContentLength(value));
        }
    }
    Ok(0)
}
