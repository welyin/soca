//! 迷宫探索：把规则引擎接进来，并把"它怎么走的"逐步记下来（§10、§17 的"认知游戏"）。
//!
//! §17 那一行是：
//!
//! > 迷宫**局部视野**、扫雷公开 board、**无 seed/真值泄露**、**终局/截断区分**、**幂等 step**；
//! > 结构化与像素成绩分开。
//!
//! 前四样由 `games/maze/game.py`（投影）与 `soca-game-host`（幂等账、终局与截断）负责。
//! 本模块补的是**中间那一段**：谁来走、怎么决定下一步、以及那些决定怎么被看见。
//!
//! ## 探索器不是"一个会玩迷宫的模型"
//!
//! 它是一台**确定性的**探索器：维护一张从局部视图拼出来的地图，按固定的优先级挑目标，
//! 用已知通路走过去。这样做的理由不是省事，是**可看**——每一步都有一个能写下来的理由
//! （"手里有钥匙，去开门"／"这里没路，换一个最近的未知边界"），而"模型说走这边"
//! 写不出这样的句子。§17 要的是把成绩与像素分开报，而这一版连成绩都还没有：
//! 它要的是**过程可见**。
//!
//! ## 地图是靠约定的里程计拼出来的
//!
//! 视图随朝向旋转，agent 恒在 `(3, 6)`，前方是**列号变小**——这条约定是规则事实，
//! 由 `games/maze/tests/test_rules.py` 的位移测试守着。本模块按它把每一格投到世界坐标上。
//!
//! 转置或左右搞反**不会报错**：地图会整体镜像，而它每一步都"看起来对"。所以这里做了一件
//! 额外的事：**每次吸收视图时对照已知道的格子**，一旦同一个世界格子出现两种不同的内容，
//! 就记一次"矛盾"。矛盾数是这套约定的自检——它是 0，约定的方向才算被这次行走验证过。
//!
//! ## 位置是算出来的，朝向是读来的
//!
//! 朝向直接取观测里的 `direction`：它是公开的，而且每步都新鲜，没有任何理由去推算它。
//! 位置则只能算——公开面里**没有**绝对坐标，而那样正是 §11.1 要的。
//! "这一步到底动没动"通过**视图变没变**来判断，而不是假定前进一定成功：撞墙也是一步，
//! 而且它必须消耗一步（§10.1 的原话）。

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use soca_contracts::{
    ActionRequest, ActionRequestTag, GameAction, GameKind, GameObservation, MazeCell, MazeObject,
    MoveAction, MoveOp, Percept, PublicId, WallClock, GAME_PROTOCOL_VERSION,
};
use soca_game_host::{GameHost, HostConfig, ProcessEngineConfig, ProcessFactory};
use soca_storage::Store;

use crate::error::CoreError;

/// agent 在视图里恒定所在的格子（`manifest.json` 的 `view_convention`）。
const AGENT_ROW: i32 = 3;
/// 同上。
const AGENT_COLUMN: i32 = 6;

/// 适配器脚本。
///
/// 从 `CARGO_MANIFEST_DIR` 推，而不是写一个相对路径：相对路径要看进程的工作目录，
/// 而那个东西在 `cargo test`、控制台、以及将来某个打包好的可执行文件里各不相同。
/// 用编译期常量把"仓库里那个文件"钉死，出问题就只可能是文件真的不在。
fn adapter_path() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    root.join("games").join("maze").join("game.py").display().to_string()
}

/// 一次探索里的一步。给页面逐步回放用。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MazeStep {
    /// 第几步（从 1 起）。
    pub index: u64,
    /// 做了什么。
    pub action: String,
    /// **为什么走这一步。** 这一栏是给看的：探索器的每个决定都是确定性的，
    /// 而一个写不出理由的确定性决定，读的人只能把它当成随机。
    pub reason: String,
    /// 回执状态（`applied` / `no_change` / `rejected`）。
    pub receipt: String,
    /// 这一次动作之后引擎给的相位。
    pub outcome: String,
    /// 探索器算出来的世界坐标。**它不进公开面**，只给页面上那张地图用。
    pub position: (i32, i32),
    /// 朝向（来自观测，0..=3）。
    pub direction: u8,
    /// 到这一步为止认得的格子数。
    pub known_cells: usize,
    /// 这一步的公开局部视图。
    pub view: Vec<Vec<MazeCell>>,
}

