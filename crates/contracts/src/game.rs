//! 认知游戏公开协议 v1（实施规格 §11、工单 ENG-01）。
//!
//! 本模块只描述**公开面**：agent 能看到的观测、能提交的动作请求、以及回执。
//! 隐藏世界状态（雷位、完整地图、seed、RNG、解法、专家动作、评估字段）在这里没有任何
//! 对应的类型，因此它们不可能被序列化进公开消息。§11.1 的"不直接转发 Gymnasium 的
//! `info`"不是纪律要求，而是类型系统的后果：没有字段可以放它。
//!
//! 三条本模块落实的规则：
//!
//! * **严格拒绝多余字段**：每个消息与每个感知都带 `deny_unknown_fields`。协议里多一个
//!   字段就是一次夹带尝试，必须失败而不是忽略。
//! * **动作只允许公开操作集合**：`done`、`drop` 这类引擎自带但不属于任务的通道不在
//!   [`GameAction`] 里，因此构造不出来（§10.1）。
//! * **形状之外还要语义校验**：Schema 只能验证形状，尺寸、状态转换和相位一致性由
//!   [`Observation::validate_semantics`] 等方法负责（§11.4）。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::opaque_string;
use crate::ContractError;

/// 公开协议版本。
pub const GAME_PROTOCOL_VERSION: u8 = 1;

/// 迷宫可见域允许的最大边长（§10.1 的局部 7×7 视图，留出余量给其他任务）。
pub const MAZE_VIEW_MAX: usize = 31;

/// 扫雷棋盘允许的最大边长。
pub const MINES_BOARD_MAX: u16 = 64;

/// 坐标允许的最大值（Schema 上限，与 `u8` 的取值域不同）。
pub const MAX_COORDINATE: u8 = 63;

opaque_string!(
    /// 公开标识。不含种子、答案或内容哈希（§13）。
    PublicId,
    "public_id",
    128
);

/// 游戏种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameKind {
    /// 迷宫。
    Maze,
    /// 扫雷。
    Minesweeper,
}

impl GameKind {
    /// 稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Maze => "maze",
            Self::Minesweeper => "minesweeper",
        }
    }

    /// 该游戏接受哪一类动作。
    pub fn accepts(self, action: GameAction) -> bool {
        action.domain() == self.action_domain()
    }

    /// 该游戏的动作域。
    pub fn action_domain(self) -> ActionDomain {
        match self {
            Self::Maze => ActionDomain::Maze,
            Self::Minesweeper => ActionDomain::Mines,
        }
    }
}

/// 终局相位。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// 进行中。
    Running,
    /// 规则自然结束：赢。
    Won,
    /// 规则自然结束：输。
    Lost,
    /// 外部截断：超时。
    Timeout,
    /// 外部截断：中止。
    Aborted,
    /// 外部截断：基础设施故障。
    InfrastructureError,
}

impl Outcome {
    /// 是否要求 `terminated = true`。
    pub fn requires_terminated(self) -> bool {
        matches!(self, Self::Won | Self::Lost)
    }

    /// 是否要求 `truncated = true`。
    pub fn requires_truncated(self) -> bool {
        matches!(self, Self::Timeout | Self::Aborted | Self::InfrastructureError)
    }

    /// 稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Won => "won",
            Self::Lost => "lost",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
            Self::InfrastructureError => "infrastructure_error",
        }
    }
}

/// 动作所属的域。跨域动作必须由宿主拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionDomain {
    /// 迷宫类：转向、前进、拾取、开关门。
    Maze,
    /// 扫雷类：揭示、标旗、chord。
    Mines,
}

/// 迷宫类操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveOp {
    /// 左转。
    TurnLeft,
    /// 右转。
    TurnRight,
    /// 前进。
    Forward,
    /// 拾取。
    Pickup,
    /// 开关门 / 切换。
    Toggle,
}

/// 需要坐标的揭示类操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevealOp {
    /// 揭示。
    Reveal,
    /// 邻旗数满足时的展开。
    Chord,
}

/// 标旗操作。刻意不是 toggle：重试必须幂等（§11.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetFlagOp {
    /// 设置或撤销旗标，按 `flagged` 的意图幂等生效。
    SetFlag,
}

