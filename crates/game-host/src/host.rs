//! 游戏规则宿主（实施规格 §8、§9、§11）。
//!
//! 宿主是"认知循环"与"规则引擎"之间的闸门。它做四件事，顺序不可颠倒：
//!
//! 1. **语义校验**：非法请求不推进世界，只产生一条拒绝回执并消耗一次违规预算（§11.2）。
//! 2. **幂等账**：先查 `(episode_id, request_id)` 再决定是否执行。载荷相同则返回原回执、
//!    **不执行 step**；载荷不同则 `IDEMPOTENCY_CONFLICT`。
//! 3. **新鲜度校验**：回合是否活跃、世代是否过期、提交者依据的观测是否过期。
//!    过期观测必须被拒绝，而不是把旧格子的动作应用在新局面上。
//! 4. **一步一写者**：一个回合同时只处理一个请求；宿主是一个 `&mut self`，这一点由类型保证。
//!
//! 三类"看起来像结束"的情况在这里被严格分开：
//!
//! | 情况 | `terminated` | `truncated` | 说明 |
//! |---|---|---|---|
//! | 规则判赢/判负 | true | false | 自然终局 |
//! | 步数/时长/预算/人工停止 | false | true | 外部截断 |
//! | 引擎故障、请求结局未知 | false | true | 基础设施截断，**不得从分母里删除** |
//!
//! 宿主不持有"最佳动作"或"真值"：它把请求交给引擎，把引擎投影后的公开感知包成
//! [`GameObservation`]。隐藏世界状态在这条路径上没有出口。

use std::collections::BTreeMap;

use soca_contracts::{
    ActionRequest, GameActionReceipt, GameKind, GameLedgerStatus, GameObservation,
    GameObservationTag, GameReceiptTag, Outcome, PublicId, ReceiptCode, ReceiptStatus, WallClock,
    GAME_PROTOCOL_VERSION,
};
use soca_storage::{LedgerDecision, Store};
use uuid::Uuid;

use crate::engine::{Engine, EngineFactory, EngineStep};
use crate::error::{EngineError, HostError};

/// 宿主配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostConfig {
    /// 连续违规多少次后把回合截断为协议失败（§11.2）。成功的一步会清零计数。
    pub max_consecutive_violations: u8,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            max_consecutive_violations: 8,
        }
    }
}

/// 一个回合的运行状态。
///
/// 不派生 `Debug`：引擎实现来自适配器，是否可打印由它们自己决定；宿主不替它们作主。
struct EpisodeState {
    session_id: PublicId,
    game: GameKind,
    topology_epoch: u64,
    step_index: u64,
    consecutive_violations: u8,
    /// 已经结束：规则终局或被截断。
    finished: bool,
    /// 结束是否由外部截断造成。
    truncated: bool,
    last_observation: GameObservation,
    engine: Box<dyn Engine>,
}

/// 游戏规则宿主。
///
/// 持有存储句柄是因为幂等账必须与"是否执行"在同一个判定里；这也是 §7 把动作账放进
/// SQLite 单写者库的原因。
pub struct GameHost<'a> {
    store: &'a mut Store,
    config: HostConfig,
    episodes: BTreeMap<String, EpisodeState>,
}

impl std::fmt::Debug for GameHost<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只打印回合标识与配置：引擎内部可能持有隐藏世界状态，不该被顺手打进日志。
        f.debug_struct("GameHost")
            .field("config", &self.config)
            .field(
                "episodes",
                &self.episodes.keys().collect::<Vec<&String>>(),
            )
            .finish_non_exhaustive()
    }
}

impl<'a> GameHost<'a> {
    /// 建立宿主。
    pub fn new(store: &'a mut Store, config: HostConfig) -> Self {
        Self {
            store,
            config,
            episodes: BTreeMap::new(),
        }
    }

