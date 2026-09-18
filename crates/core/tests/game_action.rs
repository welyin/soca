//! §15.2：游戏操作只有 `game.step` 能力，没有任意 OS 权限。
//!
//! 那一条的原文是：
//!
//! > SoCA通过同一L0事件、L1记忆、L3候选和Broker动作循环进入游戏，**游戏操作只有
//! > `game.step`能力，没有任意OS权限**。
//!
//! 此前迷宫那一局是**绕开这条循环**跑的：动作直接交给宿主，没有候选竞争、没有执行许可。
//! 这个文件钉住的是接上之后的样子，以及三件必须成立的事：
//!
//! 1. **没有能力就走不动。** 这是"只有 game.step 能力"的可验收形式——撤掉它，世界一步不动，
//!    而且**执行器一次都不该被试过**。
//! 2. **有了能力才走的是同一条闭环。** 预测先于动作、许可绑定参数、回执之后重新观测、
//!    然后核对。观测走的是与读文件**同一条**路（`ActionBroker::read`）。
//! 3. **出口只认一个工具。** `fs.write` 送到游戏域必须失败，而不是"顺便也能写个文件"。
//!
//! 跑得快的那几条用协议替身（`ProbeFactory`）；最后一条用**真的 MiniGrid**，
//! 因为"接上了"这件事最终要在一个真进程上成立。

use soca_contracts::{
    ActionIntent, ActionLevel, CapabilityPolicyRef, ExplorationQuota, GameKind, GoalBudget,
    ModelBackend, ModelBudget, ModelVersion, PermissionScope, PredictionRef, ResourceCost,
    ResourceScope, SelectionPolicy, SubjectId, ToolId, UnitId, UserChannel, WallClock,
};
use soca_core::game_os::{episode_ref, GameOs, GAME_STEP_TOOL};
use soca_core::{ActionBroker, AdvanceStep, RoundOutcome, SimulatedOs, Subject};
use soca_core_actors::DesktopAndFilesCluster;
use soca_game_host::{ProbeFactory, ProcessEngineConfig, ProcessFactory};
use soca_model_gateway::DeterministicTransport;
use soca_storage::Store;

const GAME_CAP: &str = "cap:game-step";
const WATCHED: &str = "file:D:\\资料\\摘要\\summary.md";
const EPISODE: &str = "episode-test-1";

fn at(offset: i64) -> WallClock {
    WallClock::from_rfc3339("2026-09-18T10:00:00Z")
        .expect("基准时间")
        .plus_seconds(offset)
}

fn owner() -> SubjectId {
    SubjectId::new("user:local").expect("固定主体")
}

fn subject() -> Subject {
    let mut os = SimulatedOs::new();
    os.seed(WATCHED, "资料摘要\n");
    let cluster = DesktopAndFilesCluster::new(WATCHED, Vec::new()).expect("装配能力簇");
    Subject::new(
        Store::open_in_memory(at(0)).expect("内存存储"),
        ActionBroker::new(os),
        cluster,
        owner(),
        soca_contracts::BootId::parse("00000000-0000-4000-8000-0000000000a1").expect("固定 boot"),
        Box::new(DeterministicTransport::new(Vec::new())),
        ModelBackend::Cpu,
        false,
        ModelBudget {
            max_output_tokens: 1024,
            max_wall_millis: 30_000,
            max_attempts: 1,
        },
        ModelVersion::new("sha256:test-model").expect("固定模型版本"),
    )
    .expect("装配主体")
}

