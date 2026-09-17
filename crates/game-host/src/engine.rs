//! 规则引擎接口与协议测试替身（实施规格 §8、§10）。
//!
//! §8 的模块表把职责切成两半：**引擎执行规则**，**宿主负责幂等账与下一公开观测**。
//! 因此 [`Engine`] 的返回类型是"已投影的公开感知"，而不是引擎的原始观测：
//! 隐藏真值（雷位、完整地图、RNG 状态、专家动作）在适配器构造感知时就被丢弃，
//! 宿主与客户端根本拿不到它们（§11.1）。
//!
//! §10.1 明确要求"不手写第二套看似相同的规则"。因此本模块**不含迷宫或扫雷规则**：
//! 真实规则引擎来自 MiniGrid 适配器（ENG-07）与成熟 Mines 内核封装（ENG-08），
//! 本模块只提供 [`ProtocolProbeEngine`]——一个明确标注为测试替身的最小引擎，
//! 用来验证宿主与客户端的协议行为（幂等、过期观测、世代、截断与自然终局）。

use serde::{Deserialize, Serialize};
use soca_contracts::{GameAction, GameKind, Outcome, Percept};

use crate::error::EngineError;

/// 引擎的一步结果。
///
/// 所有字段都是**公开面**已经可以承载的内容：宿主会把它们包装成带标识的
/// [`soca_contracts::GameObservation`]，不会再加任何来自引擎内部的字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineStep {
    /// 已投影的公开感知。
    pub percept: Percept,
    /// 规则是否自然结束。
    pub terminated: bool,
    /// 是否被外部截断（步数、时长、预算、人工停止）。
    pub truncated: bool,
    /// 相位。必须与上面两个标志一致，宿主会再校验一次。
    pub outcome: Outcome,
    /// 奖励。初版只给终局胜利 +1。
    pub reward: f64,
}

impl EngineStep {
    /// 进行中的一步。
    pub fn running(percept: Percept) -> Self {
        Self {
            percept,
            terminated: false,
            truncated: false,
            outcome: Outcome::Running,
            reward: 0.0,
        }
    }

    /// 自然终局。
    pub fn finished(percept: Percept, outcome: Outcome, reward: f64) -> Self {
        Self {
            percept,
            terminated: true,
            truncated: false,
            outcome,
            reward,
        }
    }

    /// 被外部截断。**与自然终局分开**，不得把故障伪装成游戏结束（§11.3）。
    pub fn truncated(percept: Percept, outcome: Outcome) -> Self {
        Self {
            percept,
            terminated: false,
            truncated: true,
            outcome,
            reward: 0.0,
        }
    }
}

/// 规则引擎。
pub trait Engine {
    /// 本引擎的游戏种类。宿主据此校验动作域。
    fn game(&self) -> GameKind;

    /// 重置到初始局面。`seed` 只在私有控制面流动，绝不进入公开面（§13）。
    fn reset(&mut self, seed: u64) -> Result<EngineStep, EngineError>;

    /// 执行一步。
    fn step(&mut self, action: GameAction) -> Result<EngineStep, EngineError>;

    /// 是否支持确定性快照恢复。
    ///
    /// §13 要求"未验证可恢复的适配器将本回合标为基础设施截断，不能简单 reset 然后冒充继续"。
    /// 默认不支持，因此宿主在崩溃后不会假装能接着玩。
    fn supports_snapshot(&self) -> bool {
        false
    }

    /// 取私有 checkpoint。
    fn snapshot(&self) -> Result<Vec<u8>, EngineError> {
        Err(EngineError::SnapshotUnsupported)
    }

    /// 从私有 checkpoint 恢复。
    fn restore(&mut self, _checkpoint: &[u8]) -> Result<(), EngineError> {
        Err(EngineError::SnapshotUnsupported)
    }
}

/// 引擎工厂。每个回合拿一个独立引擎，避免回合之间共享可变状态。
pub trait EngineFactory {
    /// 为一次回合创建引擎。
    fn create(&self, game: GameKind, seed: u64) -> Result<Box<dyn Engine>, EngineError>;
}

/// 协议测试替身。
///
/// **它不是迷宫，也不是扫雷。** 它的规则行为是任意的：只按动作类型计数，并在达到上限时
/// 结束。它的全部价值在于让宿主与客户端的协议行为可以被确定性地测到：
///
/// * `set_flag` 目标值与当前值相同时产生"合法但无变化"的一步；
/// * 达到步数上限时产生自然终局；
/// * 动作域不符时给出 [`EngineError::DomainMismatch`]；
/// * 感知里没有任何隐藏真值的通路。
///
/// 正式评测必须使用 ENG-07/ENG-08 的真实引擎；`ProtocolProbeEngine` 不得出现在任何成绩里。
#[derive(Debug, Clone)]
pub struct ProtocolProbeEngine {
    game: GameKind,
    accepted: u8,
    limit: u8,
    direction: u8,
    flags: [bool; 9],
    terminated: bool,
}