/// 迷宫类动作，只允许 `op` 一个字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveAction {
    /// 操作。
    pub op: MoveOp,
}

/// 需要坐标的揭示类动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetedAction {
    /// `reveal` 或 `chord`。
    pub op: RevealOp,
    /// 行，从 0 起。
    pub row: u8,
    /// 列，从 0 起。
    pub column: u8,
}

/// 标旗动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlagAction {
    /// 固定为 `set_flag`。
    pub op: SetFlagOp,
    /// 行，从 0 起。
    pub row: u8,
    /// 列，从 0 起。
    pub column: u8,
    /// 目标状态，不是翻转。
    pub flagged: bool,
}

/// 公开动作集合。
///
/// 引擎可能还有 `done`、`drop`、`Undo`、`Solve` 之类通道，它们**不在**这里。
/// §10.1 与 §10.2 要求禁用它们，因此协议层就不提供构造路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GameAction {
    /// 迷宫类动作。
    Move(MoveAction),
    /// 揭示类动作。
    Targeted(TargetedAction),
    /// 标旗动作。
    Flag(FlagAction),
}

impl GameAction {
    /// 所属动作域。
    pub fn domain(self) -> ActionDomain {
        match self {
            Self::Move(_) => ActionDomain::Maze,
            Self::Targeted(_) | Self::Flag(_) => ActionDomain::Mines,
        }
    }

    /// 坐标，迷宫类动作为 `None`。
    pub fn coordinates(self) -> Option<(u8, u8)> {
        match self {
            Self::Move(_) => None,
            Self::Targeted(action) => Some((action.row, action.column)),
            Self::Flag(action) => Some((action.row, action.column)),
        }
    }

    /// 稳定名称，用于审计与幂等哈希。
    pub fn op_name(self) -> &'static str {
        match self {
            Self::Move(action) => match action.op {
                MoveOp::TurnLeft => "turn_left",
                MoveOp::TurnRight => "turn_right",
                MoveOp::Forward => "forward",
                MoveOp::Pickup => "pickup",
                MoveOp::Toggle => "toggle",
            },
            Self::Targeted(action) => match action.op {
                RevealOp::Reveal => "reveal",
                RevealOp::Chord => "chord",
            },
            Self::Flag(_) => "set_flag",
        }
    }

    /// 坐标是否落在协议允许的范围内。
    pub fn coordinates_in_range(self) -> bool {
        self.coordinates()
            .is_none_or(|(row, column)| row <= MAX_COORDINATE && column <= MAX_COORDINATE)
    }
}

/// 迷宫感知的模式标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MazeMode {
    /// 符号视图。
    #[serde(rename = "symbolic_maze")]
    Symbolic,
}

/// 扫雷感知的模式标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MinesMode {
    /// 符号棋盘。
    #[serde(rename = "symbolic_mines")]
    Symbolic,
}

/// 像素感知的模式标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelMode {
    /// RGB 帧。
    #[serde(rename = "pixels")]
    Pixels,
}

/// 像素格式。初版只有 RGB8。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 每通道 8 位，三通道。
    #[serde(rename = "RGB8")]
    Rgb8,
}

/// 迷宫中一个格子的对象。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MazeObject {
    /// 视野之外，必须保持 unknown。
    Unseen,
    /// 空。
    Empty,
    /// 墙。
    Wall,
    /// 门。
    Door,
    /// 钥匙。
    Key,
    /// 球。
    Ball,
    /// 箱子。
    Box,
    /// 目标。
    Goal,
    /// 岩浆。
    Lava,
    /// agent 自身。
    Agent,
    /// 地板。
    Floor,
}

/// 格子颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MazeColor {
    /// 无色。
    None,
    /// 红。
    Red,
    /// 绿。
    Green,
    /// 蓝。
    Blue,
    /// 紫。
    Purple,
    /// 黄。
    Yellow,
    /// 灰。
    Grey,
}

/// 格子的开关状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MazeCellState {
    /// 无。
    None,
    /// 开着。
    Open,
    /// 关着。
    Closed,
    /// 锁着。
    Locked,
}

/// 迷宫中携带的物品。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Carrying {
    /// 未携带。
    None,
    /// 钥匙。
    Key,
    /// 球。
    Ball,
    /// 箱子。
    Box,
}