/// 起一局、委托一个**只带 `cap:game-step`** 的目标并受理它。
fn playing_subject(grant: bool) -> (Subject, String) {
    let mut subject = subject();
    let episode = subject
        .start_game(GameKind::Maze, 7, &ProbeFactory::new(8))
        .expect("起一局");
    let goal_id = subject
        .delegate(
            "把这一局走完",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: CapabilityPolicyRef::new(GAME_CAP).expect("固定能力策略"),
                max_action_level: ActionLevel::A1,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");

    if grant {
        grant_game_capability(&mut subject, &episode);
    }
    (subject, episode)
}

/// 把 `cap:game-step` 授予**指定的那一个回合**。
///
/// 范围写成精确的 `episode:<id>` 而不是 `episode:` 这一族前缀，原因是
/// [`GrantScope::covers`] 的匹配是**路径形状**的：前缀之后必须紧跟一个分隔符，
/// 否则 `D:\资料\摘要-backup` 会被 `D:\资料\摘要` 放行（见 `is_under` 的注释）。
/// 于是对 `episode:` 这种非路径引用，唯一能用的是精确匹配。
///
/// 那比"想要的"窄——它让"授权一个游戏域"退化成"授权这一局"。窄的方向是安全的，
/// 但它是一条**实现细节泄漏出来的语义**：把范围匹配推广到非路径引用是还没做的一件。
fn grant_game_capability(subject: &mut Subject, episode_ref_str: &str) {
    subject
        .grant_capability(
            CapabilityPolicyRef::new(GAME_CAP).expect("固定能力策略"),
            soca_contracts::GrantScope::under(episode_ref_str).expect("范围"),
            at(2),
        )
        .expect("授予");
}

fn run_one_round(subject: &mut Subject) -> Option<AdvanceStep> {
    match subject
        .run_round(&SelectionPolicy::default(), ActionLevel::A1, at(10))
        .expect("跑一轮")
        .outcome
    {
        RoundOutcome::Advanced { step } => Some(step),
        _ => None,
    }
}

/// 跑到**这次游戏动作**被选中为止，返回那一步。
///
/// 不跑"一轮"就断言，是因为这一局里还有别的候选在竞争：簇自己会提一条文件观测，
/// 而 L3 先挑证据多的、并列时挑先出现的。**它没被选中不是它没投递**——
/// 那正是"进入同一 L3 候选循环"的样子：动作要和别的候选排队。
///
/// 但也因此，这条测试必须能区分"轮到它了但被拒"与"还没轮到它"。
/// 所以判据落在**理由里的字**上，而不是"这一轮返回了 `Refused`"：
/// 一次因为别的原因被拒的观测，会让"没有能力就走不动"这条断言变成假的。
fn run_until_game_action(subject: &mut Subject) -> AdvanceStep {
    for _ in 0..16 {
        let Some(step) = run_one_round(subject) else {
            continue;
        };
        match &step {
            AdvanceStep::Action { tool_id, .. } if tool_id == GAME_STEP_TOOL => return step,
            AdvanceStep::Refused { reason, .. }
                if reason.contains(GAME_CAP) || reason.contains("授权范围") =>
            {
                return step;
            }
            _ => {}
        }
    }
    panic!("跑了十六轮都没轮到这个游戏动作——它不是没投递，就是被别的东西一直挡着");
}

#[test]
fn without_the_capability_the_world_does_not_move_and_the_executor_is_never_tried() {
    // "游戏操作只有 game.step 能力"的可验收形式。
    //
    // 两个数字都要看：世界没动（步序还是 1），而且**执行器一次都没被试过**（0 次尝试）。
    // 只看着前者的话，"被拒绝了"与"执行了但没效果"是同一个样子。
    let (mut subject, _episode) = playing_subject(false);
    assert_eq!(subject.game_progress(), Some((1, 0)));

    subject
        .request_game_step("forward", at(3))
        .expect("投递（投递不等于放行）");
    match run_until_game_action(&mut subject) {
        AdvanceStep::Refused { reason, retry_when } => {
            assert!(
                reason.contains(GAME_CAP),
                "理由要点明是哪一项能力：{reason}"
            );
            // 撤权是此路不通：要恢复得先重新授予（§13.1 的可重试条件）。
            assert_eq!(retry_when, soca_contracts::RetryWhen::Never);
        }
        other => panic!("没有授权却推进了：{other:?}"),
    }

    assert_eq!(
        subject.game_progress(),
        Some((1, 0)),
        "没授权却动了世界，或者执行器被试过了"
    );
}

#[test]
fn a_granted_step_goes_through_predict_admit_dispatch_observe_and_verify() {
    let (mut subject, _episode) = playing_subject(true);
    subject
        .request_game_step("forward", at(3))
        .expect("投递");

    match run_until_game_action(&mut subject) {
        AdvanceStep::Action {
            tool_id,
            receipt,
            verdict,
            ..
        } => {
            assert_eq!(tool_id, GAME_STEP_TOOL, "走的是游戏那一个工具");
            assert!(
                receipt.contains("Completed"),
                "回执应当是完成：{receipt}"
            );
            // 期望是 `Present`——"这一步之后这个回合仍然观测得到"。它被核验成成立，
            // 意味着回执之后**真的有一次观测**，而且那次观测读到了东西。
            //
            // 这条断言的分量在于：它同时证明了观测走的是与读文件**同一条**路。
            // 如果 `Session` 仍然直接读模拟文件系统，`episode:` 永远读不到正文，
            // 这里会得到 `Refuted`。
            assert_eq!(verdict.as_deref(), Some("Supported"), "核验没通过");
        }
        other => panic!("没有推进：{other:?}"),
    }

    let (step_index, attempts) = subject.game_progress().expect("有局");
    assert_eq!(step_index, 2, "世界应当推进了一步");
    assert_eq!(attempts, 1, "执行器应当被试过一次");
}

#[test]
fn the_executor_refuses_any_tool_that_is_not_game_step() {
    // "没有任意 OS 权限"那一句里最要紧的一半：游戏域**不接受**别的工具。
    //
    // 这条在 Broker 层还看不出差别（那里按工具选出口），所以直接对执行器测。
    // 一个"顺便也能写文件"的游戏出口，就是一个比它声明的能力更大的出口。
    let mut game = GameOs::start(&ProbeFactory::new(8), GameKind::Maze, EPISODE, 7).expect("起局");
    let intent = ActionIntent::new(
        soca_contracts::ActionId::new("action:forged-1").expect("固定动作"),
        ToolId::new("fs.write").expect("固定工具"),
        ResourceScope::new(episode_ref(EPISODE)).expect("固定范围"),
        serde_json::json!({ "path": "x", "content": "y" }),
        Vec::new(),
        PredictionRef::new("prediction:forged-1").expect("固定预测"),
        ActionLevel::A1,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 1,
        },
        UnitId::new("unit:test").expect("固定单元"),
    )
    .expect("构造意图");

    match game.execute(&intent) {
        soca_core::AttemptOutcome::Failed { reason } => {
            assert!(reason.contains("game.step"), "理由要点明只收哪一个工具：{reason}");
        }
        other => panic!("别的工具不该被执行：{other:?}"),
    }
    assert_eq!(game.step_index(), 1, "被拒的动作不该推进世界");
}

