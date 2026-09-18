//! 游戏域的"操作系统"：`game.step` 的唯一出口（§12.2、§15.2）。
//!
//! §15.2 的原话是：
//!
//! > SoCA通过同一L0事件、L1记忆、L3候选和Broker动作循环进入游戏，**游戏操作只有
//! > `game.step`能力，没有任意OS权限**。
//!
//! 这句话有两个断言，本模块要同时落实：
//!
//! 1. **游戏操作只有 `game.step`。** 这里只认一个工具标识，别的工具送到这里一律
//!    [`AttemptOutcome::Failed`]。**不是"顺便也能做点别的"**——一个能执行两种动作的
//!    出口，就是一个比它声明的能力更大的出口。
//! 2. **没有任意 OS 权限。** 适配器进程能做很多事（起子进程、读游戏文件、反序列化），
//!    而**这些都不因此成为认知循环的能力**。它们进不来：执行许可绑定的是工具、参数摘要
//!    与作用对象，而这里能执行的动作集合是写死的。§12.2 那句"能访问底层不等于能执行
//!    所有底层动作"，在这一屏里就是这三行检查。
//!
//! ## 一个回合就是一个对象
//!
//! [`GameOs::read`] 对 `episode:...` 返回的"内容"是**当前公开感知的 JSON**。
//!
//! 这样安排的用处是让"同一 L0 事件"那句真的成立：[`crate::session::Session::observe`]
//! 本来就会把对象正文写进内容仓、再记一条观测事件——于是**游戏的感知与文件的观测走的是
//! 同一条路**：同一套证据引用、同一套保留期、同一本事件账。另起一条"游戏观测"的通路，
//! 会让"这条记忆是从哪来的"有两个答案，而那个问题正是证据链要回答的。
//!
//! ## 幂等在**动作**这一层，不在引擎那一层
//!
//! 同一个 `action_id` 只应用一次（§7.3），这与 [`crate::os::SimulatedOs`] 是同一套语义。
//! 这一条很要紧：`game.step` 是一次**不可撤销**的世界推进，重放一次就是多走一步，
//! 而多走的那一步会以"局面和预测不符"的形式在很久以后才暴露出来。
//!
//! 也因此，动作标识**必须**由"这一步是第几步"参与派生。只用动作内容派生的话，连着两次
//! `forward` 会撞成同一个标识，第二次会被安静地判成 `AlreadyApplied`——**看起来一切正常，
//! 而它没有动**。

use std::collections::BTreeMap;

use soca_contracts::{ActionIntent, GameAction, GameKind, Percept, Sha256Hex};
use soca_game_host::{Engine, EngineError, EngineFactory};

use crate::os::{Attempt, AttemptOutcome, ObjectState};

/// 游戏操作的**唯一**工具标识（§15.2）。
pub const GAME_STEP_TOOL: &str = "game.step";

/// 一个回合在公开面上的对象引用。
///
/// 与 `file:` 一样是一个**前缀约定**，因为它要出现在证据引用、权限范围与预测里，
/// 而那三处都按字符串比较。让它们各自拼一遍，迟早会拼出两个不一样的引用，
/// 于是核验永远说不一致，而"哪一处拼错了"看不出来。
pub fn episode_ref(episode_id: &str) -> String {
    format!("episode:{episode_id}")
}

/// 游戏域的执行器。
///
/// 它持有一个规则引擎。**引擎是它拥有的**，不是借的：一个回合的生命周期与这个执行器一样长，
/// 而把它放在别处会立刻遇到"谁在借用谁"的问题（宿主持有引擎、宿主要借用存储、存储属于主体）。
pub struct GameOs {
    engine: Box<dyn Engine>,
    episode_ref: String,
    step_index: u64,
    /// `action_id` → 应用后的版本。动作级幂等（§7.3）。
    applied: BTreeMap<String, String>,
    attempts: Vec<Attempt>,
    /// 当前公开感知的正文。它同时是 `episode:` 对象的"内容"。
    body: String,
}

impl std::fmt::Debug for GameOs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打正文：正文是感知，感知可能很大，而日志里出现一份就多一份可以泄漏的副本。
        f.debug_struct("GameOs")
            .field("game", &self.engine.game())
            .field("episode_ref", &self.episode_ref)
            .field("step_index", &self.step_index)
            .field("attempts", &self.attempts.len())
            .finish_non_exhaustive()
    }
}

impl GameOs {
    /// 起一个回合。`seed` 属于**私有控制面**：它进引擎，绝不进正文（§13）。
    pub fn start(
        factory: &dyn EngineFactory,
        game: GameKind,
        episode_id: &str,
        seed: u64,
    ) -> Result<Self, EngineError> {
        let mut engine = factory.create(game, seed)?;
        let step = engine.reset(seed)?;
        Ok(Self {
            engine,
            episode_ref: episode_ref(episode_id),
            step_index: 1,
            applied: BTreeMap::new(),
            attempts: Vec::new(),
            body: body_of(&step.percept),
        })
    }