/// 一次探索的完整记录。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MazeRun {
    /// 回合标识。
    pub episode_id: String,
    /// 用的种子。**它属于私有控制面**——回放要靠它，而公开面上没有它。
    pub seed: u64,
    /// 终局相位。
    pub outcome: String,
    /// 是不是被外部截断（而不是规则自然结束）。
    pub truncated: bool,
    /// 走了多少步。
    pub steps: Vec<MazeStep>,
    /// 最后拼出来的地图：世界坐标 → 已知内容。给页面画全局图用。
    pub map: Vec<MappedCell>,
    /// **同一个格子出现过两种内容的次数。**
    ///
    /// 它应当是 0。不是 0 就说明视图约定被读错了（镜像或转置），而那件事不会以别的方式
    /// 报错——地图会静悄悄地整体翻转，每一步都"看起来对"。
    pub contradictions: usize,
    /// 任务描述（引擎给的公开文本）。
    pub mission: String,
}

/// 地图上的一格。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MappedCell {
    /// 世界坐标。
    pub at: (i32, i32),
    /// 对象。
    pub object: String,
    /// 颜色。
    pub color: String,
    /// 开关状态。
    pub state: String,
    /// 探索器走没走过这里。
    pub visited: bool,
}

impl MazeRun {
    /// 是不是赢了。
    pub fn won(&self) -> bool {
        self.outcome == "won"
    }
}

/// 探索器。
struct Explorer {
    map: BTreeMap<(i32, i32), (String, String, String)>,
    visited: BTreeSet<(i32, i32)>,
    position: (i32, i32),
    carrying: String,
    /// 待执行的计划（转向与前进）。
    plan: VecDeque<GameAction>,
    /// 上一步看到的视图，用来判断"那一步到底动没动"。
    last_view: Option<Vec<Vec<MazeCell>>>,
    contradictions: usize,
}

/// 一个目标，以及到了之后要做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// 走到某一格。
    Walk((i32, i32)),
    /// 走到某一格，然后拾取。
    Pickup((i32, i32)),
    /// 走到某一格的**前面**，转身面对它，然后 toggle。
    Toggle((i32, i32)),
}

impl Default for Explorer {
    fn default() -> Self {
        Self {
            map: BTreeMap::new(),
            visited: BTreeSet::new(),
            position: (0, 0),
            carrying: "none".to_string(),
            plan: VecDeque::new(),
            last_view: None,
            contradictions: 0,
        }
    }
}

/// 这一类对象**不会自己变**。
///
/// 判据是"这一格会不会因为 agent 做了什么而改变"：钥匙会被拿走、门会被打开、
/// 箱子会被推走，而墙、地板、空地和目标不会。
fn is_static(object: &str) -> bool {
    matches!(object, "wall" | "empty" | "floor" | "goal" | "lava")
}

/// 朝向 → 前方的世界位移。MiniGrid 的 `agent_dir`：0=右 1=下 2=左 3=上。
fn forward_vector(direction: u8) -> (i32, i32) {
    match direction % 4 {
        0 => (1, 0),
        1 => (0, 1),
        2 => (-1, 0),
        _ => (0, -1),
    }
}

/// 朝向 → 左方的世界位移。
///
/// 左 = 前方逆时针转 90°。屏幕坐标里 y 向下，所以面向"右"(+x) 时左边是"上"(−y)。
fn left_vector(direction: u8) -> (i32, i32) {
    let (fx, fy) = forward_vector(direction);
    (fy, -fx)
}