    /// 开始一个回合，返回公开的初始观测。
    ///
    /// `seed` 只交给引擎，绝不进入观测、标识或回执（§13）。本方法属于**私有控制面**：
    /// 认知单元拿不到它。
    pub fn start_episode(
        &mut self,
        session_id: PublicId,
        episode_id: PublicId,
        game: GameKind,
        seed: u64,
        topology_epoch: u64,
        factory: &dyn EngineFactory,
    ) -> Result<GameObservation, HostError> {
        let key = episode_id.to_string();
        if self.episodes.contains_key(&key) {
            return Err(HostError::EpisodeAlreadyExists { episode_id: key });
        }

        let mut engine = factory.create(game, seed)?;
        if engine.game() != game {
            return Err(HostError::CorruptState("工厂返回的引擎与请求的游戏不符"));
        }
        let step = engine.reset(seed)?;
        let observation = build_observation(&episode_id, &session_id, game, 0, step)?;

        let state = EpisodeState {
            session_id,
            game,
            topology_epoch,
            step_index: 1,
            consecutive_violations: 0,
            finished: observation.terminated || observation.truncated,
            truncated: observation.truncated,
            last_observation: observation.clone(),
            engine,
        };
        self.episodes.insert(key, state);
        Ok(observation)
    }

    /// 读取当前公开观测。
    pub fn observe(&self, episode_id: &PublicId) -> Result<GameObservation, HostError> {
        self.episodes
            .get(episode_id.as_str())
            .map(|state| state.last_observation.clone())
            .ok_or_else(|| HostError::UnknownEpisode {
                episode_id: episode_id.to_string(),
            })
    }

    /// 回合是否已经结束。
    pub fn is_finished(&self, episode_id: &PublicId) -> bool {
        self.episodes
            .get(episode_id.as_str())
            .is_some_and(|state| state.finished)
    }

    /// 回合是否被外部截断而非自然终局。
    pub fn is_truncated(&self, episode_id: &PublicId) -> bool {
        self.episodes
            .get(episode_id.as_str())
            .is_some_and(|state| state.truncated)
    }

    /// 当前世代。迁移之后由协调器调用 [`GameHost::advance_epoch`]。
    pub fn epoch(&self, episode_id: &PublicId) -> Option<u64> {
        self.episodes
            .get(episode_id.as_str())
            .map(|state| state.topology_epoch)
    }

    /// 推进回合的拓扑世代。旧世代的在途请求从此一律 `STALE_TOPOLOGY`。
    pub fn advance_epoch(&mut self, episode_id: &PublicId, epoch: u64) -> Result<(), HostError> {
        let state = self
            .episodes
            .get_mut(episode_id.as_str())
            .ok_or_else(|| HostError::UnknownEpisode {
                episode_id: episode_id.to_string(),
            })?;
        state.topology_epoch = epoch;
        Ok(())
    }