/// 迷宫视图里的一个格子。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MazeCell {
    /// 对象。
    pub object: MazeObject,
    /// 颜色。
    pub color: MazeColor,
    /// 开关状态。
    pub state: MazeCellState,
}

/// 迷宫局部视图（§10.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MazeView {
    /// 固定为 `symbolic_maze`。
    pub mode: MazeMode,
    /// 公开任务描述。
    pub mission: String,
    /// 朝向，0..=3。
    pub direction: u8,
    /// 行优先的局部视图。
    pub view: Vec<Vec<MazeCell>>,
    /// 携带物。
    pub carrying: Carrying,
}

/// 扫雷棋盘格。
///
/// 线格式是字符串或 0..=8 的整数，因此手写 serde：`serde` 的 untagged 无法表达
/// "字符串标记或数字"这一种混合类型而不引入额外包装。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinesCell {
    /// 未揭示。
    Covered,
    /// 被 agent 标旗。**不代表真的有雷**（§11.1）。
    Flagged,
    /// 本次已公开的踩雷格。运行中不允许出现。
    Detonated,
    /// 已揭示，邻雷数 0..=8。
    Adjacent(u8),
}

impl Serialize for MinesCell {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Covered => serializer.serialize_str("covered"),
            Self::Flagged => serializer.serialize_str("flagged"),
            Self::Detonated => serializer.serialize_str("detonated"),
            Self::Adjacent(count) => serializer.serialize_u8(*count),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MinesCellRepr {
    Label(String),
    Count(u8),
}

impl<'de> Deserialize<'de> for MinesCell {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match MinesCellRepr::deserialize(deserializer)? {
            MinesCellRepr::Label(label) => match label.as_str() {
                "covered" => Ok(Self::Covered),
                "flagged" => Ok(Self::Flagged),
                "detonated" => Ok(Self::Detonated),
                other => Err(serde::de::Error::custom(format!(
                    "未知的格子标记 {other:?}；只允许 covered / flagged / detonated"
                ))),
            },
            MinesCellRepr::Count(count) => {
                if count <= 8 {
                    Ok(Self::Adjacent(count))
                } else {
                    Err(serde::de::Error::custom(format!(
                        "邻雷数 {count} 超出 0..=8"
                    )))
                }
            }
        }
    }
}

impl MinesCell {
    /// 是否是已揭示的数字格。
    pub fn is_revealed(self) -> bool {
        matches!(self, Self::Adjacent(_))
    }
}

/// 扫雷符号棋盘（§11.1）。不包含雷位。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinesView {
    /// 固定为 `symbolic_mines`。
    pub mode: MinesMode,
    /// 宽。
    pub width: u16,
    /// 高。
    pub height: u16,
    /// 总雷数。
    pub total_mines: u16,
    /// `board[row][column]`，左上角为 0/0。
    pub board: Vec<Vec<MinesCell>>,
}

/// 像素感知（§12）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PixelView {
    /// 固定为 `pixels`。
    pub mode: PixelMode,
    /// 帧产物标识，只能属于本 episode 的公开域。
    pub frame_artifact_id: PublicId,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
    /// 固定为 RGB8。
    pub pixel_format: PixelFormat,
}

/// 公开感知。三条赛道互斥：像素赛道不附带符号 board 或局部地图（§12）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Percept {
    /// 迷宫符号视图。
    Maze(Box<MazeView>),
    /// 扫雷符号棋盘。
    Mines(Box<MinesView>),
    /// 像素帧。
    Pixels(Box<PixelView>),
}

impl<'de> Deserialize<'de> for Percept {
    /// 按 `mode` 派发，而不是让 `serde` 逐个试。
    ///
    /// untagged 的默认行为是把"字段夹带"报成"没有匹配的变体"，用户拿到这句话无法知道
    /// 自己多写了哪个字段。§11.4 要求非法请求可追溯，所以这里先读 `mode`，再把整个对象交给
    /// 具体类型——错误信息于是来自那个类型本身（例如 `unknown field \`board\``）。
    ///
    /// 本实现经由 `serde_json::Value`，因此只适用于 JSON。协议本身就是 JSON（§11），
    /// 这条限制是有意的。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let mode = value
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::missing_field("mode"))?;

        match mode {
            "symbolic_maze" => serde_json::from_value(value)
                .map(|view| Self::Maze(Box::new(view)))
                .map_err(serde::de::Error::custom),
            "symbolic_mines" => serde_json::from_value(value)
                .map(|view| Self::Mines(Box::new(view)))
                .map_err(serde::de::Error::custom),
            "pixels" => serde_json::from_value(value)
                .map(|view| Self::Pixels(Box::new(view)))
                .map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "未知的感知模式 {other:?}；只允许 symbolic_maze / symbolic_mines / pixels"
            ))),
        }
    }
}