impl Explorer {
    /// 吸收一次公开感知：更新地图、位置与携带物。
    ///
    /// `last_action` 是**刚才那一步做了什么**。位置只在"刚才走的是前进"时挪一格。
    ///
    /// 这里最初写的是"视图变了就说明动了"，而那是个 bug：**转身、拾取、开门都会让视图变**，
    /// 于是每转一次身，位置就凭空挪一格，地图整体碎掉。视图是第一人称的，它对旋转和
    /// 平移都敏感，所以它区分不了这两件事——能区分的只有"我按的是哪个键"。
    fn absorb(&mut self, observation: &GameObservation, last_action: Option<GameAction>) {
        let Percept::Maze(view) = &observation.percept else {
            return;
        };
        // "这一步到底动没动"。
        //
        // 判据是**两个条件同时成立**：刚才走的是前进，**而且**视图变了。
        //
        // * 只看"视图变了"：转身、拾取、开门都会让视图变，于是每转一次身位置就凭空挪一格。
        // * 只看"按的是前进"：撞墙的前进也是一步，而它不移动（§10.1："撞墙也消费一步并
        //   返回真实未移动结果"）。把它当成移动，位置就开始漂。
        //
        // 两个条件合起来只在一种情形下会判错：**在一模一样的地形里前进**——那时视图确实
        // 不变。DoorKey 的房间比 7×7 的视野小，所以这种情况在这里近乎不出现；
        // 而它一旦出现，`contradictions` 会涨起来，不会静悄悄地漂。
        let moved = matches!(
            last_action,
            Some(GameAction::Move(MoveAction {
                op: MoveOp::Forward
            }))
        ) && self.last_view.as_ref().is_none_or(|previous| previous != &view.view);
        if moved {
            let (dx, dy) = forward_vector(view.direction);
            self.position = (self.position.0 + dx, self.position.1 + dy);
        }
        self.visited.insert(self.position);
        self.carrying = match view.carrying {
            soca_contracts::Carrying::None => "none",
            soca_contracts::Carrying::Key => "key",
            soca_contracts::Carrying::Ball => "ball",
            soca_contracts::Carrying::Box => "box",
        }
        .to_string();

        for row in 0..view.view.len() {
            for column in 0..view.view[row].len() {
                if row == AGENT_ROW as usize && column == AGENT_COLUMN as usize {
                    continue;
                }
                let cell = view.view[row][column];
                if cell.object == MazeObject::Unseen {
                    continue;
                }
                let forward = AGENT_COLUMN - column as i32;
                let left = AGENT_ROW - row as i32;
                let (fx, fy) = forward_vector(view.direction);
                let (lx, ly) = left_vector(view.direction);
                let at = (
                    self.position.0 + forward * fx + left * lx,
                    self.position.1 + forward * fy + left * ly,
                );
                let value = (
                    format!("{:?}", cell.object).to_lowercase(),
                    format!("{:?}", cell.color).to_lowercase(),
                    format!("{:?}", cell.state).to_lowercase(),
                );
                // **每次都覆盖**，而不是"已知的就不动"。
                //
                // 这条起初写成了写一次：出发点是"先看见的为准"。代价是**地图永远停在过去**——
                // 门开了它还记着 `closed`，于是探索器一遍遍地回去开门（真的会一直开下去），
                // 而钥匙拿走之后它还记着那儿有把钥匙。最新的一次观测永远更准，这是没有例外的。
                //
                // 覆盖带来的问题只有一个：怎么还发现得了"约定读错了"。答案是**静态几何**：
                // 墙、地板、空地不会因为 agent 做了什么而改变，所以它们对不上就是镜像或转置。
                if let Some(known) = self.map.get(&at)
                    && known != &value
                    && is_static(&known.0)
                    && is_static(&value.0)
                {
                    self.contradictions += 1;
                }
                self.map.insert(at, value);
            }
        }
        self.last_view = Some(view.view.clone());
    }

    /// 选下一步，并说明为什么。
    fn decide(&mut self, direction: u8) -> (GameAction, String) {
        if let Some(next) = self.plan.pop_front() {
            return (next, "沿着算好的路线继续走".to_string());
        }

        let (target, reason) = self.choose_target();
        let plan = self.plan_towards(target, direction);
        if plan.is_empty() {
            // 算不出路：这在"只认地图"的探索器里是正常的（目标在未知区域后面）。
            // 落到探索上，而不是原地打转。
            let plan = self.plan_towards(Target::Walk(self.nearest_frontier()), direction);
            self.plan = plan.into();
            return (
                self.plan.pop_front().unwrap_or(GameAction::Move(MoveAction {
                    op: MoveOp::TurnLeft,
                })),
                format!("{reason}；但算不出通路，改为朝最近的未知边界推进"),
            );
        }
        self.plan = plan.into();
        (
            self.plan.pop_front().unwrap_or(GameAction::Move(MoveAction {
                op: MoveOp::TurnLeft,
            })),
            reason,
        )
    }