    /// 承载的游戏。
    pub fn game(&self) -> GameKind {
        self.engine.game()
    }

    /// 回合的对象引用。
    pub fn episode_ref(&self) -> &str {
        &self.episode_ref
    }

    /// 已经推进到第几步。
    pub fn step_index(&self) -> u64 {
        self.step_index
    }

    /// 全部执行尝试（含失败与重复），用于断言"只应用了一次"。
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    /// 当前公开感知。
    pub fn percept(&self) -> Option<Percept> {
        serde_json::from_str(&self.body).ok()
    }

    /// 这个对象在当前公开面上读得到什么。
    ///
    /// 只有本回合自己那个引用读得到。别的引用一律 `None`——**不是空对象**。
    /// 返回一个"存在的空对象"会让"这一局还没有感知"和"这里本来就没东西"看起来一样。
    pub fn read(&self, subject_ref: &str) -> Option<ObjectState> {
        (subject_ref == self.episode_ref).then(|| ObjectState::new(&self.body))
    }

    /// 执行一次 `game.step`。
    pub fn execute(&mut self, intent: &ActionIntent) -> AttemptOutcome {
        let action_id = intent.action_id.to_string();
        let tool_id = intent.tool_id.to_string();

        let refuse = |this: &mut Self, reason: String| {
            let outcome = AttemptOutcome::Failed { reason };
            this.attempts.push(Attempt {
                action_id: action_id.clone(),
                tool_id: tool_id.clone(),
                subject_ref: intent.object_scope.to_string(),
                outcome: outcome.clone(),
            });
            outcome
        };

        // 一、只有 `game.step` 能进这道门。
        if tool_id != GAME_STEP_TOOL {
            return refuse(
                self,
                format!("游戏域只执行 {GAME_STEP_TOOL}，收到 {tool_id}（§15.2）"),
            );
        }
        // 二、只有本回合那一个对象。
        if intent.object_scope.as_str() != self.episode_ref {
            return refuse(
                self,
                format!(
                    "这次动作指向 {}，而本回合是 {}；游戏域不接受别的对象",
                    intent.object_scope, self.episode_ref
                ),
            );
        }

        // 三、动作级幂等。**在推进世界之前**查（§7.3）。
        if let Some(version) = self.applied.get(&action_id) {
            let outcome = AttemptOutcome::AlreadyApplied {
                version: version.clone(),
            };
            self.attempts.push(Attempt {
                action_id,
                tool_id,
                subject_ref: self.episode_ref.clone(),
                outcome: outcome.clone(),
            });
            return outcome;
        }

        // 四、取出动作。参数里只有一个 `action` 字段，它必须是一个合法的公开动作。
        let Some(action) = intent
            .parameters
            .get("action")
            .and_then(|value| serde_json::from_value::<GameAction>(value.clone()).ok())
        else {
            return refuse(
                self,
                "game.step 需要参数 action，且它必须是协议公开的动作（§10.1）".to_string(),
            );
        };
        if !self.engine.game().accepts(action) {
            return refuse(
                self,
                format!("动作 {:?} 不属于 {:?}", action.op_name(), self.engine.game()),
            );
        }

        // 五、推进世界。
        match self.engine.step(action) {
            Ok(step) => {
                self.step_index = self.step_index.saturating_add(1);
                self.body = body_of(&step.percept);
                let version = Sha256Hex::of_bytes(self.body.as_bytes()).to_string();
                self.applied.insert(action_id.clone(), version.clone());
                let outcome = AttemptOutcome::Applied { version };
                self.attempts.push(Attempt {
                    action_id,
                    tool_id,
                    subject_ref: self.episode_ref.clone(),
                    outcome: outcome.clone(),
                });
                outcome
            }
            // 引擎报错**不是**"这一步没发生"。§11.4 那句"超时不代表动作未执行"同样适用于
            // 这里，所以它走 `Failed`——回执会记成 `Failed`，而"局面到底动不动过"
            // 只能靠下一次观测去核对，不能靠这一句。
            Err(error) => refuse(self, format!("规则引擎拒绝了这一步：{error}")),
        }
    }
}

/// 公开感知的正文。
///
/// 序列化失败时给一个**显式的缺席表示**，而不是空字符串：空字符串会变成一个"存在的空对象"，
/// 于是 `Present` 预测会通过、而读的人看到的是"这一局什么都没有"。
fn body_of(percept: &Percept) -> String {
    serde_json::to_string(percept)
        .unwrap_or_else(|error| format!("{{\"error\":\"感知无法序列化：{error}\"}}"))
}