/// 观测消息的类型标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameObservationTag {
    /// 固定值。
    #[serde(rename = "observation")]
    Observation,
}

/// 公开观测。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameObservation {
    /// 协议版本。
    pub protocol_version: u8,
    /// 消息类型。
    #[serde(rename = "type")]
    pub message_type: GameObservationTag,
    /// 会话。
    pub session_id: PublicId,
    /// 回合。
    pub episode_id: PublicId,
    /// 观测标识。
    pub observation_id: PublicId,
    /// 步序。
    pub step_index: u64,
    /// 游戏。
    pub game: GameKind,
    /// 规则是否自然结束。
    pub terminated: bool,
    /// 是否被外部截断。
    pub truncated: bool,
    /// 相位。
    pub outcome: Outcome,
    /// 奖励。
    pub reward: f64,
    /// 感知。
    pub percept: Percept,
}

/// 动作请求的类型标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionRequestTag {
    /// 固定值。
    #[serde(rename = "action_request")]
    ActionRequest,
}

/// 动作请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    /// 协议版本。
    pub protocol_version: u8,
    /// 消息类型。
    #[serde(rename = "type")]
    pub message_type: ActionRequestTag,
    /// 请求标识。幂等键的一部分。
    pub request_id: PublicId,
    /// 回合。
    pub episode_id: PublicId,
    /// 提交者认为的当前观测。过期即拒（§11.2）。
    pub expected_observation_id: PublicId,
    /// 提交动作的认知单元。
    pub actor_id: PublicId,
    /// 拓扑世代。迁移后旧世代动作必须被拒。
    pub topology_epoch: u64,
    /// 执行许可。
    pub permit_id: PublicId,
    /// 动作。
    pub action: GameAction,
}

/// 回执状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    /// 已应用。
    Applied,
    /// 合法但没有改变局面。仍消耗一步。
    NoChange,
    /// 被拒绝，世界未推进。
    Rejected,
    /// 已执行但结局未知。
    UnknownCommit,
}

/// 回执代码。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReceiptCode {
    /// 成功。
    Ok,
    /// 合法无变化。
    NoChange,
    /// 提交者依据的观测已过期。
    StaleObservation,
    /// 拓扑世代已过期。
    StaleTopology,
    /// 回合已结束。
    EpisodeFinished,
    /// 动作不在公开合法集合内。
    InvalidAction,
    /// 许可不足。
    PermissionDenied,
    /// 预算耗尽。
    BudgetExhausted,
    /// 同一请求标识被用于不同载荷。
    IdempotencyConflict,
    /// 引擎不可用。
    EngineUnavailable,
}

impl ReceiptCode {
    /// 该代码允许的回执状态。
    ///
    /// `ENGINE_UNAVAILABLE` 同时允许 `rejected` 与 `unknown_commit`：前者是没跑起来，
    /// 后者是"执行后、回执前崩溃"（§13）。
    pub fn allowed_statuses(self) -> &'static [ReceiptStatus] {
        use ReceiptStatus::{Applied, NoChange, Rejected, UnknownCommit};
        match self {
            Self::Ok => &[Applied],
            Self::NoChange => &[NoChange],
            Self::EngineUnavailable => &[Rejected, UnknownCommit],
            Self::StaleObservation
            | Self::StaleTopology
            | Self::EpisodeFinished
            | Self::InvalidAction
            | Self::PermissionDenied
            | Self::BudgetExhausted
            | Self::IdempotencyConflict => &[Rejected],
        }
    }

    /// 稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::NoChange => "NO_CHANGE",
            Self::StaleObservation => "STALE_OBSERVATION",
            Self::StaleTopology => "STALE_TOPOLOGY",
            Self::EpisodeFinished => "EPISODE_FINISHED",
            Self::InvalidAction => "INVALID_ACTION",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::BudgetExhausted => "BUDGET_EXHAUSTED",
            Self::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            Self::EngineUnavailable => "ENGINE_UNAVAILABLE",
        }
    }
}