    /// 按固定优先级挑一个目标。**这段顺序就是"它怎么探索"**，所以每一步都带理由。
    fn choose_target(&mut self) -> (Target, String) {
        let goal = self.find_object(MazeObject::Goal);
        let door = self.find_object(MazeObject::Door);
        let key = self.find_object(MazeObject::Key);

        // 一、门开着、目标也知道 → 直奔目标。
        if let Some(at) = goal {
            let door_open = door
                .and_then(|door_at| self.map.get(&door_at))
                .is_some_and(|(_, _, state)| state == "open");
            let door_known = door.is_some();
            if door_open || !door_known {
                return (Target::Walk(at), "目标已知，门也开着，直奔目标".to_string());
            }
        }

        // 二、手里有钥匙、门也认得 → 去开门。
        if self.carrying == "key"
            && let Some(at) = door
        {
            return (
                Target::Toggle(at),
                "手里有钥匙，门的位置也认得，去开门".to_string(),
            );
        }

        // 三、知道钥匙在哪、还没拿到 → 去拿。
        if self.carrying != "key"
            && let Some(at) = key
        {
            return (
                Target::Pickup(at),
                "还差钥匙，而钥匙的位置已经看见了".to_string(),
            );
        }

        // 四、什么都不知道 → 去地图边缘最近的那个未知。
        let at = self.nearest_frontier();
        (
            Target::Walk(at),
            "地图上还有没看见过的地方，朝最近的那一处走".to_string(),
        )
    }

    /// 地图上某个对象出现的位置（取字典序最小的那个，保证确定性）。
    fn find_object(&self, object: MazeObject) -> Option<(i32, i32)> {
        let name = format!("{object:?}").to_lowercase();
        self.map
            .iter()
            .find(|(_, value)| value.0 == name)
            .map(|(at, _)| *at)
    }

    /// 最靠近 agent 的"未知边界"：一个已知可走、且旁边还有未知的格子。
    fn nearest_frontier(&self) -> (i32, i32) {
        let known_walkable = |at: &(i32, i32)| {
            self.map.get(at).is_some_and(|(object, _, _)| {
                !matches!(object.as_str(), "wall" | "unseen")
            })
        };
        let has_unknown_neighbour = |at: &(i32, i32)| {
            [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dy)| !self.map.contains_key(&(at.0 + dx, at.1 + dy)))
        };

