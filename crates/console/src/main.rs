//! 启动 SoCA 控制台。
//!
//! ```text
//! soca-console [--db <路径>] [--port <端口>]
//! ```
//!
//! 不带 `--db` 时用内存存储，关掉即忘。带上时目标栈、事件、动作账与记忆都会落盘，
//! 重启后目标还在。
//!
//! **这一版没有接入真实模型。** 应答源是一个确定性桩，它只把收到的证据原样复述成一条带证据
//! 的结论（见 [`stub_transport`]）。这样做的目的是：让上下文编译、网关、预算、返回物校验这
//! 整条通路真实地跑起来并可被观察，同时**不假装**已经有了推理能力。真接远端 API 需要在
//! `Transport` 上实现一个 HTTP 后端，并先定服务商与密钥来源。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use soca_console::{offline_stub, serve, ConsoleModel, Session};
use soca_contracts::{BootId, ModelBackend, ModelBudget, ModelVersion, SubjectId, WallClock};
use soca_core::{ActionBroker, SimulatedOs, Subject};
use soca_core_actors::{DesktopAndFilesCluster, Precondition};
use soca_storage::Store;

/// 启动时播进模拟环境的对象。让界面上的"读取一次"立刻有东西可读。
const SEEDED_FILE: &str = "file:D:\\资料\\摘要\\summary.md";

/// 那份文件的正文。
///
/// 一段像样的文字而不是 `sha256:initial` 这样的占位：观测会把正文存进内容仓，
/// 而"点一下能看到什么"正是这个演示要回答的问题。
const SEEDED_BODY: &str = "资料摘要\n\n- 项目代号：晨星\n- 负责团队：系统组\n- 下一次评审：2026-10-15\n";

/// 默认端口。
const DEFAULT_PORT: u16 = 4319;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db: Option<PathBuf> = None;
    let mut port = DEFAULT_PORT;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => {
                db = Some(PathBuf::from(
                    args.next().ok_or("--db 需要一个路径参数")?,
                ));
            }
            "--port" => {
                port = args.next().ok_or("--port 需要一个端口参数")?.parse()?;
            }
            "--help" | "-h" => {
                println!("用法：soca-console [--db <路径>] [--port <端口>]");
                return Ok(());
            }
            other => return Err(format!("未知参数：{other}").into()),
        }
    }

    let at = WallClock::now();
    let store = match &db {
        Some(path) => Store::open(path, at)?,
        None => Store::open_in_memory(at)?,
    };

    let mut broker = ActionBroker::new(SimulatedOs::new());
    // 播一份初始内容。不播的话界面一打开就是"对象不存在"，而那看起来像故障。
    broker.os_mut().seed(SEEDED_FILE, SEEDED_BODY);

    let cluster = DesktopAndFilesCluster::new(
        SEEDED_FILE,
        vec![Precondition::new(
            "目标目录已授权",
            "cap:read-selected-folder",
        )],
    )?;

    let owner = SubjectId::new("user:local")?;
    let boot = BootId::generate();

    let mut subject = Subject::new(
        store,
        broker,
        cluster,
        owner,
        boot,
        Box::new(offline_stub()),
        ModelBackend::Cpu,
        // §8：远端未获授权。桩跑在本地，所以这不影响它。
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new(soca_console::model::STUB_MODEL)?,
    )?;

    // 上次运行留下的目标要恢复回来。§13.3：目标属于主体，而"属于"意味着它能跨重启存在。
    let restored = subject.restore_goals()?;

    let session = Session::generate();
    let handle = serve(
        Arc::new(Mutex::new(subject)),
        Arc::new(Mutex::new(ConsoleModel::new())),
        session.clone(),
        port,
    )?;

    println!("SoCA 控制台已启动");
    println!("  地址  {}", handle.url());
    println!("  令牌  {}", session.token());
    println!(
        "  存储  {}",
        db.as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "内存（退出即忘，加 --db <路径> 可落盘）".to_string())
    );
    if restored {
        println!("  已从存储恢复目标栈");
    }
    println!();
    println!("  只绑定 127.0.0.1。同机其他用户可以读到这个令牌——");
    println!("  控制台是单用户本机工具，不是多租户服务。");
    println!();
    println!("  模型：未接入。打开页面里的「模型」一节填端点与密钥即可接上。");
    println!("        密钥只保存在本进程内存中，不落盘；重启后需要重填。");
    println!("按 Ctrl+C 退出。");

    loop {
        std::thread::park();
    }
}