/// 动作回执的类型标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameReceiptTag {
    /// 固定值。
    #[serde(rename = "action_receipt")]
    ActionReceipt,
}

/// 动作回执。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameActionReceipt {
    /// 协议版本。
    pub protocol_version: u8,
    /// 消息类型。
    #[serde(rename = "type")]
    pub message_type: GameReceiptTag,
    /// 对应的请求。
    pub request_id: PublicId,
    /// 回合。
    pub episode_id: PublicId,
    /// 状态。
    pub status: ReceiptStatus,
    /// 代码。
    pub code: ReceiptCode,
    /// 新观测。被拒绝或结局未知时为 `null`。
    pub observation_id: Option<PublicId>,
}

// ---------------------------------------------------------------------------
// 语义校验（§11.4）
// ---------------------------------------------------------------------------

/// 语义校验失败。形状合法但运行时条件不成立。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GameProtocolError {
    /// 协议版本不是本实现支持的版本。
    #[error("协议版本不受支持：{actual}（本实现支持 {expected}）")]
    UnsupportedVersion {
        /// 本实现支持的版本。
        expected: u8,
        /// 消息声明的版本。
        actual: u8,
    },

    /// 观测的相位与结束标志不一致。
    #[error("相位 {outcome} 要求 terminated={expected_terminated} truncated={expected_truncated}，实际为 {terminated}/{truncated}")]
    PhaseInconsistent {
        /// 相位。
        outcome: &'static str,
        /// 期望的 `terminated`。
        expected_terminated: bool,
        /// 期望的 `truncated`。
        expected_truncated: bool,
        /// 实际的 `terminated`。
        terminated: bool,
        /// 实际的 `truncated`。
        truncated: bool,
    },

    /// 游戏与感知类型不匹配。
    #[error("游戏 {game} 与感知模式 {percept} 不匹配")]
    GamePerceptMismatch {
        /// 游戏。
        game: &'static str,
        /// 感知模式。
        percept: &'static str,
    },

    /// 视图尺寸非法。
    #[error("{field} 尺寸非法：{detail}")]
    InvalidShape {
        /// 字段名。
        field: &'static str,
        /// 具体原因。
        detail: String,
    },

    /// 棋盘尺寸与声明不一致。
    #[error("board 尺寸 {rows}×{columns:?} 与声明的 {height}×{width} 不一致")]
    BoardSizeMismatch {
        /// 声明的高。
        height: u16,
        /// 声明的宽。
        width: u16,
        /// 实际行数。
        rows: usize,
        /// 各行列数。
        columns: Vec<usize>,
    },

    /// 运行中的局面出现了踩雷格。
    #[error("运行中的扫雷局面出现了 detonated 格")]
    DetonatedWhileRunning,

    /// 总雷数不少于总格数。
    #[error("总雷数 {total_mines} 不小于总格数 {cells}")]
    TooManyMines {
        /// 声明的总雷数。
        total_mines: u16,
        /// 总格数。
        cells: u32,
    },

    /// 坐标越界。
    #[error("坐标 ({row}, {column}) 超出协议上限 {limit}")]
    CoordinateOutOfRange {
        /// 行。
        row: u8,
        /// 列。
        column: u8,
        /// 上限。
        limit: u8,
    },

    /// 拓扑世代为零。
    #[error("topology_epoch 必须至少为 1，实际为 0")]
    ZeroEpoch,

    /// 回执状态与代码不配对。
    #[error("代码 {code} 不允许状态 {status}")]
    StatusCodeMismatch {
        /// 代码。
        code: &'static str,
        /// 状态。
        status: &'static str,
    },

    /// 回执缺少应有的新观测。
    #[error("状态 {status} 必须携带新的 observation_id")]
    MissingObservation {
        /// 状态。
        status: &'static str,
    },

    /// 被拒绝或结局未知却携带了新观测。
    #[error("状态 {status} 不得携带新的 observation_id")]
    UnexpectedObservation {
        /// 状态。
        status: &'static str,
    },
}