        let mut best: Option<((i32, i32), i32)> = None;
        for at in self.map.keys() {
            if !known_walkable(at) || !has_unknown_neighbour(at) {
                continue;
            }
            let distance = (at.0 - self.position.0).abs() + (at.1 - self.position.1).abs();
            match best {
                Some((_, current)) if current <= distance => {}
                _ => best = Some((*at, distance)),
            }
        }
        best.map(|(at, _)| at).unwrap_or(self.position)
    }

    /// 从当前位置到目标的一串动作。走不通时返回空。
    ///
    /// ## 拾取与开门都不是"走过去"
    ///
    /// MiniGrid 的 `pickup` 与 `toggle` 都作用在**面前那一格**，而且钥匙、门都**走不上去**
    /// （它们不 `can_overlap`）。所以这两件事的正确计划是同一个形状：
    /// **站到它的四邻、转身面对它、然后按键**——而不是"走到它的位置上"。
    ///
    /// 这条起初写错了：`Pickup` 规划的是"走到钥匙那一格再拾取"。它之所以还能跑通，
    /// 是因为撞上去的那一步恰好把 agent 停在了钥匙面前——**运气，不是计划**。
    /// 钥匙离得远一点，那条路就会是"撞两次墙，然后在两格外按键"。
    fn plan_towards(&self, target: Target, direction: u8) -> Vec<GameAction> {
        let goal = match target {
            Target::Walk(at) => at,
            Target::Pickup(at) | Target::Toggle(at) => {
                let op = if matches!(target, Target::Pickup(_)) {
                    MoveOp::Pickup
                } else {
                    MoveOp::Toggle
                };
                let Some((stand, turn_to_face)) = self.stand_next_to(at, direction) else {
                    return Vec::new();
                };
                let mut actions = match self.path_to(stand, direction) {
                    path if path.is_empty() && stand != self.position => return Vec::new(),
                    path => path,
                };
                actions.push(turn_to_face);
                actions.push(GameAction::Move(MoveAction { op }));
                return actions;
            }
        };

        self.path_to(goal, direction)
    }

    /// 四邻里哪个位置能面对 `at`，以及要转成哪个方向。
    fn stand_next_to(&self, at: (i32, i32), direction: u8) -> Option<((i32, i32), GameAction)> {
        let mut best: Option<((i32, i32), GameAction, i32)> = None;
        // 四个站位与对应的朝向：站在西边朝右、站在东边朝左、站在北边朝下、站在南边朝上。
        let candidates = [
            ((at.0 - 1, at.1), 0u8),
            ((at.0 + 1, at.1), 2u8),
            ((at.0, at.1 - 1), 1u8),
            ((at.0, at.1 + 1), 3u8),
        ];
        for (stand, face) in candidates {
            let walkable = self
                .map
                .get(&stand)
                .is_some_and(|(object, _, _)| !matches!(object.as_str(), "wall" | "unseen"));
            if !walkable && stand != self.position {
                continue;
            }
            let distance = (stand.0 - self.position.0).abs() + (stand.1 - self.position.1).abs();
            if best.as_ref().is_none_or(|(_, _, current)| distance < *current) {
                best = Some((stand, self.turn_actions(direction, face), distance));
            }
        }
        best.map(|(stand, turn, _)| (stand, turn))
    }

    /// 转向动作。0=右 1=下 2=左 3=上，左转是 −1。
    fn turn_actions(&self, from: u8, to: u8) -> GameAction {
        let left_turns = (from + 4 - to) % 4;
        let right_turns = (to + 4 - from) % 4;
        if left_turns <= right_turns {
            GameAction::Move(MoveAction {
                op: MoveOp::TurnLeft,
            })
        } else {
            GameAction::Move(MoveAction {
                op: MoveOp::TurnRight,
            })
        }
    }

    /// 在已知可走的格子上做 BFS，返回一串"转向 + 前进"。
    ///
    /// 走不通就返回空——**不猜**。猜一条穿过未知区域的路，会让"探索"退化成"乱撞"，
    /// 而乱撞的地图恰好看起来一样大。
    fn path_to(&self, goal: (i32, i32), direction: u8) -> Vec<GameAction> {
        if goal == self.position {
            return Vec::new();
        }
        let walkable = |at: &(i32, i32)| {
            // 目标格本身可以站上去（钥匙、目标），其余必须是已知且不是墙。
            if *at == goal {
                return true;
            }
            self.map.get(at).is_some_and(|(object, _, state)| {
                if object == "wall" || object == "unseen" {
                    return false;
                }
                // 关着的门走不过去；开着的可以。
                if object == "door" {
                    return state == "open";
                }
                true
            })
        };

        let mut came_from: BTreeMap<(i32, i32), (i32, i32)> = BTreeMap::new();
        let mut queue: VecDeque<(i32, i32)> = VecDeque::new();
        queue.push_back(self.position);
        let mut seen: BTreeSet<(i32, i32)> = BTreeSet::new();
        seen.insert(self.position);
        let mut found = false;
        while let Some(at) = queue.pop_front() {
            if at == goal {
                found = true;
                break;
            }
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let next = (at.0 + dx, at.1 + dy);
                if seen.contains(&next) || !walkable(&next) {
                    continue;
                }
                seen.insert(next);
                came_from.insert(next, at);
                queue.push_back(next);
            }
        }
        if !found {
            return Vec::new();
        }

        let mut route: Vec<(i32, i32)> = Vec::new();
        let mut cursor = goal;
        while cursor != self.position {
            route.push(cursor);
            match came_from.get(&cursor) {
                Some(previous) => cursor = *previous,
                None => return Vec::new(),
            }
        }
        route.reverse();

        let mut actions: Vec<GameAction> = Vec::new();
        let mut facing = direction;
        let mut at = self.position;
        for next in route {
            let wanted = direction_between(at, next);
            if wanted != facing {
                // 一次左转或右转只挪一格；这里展开成最短的那一串。
                let left = (facing + 4 - wanted) % 4;
                let right = (wanted + 4 - facing) % 4;
                if left <= right {
                    for _ in 0..left {
                        actions.push(GameAction::Move(MoveAction {
                            op: MoveOp::TurnLeft,
                        }));
                    }
                } else {
                    for _ in 0..right {
                        actions.push(GameAction::Move(MoveAction {
                            op: MoveOp::TurnRight,
                        }));
                    }
                }
                facing = wanted;
            }
            actions.push(GameAction::Move(MoveAction {
                op: MoveOp::Forward,
            }));
            at = next;
        }
        actions
    }

    /// 把地图导出成给页面用的列表。
    fn mapped(&self) -> Vec<MappedCell> {
        self.map
            .iter()
            .map(|(at, (object, color, state))| MappedCell {
                at: *at,
                object: object.clone(),
                color: color.clone(),
                state: state.clone(),
                visited: self.visited.contains(at),
            })
            .collect()
    }
}