    /// 处理一次动作请求，返回公开回执。
    ///
    /// 所有"请求不可接受"的情况都走同一条拒绝路径：产生回执、计入违规、**不推进世界**。
    /// 只有拿到 [`LedgerDecision::Fresh`] 才会真正执行一步。
    pub fn submit(
        &mut self,
        request: &ActionRequest,
        at: WallClock,
    ) -> Result<GameActionReceipt, HostError> {
        let key = request.episode_id.to_string();

        // 1. 幂等账。**先登记再判定**：连非法请求也要在账上留一行，否则拒绝路径无处可写，
        //    而且同一个非法请求会被反复"首次"处理。登记放在语义校验之前是有意的。
        match self.store.record_game_request(request, at)? {
            // 冲突回执是**响应**，不是账务变更：原记录已经结算过，不能被这条新载荷覆盖。
            // 因此这里只产生回执并计入违规，不写账。
            LedgerDecision::Conflict { .. } => {
                let receipt = rejected_receipt(request, ReceiptCode::IdempotencyConflict);
                self.note_violation(&key);
                return Ok(receipt);
            }
            // 载荷相同：返回原回执，不执行 step（§11.2 的核心）。
            LedgerDecision::Replay(receipt) => return Ok(*receipt),
            // 上一次执行结局未知。既不能重放，也不能假装没发生（§13）。
            LedgerDecision::Pending => {
                let receipt = unknown_commit_receipt(request);
                self.store.settle_game_request(
                    &request.episode_id,
                    &request.request_id,
                    GameLedgerStatus::UnknownCommit,
                    None,
                    Some(&receipt),
                    at,
                )?;
                self.mark_truncated(&key, Outcome::InfrastructureError);
                return Ok(receipt);
            }
            LedgerDecision::Fresh => {}
        }

        // 2. 语义校验、回合存在性、新鲜度与动作域。这一步不改变任何状态。
        let verdict = match self.episodes.get(&key) {
            None => Some(ReceiptCode::InvalidAction),
            Some(state) => {
                if request.validate_semantics().is_err() {
                    Some(ReceiptCode::InvalidAction)
                } else if state.finished {
                    Some(ReceiptCode::EpisodeFinished)
                } else if request.topology_epoch != state.topology_epoch {
                    // 拓扑迁移之后，旧世代的动作不得继续作用于新局面（§6.2）。
                    Some(ReceiptCode::StaleTopology)
                } else if request.expected_observation_id != state.last_observation.observation_id
                {
                    // 过期观测必须重新感知，不能默默把旧格子的动作应用在新局面上。
                    Some(ReceiptCode::StaleObservation)
                } else if !state.game.accepts(request.action) {
                    Some(ReceiptCode::InvalidAction)
                } else {
                    None
                }
            }
        };
        if let Some(code) = verdict {
            return self.reject(request, code, at);
        }

        // 5. 执行一步。到这里才真正可能改变世界。
        let executed = {
            let state = self
                .episodes
                .get_mut(&key)
                .ok_or_else(|| HostError::UnknownEpisode {
                    episode_id: key.clone(),
                })?;
            let previous_percept = state.last_observation.percept.clone();
            match state.engine.step(request.action) {
                Ok(step) => {
                    let observation = build_observation(
                        &request.episode_id,
                        &state.session_id,
                        state.game,
                        state.step_index,
                        step,
                    )?;
                    state.step_index = state.step_index.saturating_add(1);
                    state.finished = observation.terminated || observation.truncated;
                    state.truncated = observation.truncated;
                    state.consecutive_violations = 0;
                    // 合法但没改变局面：仍消耗一步、仍产生新观测（§11.3）。
                    let unchanged = !observation.terminated
                        && !observation.truncated
                        && observation.percept == previous_percept;
                    state.last_observation = observation;
                    Ok((state.last_observation.clone(), unchanged))
                }
                Err(error) => Err(error),
            }
        };

        match executed {
            Ok((observation, unchanged)) => {
                let (status, code) = if unchanged {
                    (ReceiptStatus::NoChange, ReceiptCode::NoChange)
                } else {
                    (ReceiptStatus::Applied, ReceiptCode::Ok)
                };
                let receipt = completed_receipt(request, &observation, status, code);
                self.store.settle_game_request(
                    &request.episode_id,
                    &request.request_id,
                    GameLedgerStatus::from_receipt(status),
                    Some(&observation.observation_id),
                    Some(&receipt),
                    at,
                )?;
                Ok(receipt)
            }
            Err(error) => {
                // 引擎故障是基础设施截断，不能伪装成"玩家输了"（§11.3）。
                let receipt = rejected_receipt(request, ReceiptCode::EngineUnavailable);
                self.store.settle_game_request(
                    &request.episode_id,
                    &request.request_id,
                    GameLedgerStatus::Rejected,
                    None,
                    Some(&receipt),
                    at,
                )?;
                if matches!(error, EngineError::EpisodeFinished) {
                    self.mark_truncated(&key, Outcome::Aborted);
                } else {
                    self.note_violation(&key);
                }
                Ok(receipt)
            }
        }
    }