impl From<GameProtocolError> for ContractError {
    fn from(error: GameProtocolError) -> Self {
        ContractError::GameProtocol(error.to_string())
    }
}

fn check_version(version: u8) -> Result<(), GameProtocolError> {
    if version != GAME_PROTOCOL_VERSION {
        return Err(GameProtocolError::UnsupportedVersion {
            expected: GAME_PROTOCOL_VERSION,
            actual: version,
        });
    }
    Ok(())
}

impl GameObservation {
    /// 语义校验（§11.4）。
    pub fn validate_semantics(&self) -> Result<(), GameProtocolError> {
        check_version(self.protocol_version)?;

        // 相位一致性：running 要求两个结束标志都为假；终局相位各自要求对应的标志为真。
        let expected = (self.outcome.requires_terminated(), self.outcome.requires_truncated());
        if (self.terminated, self.truncated) != expected {
            return Err(GameProtocolError::PhaseInconsistent {
                outcome: self.outcome.as_str(),
                expected_terminated: expected.0,
                expected_truncated: expected.1,
                terminated: self.terminated,
                truncated: self.truncated,
            });
        }

        match (&self.game, &self.percept) {
            (GameKind::Maze, Percept::Maze(view)) => view.validate_semantics()?,
            (GameKind::Minesweeper, Percept::Mines(view)) => {
                view.validate_semantics(self.outcome)?;
            }
            (GameKind::Maze | GameKind::Minesweeper, Percept::Pixels(view)) => {
                view.validate_semantics()?;
            }
            (game, percept) => {
                return Err(GameProtocolError::GamePerceptMismatch {
                    game: game.as_str(),
                    percept: percept.mode_name(),
                });
            }
        }
        Ok(())
    }
}

impl Percept {
    /// 感知模式名称，用于错误信息。
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Maze(_) => "symbolic_maze",
            Self::Mines(_) => "symbolic_mines",
            Self::Pixels(_) => "pixels",
        }
    }
}

impl MazeView {
    /// 语义校验：矩形视图、尺寸在范围内、朝向合法。
    pub fn validate_semantics(&self) -> Result<(), GameProtocolError> {
        if self.direction > 3 {
            return Err(GameProtocolError::InvalidShape {
                field: "maze.direction",
                detail: format!("朝向必须是 0..=3，实际为 {}", self.direction),
            });
        }
        if self.mission.len() > 2048 {
            return Err(GameProtocolError::InvalidShape {
                field: "maze.mission",
                detail: format!("任务描述 {} 字节超出 2048", self.mission.len()),
            });
        }
        if self.view.is_empty() || self.view.len() > MAZE_VIEW_MAX {
            return Err(GameProtocolError::InvalidShape {
                field: "maze.view",
                detail: format!("行数 {} 不在 1..={MAZE_VIEW_MAX} 内", self.view.len()),
            });
        }
        let width = self.view[0].len();
        if width == 0 || width > MAZE_VIEW_MAX {
            return Err(GameProtocolError::InvalidShape {
                field: "maze.view",
                detail: format!("列数 {width} 不在 1..={MAZE_VIEW_MAX} 内"),
            });
        }
        // 视图必须是矩形。非矩形视图意味着遮蔽规则被破坏。
        if let Some(ragged) = self.view.iter().find(|row| row.len() != width) {
            return Err(GameProtocolError::InvalidShape {
                field: "maze.view",
                detail: format!("视图不是矩形：行宽 {} 与首行 {width} 不一致", ragged.len()),
            });
        }
        Ok(())
    }
}