/// 观测里的朝向。非迷宫感知回 0（本模块只跑迷宫，这条分支不该被走到）。
fn direction_of(observation: &GameObservation) -> u8 {
    match &observation.percept {
        Percept::Maze(view) => view.direction,
        _ => 0,
    }
}

/// 两个相邻格子之间的朝向。
fn direction_between(from: (i32, i32), to: (i32, i32)) -> u8 {
    match (to.0 - from.0, to.1 - from.1) {
        (1, 0) => 0,
        (-1, 0) => 2,
        (0, 1) => 1,
        _ => 3,
    }
}

/// 跑一局迷宫，返回逐步记录（§17 的"认知游戏"那一行）。
///
/// `seed` 属于**私有控制面**：它进引擎，出不去公开面。回放要靠它，所以它出现在返回值里
/// ——而返回值是给操作员的，不是给认知单元的。
pub fn run_episode(
    store: &mut Store,
    seed: u64,
    max_steps: u32,
    at: WallClock,
) -> Result<MazeRun, CoreError> {
    let adapter = adapter_path();
    let factory = ProcessFactory::new(ProcessEngineConfig::new(
        engine_program(),
        &adapter,
        GameKind::Maze,
    ));

    let session_id = PublicId::new("session:maze")?;
    let episode_id = PublicId::new(format!("episode:maze:{seed}"))?;
    let mut host = GameHost::new(store, HostConfig::default());
    let mut observation = host.start_episode(
        session_id.clone(),
        episode_id.clone(),
        GameKind::Maze,
        seed,
        1,
        &factory,
    )?;

    let mut explorer = Explorer::default();
    let mut steps: Vec<MazeStep> = Vec::new();
    let mut mission = String::new();

    for index in 1..=u64::from(max_steps) {
        if observation.terminated || observation.truncated {
            break;
        }
        if let Percept::Maze(view) = &observation.percept {
            mission = view.mission.clone();
        }

        let direction = match &observation.percept {
            Percept::Maze(view) => view.direction,
            _ => 0,
        };
        let (action, reason) = explorer.decide(direction);
        let request = ActionRequest {
            protocol_version: GAME_PROTOCOL_VERSION,
            message_type: ActionRequestTag::ActionRequest,
            request_id: PublicId::new(format!("request:maze:{seed}:{index}"))?,
            episode_id: episode_id.clone(),
            expected_observation_id: observation.observation_id.clone(),
            actor_id: PublicId::new("unit:maze:frontier-1")?,
            topology_epoch: host.epoch(&episode_id).unwrap_or(1),
            permit_id: PublicId::new(format!("permit:maze:{seed}:{index}"))?,
            action,
        };
        let receipt = host.submit(&request, at.plus_seconds(index as i64))?;
        observation = host.observe(&episode_id)?;

        explorer.absorb(&observation, Some(action));
        steps.push(MazeStep {
            index,
            action: action.op_name().to_string(),
            reason,
            // 走序列化拿名字，而不是手抄一份 `match`：手抄的那一份会在加取值时悄悄漏掉
            // 一个新分支，而"漏掉"的表现是页面上显示一个空字符串。
            receipt: serde_json::to_value(receipt.status)
                .ok()
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
            outcome: observation.outcome.as_str().to_string(),
            position: explorer.position,
            direction: direction_of(&observation),
            known_cells: explorer.map.len(),
            view: match &observation.percept {
                Percept::Maze(view) => view.view.clone(),
                _ => Vec::new(),
            },
        });

        if observation.terminated || observation.truncated {
            break;
        }
    }

    // 让宿主连同它的引擎一起落地：`ProcessEngine` 的 `Drop` 会杀掉子进程。
    // 不显式关一下的话，一局一个进程很快就会攒成一片僵尸。
    drop(host);

    Ok(MazeRun {
        episode_id: episode_id.to_string(),
        seed,
        outcome: observation.outcome.as_str().to_string(),
        truncated: observation.truncated,
        steps,
        map: explorer.mapped(),
        contradictions: explorer.contradictions,
        mission,
    })
}

/// 解释器。可以由 `SOCA_PYTHON` 覆盖——虚拟环境与系统解释器不是一回事，
/// 而把它写死会让"在我机器上跑得起来"变成一个不可移植的断言。
fn engine_program() -> std::path::PathBuf {
    std::env::var("SOCA_PYTHON")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("python"))
}