    // ---- 拒绝与终结路径 ----------------------------------------------------

    fn reject(
        &mut self,
        request: &ActionRequest,
        code: ReceiptCode,
        at: WallClock,
    ) -> Result<GameActionReceipt, HostError> {
        let receipt = rejected_receipt(request, code);
        self.store.settle_game_request(
            &request.episode_id,
            &request.request_id,
            GameLedgerStatus::Rejected,
            None,
            Some(&receipt),
            at,
        )?;
        self.note_violation(&request.episode_id.to_string());
        Ok(receipt)
    }

    /// 记一次违规。连续违规到上限即把回合截断为协议失败，避免无限无成本重试（§11.2）。
    fn note_violation(&mut self, key: &str) {
        let limit = self.config.max_consecutive_violations;
        let Some(state) = self.episodes.get_mut(key) else {
            return;
        };
        state.consecutive_violations = state.consecutive_violations.saturating_add(1);
        if state.consecutive_violations >= limit {
            state.finished = true;
            state.truncated = true;
            let mut observation = state.last_observation.clone();
            observation.terminated = false;
            observation.truncated = true;
            observation.outcome = Outcome::Aborted;
            state.last_observation = observation;
        }
    }

    /// 把回合标为截断。**不是**规则判负。
    fn mark_truncated(&mut self, key: &str, outcome: Outcome) {
        let Some(state) = self.episodes.get_mut(key) else {
            return;
        };
        state.finished = true;
        state.truncated = true;
        let mut observation = state.last_observation.clone();
        observation.terminated = false;
        observation.truncated = true;
        observation.outcome = outcome;
        state.last_observation = observation;
    }
}

/// 把引擎的公开步结果包成带标识的公开观测，并再校验一次语义。
///
/// 适配器构造的感知即使形状合法，也可能相位矛盾；契约层的校验在这里兜底。
fn build_observation(
    episode_id: &PublicId,
    session_id: &PublicId,
    game: GameKind,
    step_index: u64,
    step: EngineStep,
) -> Result<GameObservation, HostError> {
    let observation = GameObservation {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: GameObservationTag::Observation,
        session_id: session_id.clone(),
        episode_id: episode_id.clone(),
        // §13：公开 ID 随机生成，不编码种子、答案或内容哈希。
        observation_id: PublicId::new(format!("obs-{}", Uuid::new_v4()))?,
        step_index,
        game,
        terminated: step.terminated,
        truncated: step.truncated,
        outcome: step.outcome,
        reward: step.reward,
        percept: step.percept,
    };
    observation.validate_semantics()?;
    Ok(observation)
}

fn rejected_receipt(request: &ActionRequest, code: ReceiptCode) -> GameActionReceipt {
    GameActionReceipt {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: GameReceiptTag::ActionReceipt,
        request_id: request.request_id.clone(),
        episode_id: request.episode_id.clone(),
        status: ReceiptStatus::Rejected,
        code,
        observation_id: None,
    }
}

fn unknown_commit_receipt(request: &ActionRequest) -> GameActionReceipt {
    GameActionReceipt {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: GameReceiptTag::ActionReceipt,
        request_id: request.request_id.clone(),
        episode_id: request.episode_id.clone(),
        status: ReceiptStatus::UnknownCommit,
        code: ReceiptCode::EngineUnavailable,
        observation_id: None,
    }
}

fn completed_receipt(
    request: &ActionRequest,
    observation: &GameObservation,
    status: ReceiptStatus,
    code: ReceiptCode,
) -> GameActionReceipt {
    GameActionReceipt {
        protocol_version: GAME_PROTOCOL_VERSION,
        message_type: GameReceiptTag::ActionReceipt,
        request_id: request.request_id.clone(),
        episode_id: request.episode_id.clone(),
        status,
        code,
        observation_id: Some(observation.observation_id.clone()),
    }
}
