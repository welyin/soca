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

use soca_console::{serve, Session};
use soca_contracts::{
    BootId, Candidate, ModelBackend, ModelBudget, ModelOutput, ModelProposal, ModelSelfReport,
    ModelVersion, SubjectId, TokenUsage, WallClock, MODEL_OUTPUT_SCHEMA_VERSION,
};
use soca_core::{ActionBroker, SimulatedOs, Subject};
use soca_core_actors::{DesktopAndFilesCluster, Precondition};
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

/// 启动时播进模拟环境的对象。让界面上的"读取一次"立刻有东西可读。
const SEEDED_FILE: &str = "file:D:\\资料\\摘要\\summary.md";

/// 默认端口。
const DEFAULT_PORT: u16 = 4319;

/// 确定性桩的模型版本标识。
const STUB_MODEL: &str = "sha256:deterministic-stub";

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
    // 播一个初始版本。不播的话界面一打开就是"对象不存在"，而那看起来像故障。
    broker.os_mut().seed(SEEDED_FILE, "sha256:initial");

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
        Box::new(stub_transport()),
        ModelBackend::Cpu,
        // §8：远端未获授权。桩跑在本地，所以这不影响它。
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new(STUB_MODEL)?,
    )?;

    // 上次运行留下的目标要恢复回来。§13.3：目标属于主体，而"属于"意味着它能跨重启存在。
    let restored = subject.restore_goals()?;

    let session = Session::generate();
    let handle = serve(Arc::new(Mutex::new(subject)), session.clone(), port)?;

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
    println!("  模型：未接入。应答源是确定性桩，只会复述它收到的证据。");
    println!("按 Ctrl+C 退出。");

    loop {
        std::thread::park();
    }
}

/// 未接入真实模型时使用的应答源。
///
/// 它做的事只有一件：把上下文里的每一条证据复述成一条带证据的结论。这不是"假装有推理"，
/// 恰恰相反——它**不可能**引用它没看到的东西，因此 §8 那条边界在它身上永远不会被触发，
/// 而整条通路（编译 → 网关 → 预算 → 校验 → 候选）是真的在跑。
fn stub_transport() -> DeterministicTransport {
    DeterministicTransport::from_fn(|request| {
        let proposals = request
            .context
            .evidence
            .iter()
            .map(|slice| ModelProposal {
                candidate: Candidate::Claim {
                    statement: format!("{} 的值是 {}", slice.subject_ref, slice.observed_value),
                    evidence_refs: vec![slice.evidence_ref.clone()],
                },
                self_report: ModelSelfReport {
                    // §3.2：自报数值不是概率。0.5 在这里只是"我没有任何把握"的占位，
                    // 它没有任何校准来源，也不会变成 CalibratedProbability。
                    reported_value: 0.5,
                    model_version: ModelVersion::new(STUB_MODEL).expect("固定模型版本"),
                    rationale: "确定性桩：不是判断，只是复述".to_string(),
                },
                rationale: "未接入真实模型；本桩把收到的证据原样复述，用来证明通路可用"
                    .to_string(),
            })
            .collect();

        Ok(ModelOutput {
            schema_version: MODEL_OUTPUT_SCHEMA_VERSION,
            model_version: ModelVersion::new(STUB_MODEL).expect("固定模型版本"),
            proposals,
            // 桩不消耗 token，记 0 而不是编一个数。编一个数会让成本对照失去意义。
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            claims_finished: false,
        })
    })
}