#[test]
fn the_same_action_id_is_never_applied_twice() {
    // 一步是**不可撤销**的世界推进，重放一次就是多走一步。而多走的那一步会以
    // "局面和预测不符"的形式在很久以后才暴露出来。
    let mut game = GameOs::start(&ProbeFactory::new(8), GameKind::Maze, EPISODE, 7).expect("起局");
    let intent = ActionIntent::new(
        soca_contracts::ActionId::new("action:once-1").expect("固定动作"),
        ToolId::new(GAME_STEP_TOOL).expect("固定工具"),
        ResourceScope::new(episode_ref(EPISODE)).expect("固定范围"),
        serde_json::json!({ "action": { "op": "turn_left" }, "step": 1 }),
        Vec::new(),
        PredictionRef::new("prediction:once-1").expect("固定预测"),
        ActionLevel::A1,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 1,
        },
        UnitId::new("unit:test").expect("固定单元"),
    )
    .expect("构造意图");

    assert!(matches!(
        game.execute(&intent),
        soca_core::AttemptOutcome::Applied { .. }
    ));
    assert!(matches!(
        game.execute(&intent),
        soca_core::AttemptOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(game.step_index(), 2, "只该走一步");
    assert_eq!(game.attempts().len(), 2, "两次都留痕，才能区分'试了两次'" );
}

#[test]
fn the_episode_is_observable_through_the_same_path_as_a_file() {
    // §15.2 那句"同一 L0 事件"。
    //
    // 判据不是"能不能读到正文"，而是**读到的那一份进了事件账**：执行器里的正文与账上的
    // 证据是同一份字节。这里用"预测得到 `Supported`"间接断言它——`Present` 只有在那次
    // 观测真的写成了事件、并被判成存在时才成立。
    let (mut subject, _episode) = playing_subject(true);
    subject
        .request_game_step("turn_left", at(3))
        .expect("投递");
    let step = run_until_game_action(&mut subject);

    let AdvanceStep::Action { verdict, .. } = step else {
        panic!("应当推进");
    };
    assert_eq!(verdict.as_deref(), Some("Supported"));

    // 而感知本身是公开面的形状。
    //
    // 这里**不**断言 7×7：这一局用的是协议替身，它给的是一个方形的符号视图，而不是
    // MiniGrid 那个 7×7。把替身的形状写进断言，会让这条测试在真引擎上失败——而失败的理由
    // 与它要证的东西无关。7×7 那条断言在真引擎的测试里，那里它才是事实。
    match subject.game_percept() {
        Some(soca_contracts::Percept::Maze(view)) => {
            assert!(!view.view.is_empty(), "视图不该是空的");
            let width = view.view[0].len();
            assert!(
                view.view.iter().all(|row| row.len() == width),
                "视图的每一行该一样长"
            );
            assert!(view.direction <= 3);
        }
        other => panic!("迷宫那一路应当给出符号视图：{other:?}"),
    }
}

#[test]
fn a_real_minigrid_engine_steps_through_the_same_door() {
    // 前面的替身只证明"通路接对了"。这一条把真引擎放进同一个出口——
    // 因为是它最终要跑的东西，而"替身上过通了"从来不能替代这件事。
    let program = std::env::var("SOCA_PYTHON").unwrap_or_else(|_| "python".to_string());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("仓库根");
    // 驱动是**引擎**的（一份），游戏是**清单**（每游戏一份）。所以这里两样都要给：
    // 驱动在哪、跑哪个游戏的哪一份清单。少了后者，驱动会拒绝启动——
    // 它不认识任何一个游戏名，那是刻意的。
    let driver = root
        .join("games")
        .join("adapters")
        .join("minigrid")
        .join("driver.py");
    let manifest = root.join("games").join("door-key").join("manifest.json");
    let mut config = ProcessEngineConfig::new(
        program,
        &driver.display().to_string(),
        GameKind::Maze,
    );
    config.args.push("--manifest".to_string());
    config.args.push(manifest.display().to_string());
    let factory = ProcessFactory::new(config);

    let mut game = GameOs::start(&factory, GameKind::Maze, EPISODE, 7).expect("起真局");
    assert!(matches!(
        game.percept(),
        Some(soca_contracts::Percept::Maze(_))
    ));

    let intent = ActionIntent::new(
        soca_contracts::ActionId::new("action:real-1").expect("固定动作"),
        ToolId::new(GAME_STEP_TOOL).expect("固定工具"),
        ResourceScope::new(episode_ref(EPISODE)).expect("固定范围"),
        serde_json::json!({ "action": { "op": "turn_left" }, "step": 1 }),
        Vec::new(),
        PredictionRef::new("prediction:real-1").expect("固定预测"),
        ActionLevel::A1,
        ResourceCost {
            est_ram_bytes: 0,
            est_tokens: 0,
            est_millis: 10,
        },
        UnitId::new("unit:test").expect("固定单元"),
    )
    .expect("构造意图");

    assert!(matches!(
        game.execute(&intent),
        soca_core::AttemptOutcome::Applied { .. }
    ));
    assert_eq!(game.step_index(), 2);
    let direction = match game.percept() {
        Some(soca_contracts::Percept::Maze(view)) => view.direction,
        other => panic!("应当给出迷宫视图：{other:?}"),
    };
    // 左转一次。起始朝向是 3（上），左转之后应当变成 2。
    assert_eq!(direction, 2, "真引擎的左转没生效");
}

#[test]
fn a_grant_for_one_episode_does_not_open_another() {
    // 授权的粒度。一个"允许玩游戏"的授权不该顺手把**别的**回合也放开——
    // 那正是 §12.1 那句"范围限定授权"里范围二字的用处。
    //
    // 这里用的是另一局：起一局新的（`episode-test-2`），只给 `episode-test-1` 授过权。
    let mut subject = subject();
    // 起两局：授的是**第一局**，而正在跑的是第二局。两局的标识由 `start_game` 自己生成，
    // 所以它们必然不同——这正是"同一个 seed 跑两次是两局"那句话的样子。
    let granted = subject
        .start_game(GameKind::Maze, 7, &ProbeFactory::new(8))
        .expect("第一局");
    subject.end_game();
    let running = subject
        .start_game(GameKind::Maze, 7, &ProbeFactory::new(8))
        .expect("第二局");
    assert_ne!(granted, running, "两局的标识不该撞在一起");
    let goal_id = subject
        .delegate(
            "把这一局走完",
            UserChannel::Chat,
            PermissionScope {
                capability_policy_ref: CapabilityPolicyRef::new(GAME_CAP).expect("固定能力策略"),
                max_action_level: ActionLevel::A1,
            },
            GoalBudget::new(16, 8, 8192, 3_600_000).expect("合法额度"),
            ExplorationQuota::new(0),
            at(0),
            None,
        )
        .expect("委托");
    subject.accept(&goal_id, at(1)).expect("受理");
    // 授的是**第一局**，不是正在跑的这一局。
    grant_game_capability(&mut subject, &granted);

    subject
        .request_game_step("forward", at(3))
        .expect("投递");
    match run_until_game_action(&mut subject) {
        AdvanceStep::Refused { reason, .. } => assert!(
            reason.contains("授权范围"),
            "理由要说是范围问题，而不是'没这项能力'：{reason}"
        ),
        other => panic!("另一局不该被放行：{other:?}"),
    }
    assert_eq!(subject.game_progress(), Some((1, 0)));
}