impl MinesView {
    /// 语义校验（§11.4）。
    pub fn validate_semantics(&self, outcome: Outcome) -> Result<(), GameProtocolError> {
        if !(2..=MINES_BOARD_MAX).contains(&self.width)
            || !(2..=MINES_BOARD_MAX).contains(&self.height)
        {
            return Err(GameProtocolError::InvalidShape {
                field: "mines",
                detail: format!(
                    "尺寸 {}×{} 不在 2..={MINES_BOARD_MAX} 内",
                    self.width, self.height
                ),
            });
        }
        if self.total_mines == 0 || self.total_mines > 4095 {
            return Err(GameProtocolError::InvalidShape {
                field: "mines.total_mines",
                detail: format!("总雷数 {} 不在 1..=4095 内", self.total_mines),
            });
        }

        let cells = u32::from(self.width) * u32::from(self.height);
        if u32::from(self.total_mines) >= cells {
            return Err(GameProtocolError::TooManyMines {
                total_mines: self.total_mines,
                cells,
            });
        }

        let columns: Vec<usize> = self.board.iter().map(Vec::len).collect();
        let expected_columns = usize::from(self.width);
        if self.board.len() != usize::from(self.height)
            || columns.iter().any(|count| *count != expected_columns)
        {
            return Err(GameProtocolError::BoardSizeMismatch {
                height: self.height,
                width: self.width,
                rows: self.board.len(),
                columns,
            });
        }

        // `detonated` 只能表示本次已公开的踩雷格，运行中不可出现（§11.1）。
        if outcome == Outcome::Running
            && self
                .board
                .iter()
                .flatten()
                .any(|cell| *cell == MinesCell::Detonated)
        {
            return Err(GameProtocolError::DetonatedWhileRunning);
        }
        Ok(())
    }

    /// 已揭示的格子数。用于判断"全安全格揭开即胜"。
    pub fn revealed_count(&self) -> usize {
        self.board
            .iter()
            .flatten()
            .filter(|cell| cell.is_revealed())
            .count()
    }

    /// 标旗数量。注意标旗正确与否**不是**胜利标准（§10.2）。
    pub fn flagged_count(&self) -> usize {
        self.board
            .iter()
            .flatten()
            .filter(|cell| **cell == MinesCell::Flagged)
            .count()
    }
}

impl PixelView {
    /// 语义校验。
    pub fn validate_semantics(&self) -> Result<(), GameProtocolError> {
        if self.width == 0 || self.width > 4096 || self.height == 0 || self.height > 4096 {
            return Err(GameProtocolError::InvalidShape {
                field: "pixels",
                detail: format!("尺寸 {}×{} 不在 1..=4096 内", self.width, self.height),
            });
        }
        Ok(())
    }
}

impl ActionRequest {
    /// 语义校验（§11.4）。
    pub fn validate_semantics(&self) -> Result<(), GameProtocolError> {
        check_version(self.protocol_version)?;
        if self.topology_epoch == 0 {
            return Err(GameProtocolError::ZeroEpoch);
        }
        if let Some((row, column)) = self.action.coordinates()
            && (row > MAX_COORDINATE || column > MAX_COORDINATE)
        {
            return Err(GameProtocolError::CoordinateOutOfRange {
                row,
                column,
                limit: MAX_COORDINATE,
            });
        }
        Ok(())
    }

    /// 动作是否属于该游戏的动作域。
    pub fn matches_game(&self, game: GameKind) -> bool {
        game.accepts(self.action)
    }
}

impl GameActionReceipt {
    /// 语义校验（§11.3）。
    pub fn validate_semantics(&self) -> Result<(), GameProtocolError> {
        check_version(self.protocol_version)?;

        if !self.code.allowed_statuses().contains(&self.status) {
            return Err(GameProtocolError::StatusCodeMismatch {
                code: self.code.as_str(),
                status: status_name(self.status),
            });
        }

        // §11.3："每个成功 step 返回回执和新的 observation_id，包括状态未变化的已接受动作"。
        // 反之，"协议非法请求不推进世界"，因此拒绝与结局未知不得携带新观测。
        match self.status {
            ReceiptStatus::Applied | ReceiptStatus::NoChange => {
                if self.observation_id.is_none() {
                    return Err(GameProtocolError::MissingObservation {
                        status: status_name(self.status),
                    });
                }
            }
            ReceiptStatus::Rejected | ReceiptStatus::UnknownCommit => {
                if self.observation_id.is_some() {
                    return Err(GameProtocolError::UnexpectedObservation {
                        status: status_name(self.status),
                    });
                }
            }
        }
        Ok(())
    }
}

fn status_name(status: ReceiptStatus) -> &'static str {
    match status {
        ReceiptStatus::Applied => "applied",
        ReceiptStatus::NoChange => "no_change",
        ReceiptStatus::Rejected => "rejected",
        ReceiptStatus::UnknownCommit => "unknown_commit",
    }
}