impl ProtocolProbeEngine {
    /// 按游戏种类创建一个上限为 `limit` 步的替身。
    pub fn new(game: GameKind, limit: u8) -> Self {
        Self {
            game,
            accepted: 0,
            limit: limit.clamp(1, 8),
            direction: 0,
            flags: [false; 9],
            terminated: false,
        }
    }

    /// 已接受的动作数。
    pub fn accepted(&self) -> u8 {
        self.accepted
    }

    fn maze_percept(&self) -> Percept {
        use soca_contracts::{
            Carrying, MazeCell, MazeCellState, MazeColor, MazeMode, MazeObject, MazeView,
        };
        Percept::Maze(Box::new(MazeView {
            mode: MazeMode::Symbolic,
            // 感知里带上计数，使"这一步确实改变了局面"在协议上可观察。
            // 真实引擎的感知由适配器从公开局面构造，与此无关。
            mission: format!("protocol probe step {}", self.accepted),
            direction: self.direction,
            view: vec![vec![MazeCell {
                object: MazeObject::Agent,
                color: MazeColor::None,
                state: MazeCellState::None,
            }]],
            carrying: Carrying::None,
        }))
    }

    fn mines_percept(&self) -> Percept {
        use soca_contracts::{MinesCell, MinesMode, MinesView};

        let revealed = usize::from(self.accepted);
        let board: Vec<Vec<MinesCell>> = (0..9)
            .map(|index| {
                if self.flags.get(index).copied().unwrap_or(false) {
                    MinesCell::Flagged
                } else if index < revealed {
                    MinesCell::Adjacent(u8::try_from(index).unwrap_or(8).min(8))
                } else {
                    MinesCell::Covered
                }
            })
            .collect::<Vec<_>>()
            .chunks(3)
            .map(<[MinesCell]>::to_vec)
            .collect();

        Percept::Mines(Box::new(MinesView {
            mode: MinesMode::Symbolic,
            width: 3,
            height: 3,
            total_mines: 1,
            board,
        }))
    }

    fn current_percept(&self) -> Percept {
        match self.game {
            GameKind::Maze => self.maze_percept(),
            GameKind::Minesweeper => self.mines_percept(),
        }
    }
}

impl Engine for ProtocolProbeEngine {
    fn game(&self) -> GameKind {
        self.game
    }

    fn reset(&mut self, seed: u64) -> Result<EngineStep, EngineError> {
        self.accepted = 0;
        self.direction = u8::try_from(seed % 4).unwrap_or(0);
        self.flags = [false; 9];
        self.terminated = false;
        Ok(EngineStep::running(self.current_percept()))
    }

    fn step(&mut self, action: GameAction) -> Result<EngineStep, EngineError> {
        if self.terminated {
            return Err(EngineError::EpisodeFinished);
        }
        if !self.game.accepts(action) {
            return Err(EngineError::DomainMismatch);
        }

        match action {
            GameAction::Move(movement) => {
                use soca_contracts::MoveOp;
                match movement.op {
                    MoveOp::TurnLeft => self.direction = (self.direction + 3) % 4,
                    MoveOp::TurnRight => self.direction = (self.direction + 1) % 4,
                    MoveOp::Forward | MoveOp::Pickup | MoveOp::Toggle => {
                        self.accepted = self.accepted.saturating_add(1);
                    }
                }
            }
            GameAction::Targeted(targeted) => {
                // 替身不解释坐标：只按"揭示类动作"计数。
                let _ = targeted;
                self.accepted = self.accepted.saturating_add(1);
            }
            GameAction::Flag(flag) => {
                // 按意图幂等设置：目标值与当前值相同时不改变任何东西。
                let index = (usize::from(flag.row) * 3 + usize::from(flag.column)) % 9;
                if self.flags[index] == flag.flagged {
                    return Ok(EngineStep::running(self.current_percept()));
                }
                self.flags[index] = flag.flagged;
            }
        }

        if self.accepted >= self.limit {
            self.terminated = true;
            return Ok(EngineStep::finished(self.current_percept(), Outcome::Won, 1.0));
        }
        Ok(EngineStep::running(self.current_percept()))
    }
}

/// 固定替身工厂。
#[derive(Debug, Clone, Copy)]
pub struct ProbeFactory {
    /// 每个回合的步数上限。
    pub limit: u8,
}

impl ProbeFactory {
    /// 创建工厂。
    pub fn new(limit: u8) -> Self {
        Self { limit }
    }
}

impl EngineFactory for ProbeFactory {
    fn create(&self, game: GameKind, _seed: u64) -> Result<Box<dyn Engine>, EngineError> {
        Ok(Box::new(ProtocolProbeEngine::new(game, self.limit)))
    }
}
