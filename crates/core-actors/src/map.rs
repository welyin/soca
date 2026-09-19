//! 知识地图：认知单元在迷宫里积累的那份"我知道什么"（工单 ENG-06）。
//!
//! 这里是"学习"真正发生的地方。地图必须做到两件事，否则整个演示就只是随机游走：
//!
//! 1. **无漂移的自我定位**。地图以**起点为原点、初始朝向为基准**建立自我中心锚定坐标系，
//!    而不是依赖绝对坐标——迷宫规则不公开绝对位置。之所以不会漂移，是因为前进是否成功
//!    可以从"前方格是不是墙"直接预判，不需要靠碰撞事后纠偏。
//! 2. **区分"已知"与"未知"**。`unseen` 只是"我没看见过"，绝不等于"那里是空的"。
//!    把两者混起来，agent 会一头撞进没见过的墙，而且看上去像是"学会了"。
//!
//! 坐标约定（与游戏清单里的 `view_convention` 一致，见 `games/door-key/manifest.json`）：
//!
//! * 视图里 agent 自己在 `(row = 3, col = 6)`；
//! * 前进一格 → 列号减 1，因此 `forward 偏移 = 6 - col`；
//! * agent 的左手 → 行号减 1，因此 `横向偏移 = row - 3`，正值表示在右手边。
//!
//! 世界坐标用 `(dx, dy)`，`dx` 向右（初始朝向的右手边）、`dy` 向前。

use std::collections::{BTreeMap, VecDeque};

use soca_contracts::{Carrying, MazeCell, MazeCellState, MazeColor, MazeObject, MazeView};

/// agent 在视图里的位置：MiniGrid 把它放在"最后一列的中点"（见清单的 `view_convention`）。
///
/// **从视图形状算，不是常数。** 视图多大是**每个游戏自己声明的**（门钥匙 7×7、
/// 传统迷宫 3×3），而写死 (3,6) 只对 7×7 成立——换一个尺寸之后每一格会被投到错的世界坐标上，
/// 地图安静地整片歪掉，而每一步都"看着对"。7×7 时它算出 (3,6)，与从前逐字相同。
pub fn agent_cell(size: usize) -> (i32, i32) {
    ((size / 2) as i32, (size as i32) - 1)
}

/// 世界坐标：起点为原点，`dy` 为初始朝向，`dx` 为初始朝向的右手边。
pub type Cell = (i32, i32);

/// 地图里一格的知识。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownCell {
    /// 看见的对象。
    pub object: MazeObject,
    /// 颜色。
    pub color: MazeColor,
    /// 开关状态。
    pub state: MazeCellState,
    /// 第一次看见的行动序号。
    pub first_seen_step: u64,
    /// 最近一次看见的行动序号。
    pub last_seen_step: u64,
    /// 被看见次数。**不**用来提升置信度：同一来源重复观测不算独立证据。
    pub sightings: u32,
}

impl KnownCell {
    /// 从视图格建立初次记录。
    fn new(cell: &MazeCell, step: u64) -> Self {
        Self {
            object: cell.object,
            color: cell.color,
            state: cell.state,
            first_seen_step: step,
            last_seen_step: step,
            sightings: 1,
        }
    }

    /// 用新的一次观测更新。重复观测只刷新最近时间与计数。
    fn update(&mut self, cell: &MazeCell, step: u64) {
        self.object = cell.object;
        self.color = cell.color;
        self.state = cell.state;
        self.last_seen_step = step;
        self.sightings = self.sightings.saturating_add(1);
    }

    /// 这一格能否走进去。
    ///
    /// 门只有在开着的时候才算可通行；锁着的门要走进去得先拿到钥匙并 toggle。
    pub fn is_passable(&self) -> bool {
        match self.object {
            MazeObject::Empty | MazeObject::Floor | MazeObject::Goal => true,
            MazeObject::Door => self.state == MazeCellState::Open,
            _ => false,
        }
    }

    /// 这一格是"边界格"的候选：已经知道可走，但它旁边还有没见过的格子。
    pub fn is_goal(&self) -> bool {
        matches!(self.object, MazeObject::Goal)
    }
}

/// 一次观测给地图带来的变化。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MapUpdate {
    /// 这次新记录下来的格子数。
    pub discovered: usize,
    /// 这次刷新了既有记录的格子数。
    pub refreshed: usize,
    /// 这次发现目标格了吗。
    pub goal_seen: bool,
    /// 这次发现有变化的既有格（例如门从关着变成开着）。
    pub changed: Vec<Cell>,
}

/// 知识地图。
#[derive(Clone, Debug)]
pub struct KnowledgeMap {
    cells: BTreeMap<Cell, KnownCell>,
    position: Cell,
    heading: u8,
    step: u64,
    /// 手里拿着什么。地图不解释它，只在规划时用来判断能不能开门。
    carrying: Carrying,
}

impl Default for KnowledgeMap {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeMap {
    /// 一张空地图。起点即原点，朝向记为 0（"初始朝向"）。
    ///
    /// 起点会被直接记为已知可走：agent 此刻就站在那里，这是它能有的最基本的观测。
    /// 注意这只是"知道自己站得住"，不是"看清了这一格是什么"，因此物件类型记为地板。
    pub fn new() -> Self {
        let mut map = Self {
            cells: BTreeMap::new(),
            position: (0, 0),
            heading: 0,
            step: 0,
            carrying: Carrying::None,
        };
        map.mark_standable((0, 0));
        map
    }

    /// 当前自我定位。
    pub fn position(&self) -> Cell {
        self.position
    }

    /// 当前朝向，0..=3，0 表示初始朝向。
    pub fn heading(&self) -> u8 {
        self.heading
    }

    /// 已经见过的格子数。
    pub fn known_count(&self) -> usize {
        self.cells.len()
    }

    /// 读一格的知识。返回 `None` 表示没见过——**不是**"那里是空的"。
    pub fn known(&self, cell: Cell) -> Option<&KnownCell> {
        self.cells.get(&cell)
    }

    /// 当前携带物。
    pub fn carrying(&self) -> Carrying {
        self.carrying
    }

    /// 把所有已知格按坐标列出，供 UI 与快照使用。
    pub fn iter(&self) -> impl Iterator<Item = (Cell, &KnownCell)> {
        self.cells.iter().map(|(cell, known)| (*cell, known))
    }

    /// 地图的边界框（最小、最大坐标）。空地图返回 `None`。
    pub fn bounds(&self) -> Option<(Cell, Cell)> {
        let mut keys = self.cells.keys();
        let first = *keys.next()?;
        let mut min = first;
        let mut max = first;
        for cell in keys {
            min = (min.0.min(cell.0), min.1.min(cell.1));
            max = (max.0.max(cell.0), max.1.max(cell.1));
        }
        Some((min, max))
    }

    /// 用一次公开感知更新地图。
    ///
    /// 关键一步是**只记录看得见的格子**：`unseen` 一律跳过。把 `unseen` 记成未知格而不是
    /// 空格的替代品，是这张地图唯一的正确性来源。
    pub fn observe(&mut self, view: &MazeView, carrying: Carrying) -> MapUpdate {
        self.carrying = carrying;
        let mut update = MapUpdate::default();
        // agent 在视图里的那一格**从形状算**，不写死。
        let (agent_row, agent_column) = agent_cell(view.view.len());

        for (row_index, row) in view.view.iter().enumerate() {
            for (column_index, cell) in row.iter().enumerate() {
                if cell.object == MazeObject::Unseen {
                    continue;
                }
                let forward = agent_column - column_index as i32;
                let lateral = row_index as i32 - agent_row;
                let target = self.to_world(forward, lateral);

                // agent 自己那一格在视图里是"空"，但它说的不是世界，而是 agent 自己。
                if forward == 0 && lateral == 0 {
                    continue;
                }

                match self.cells.get_mut(&target) {
                    None => {
                        self.cells.insert(target, KnownCell::new(cell, self.step));
                        update.discovered += 1;
                        if cell.object == MazeObject::Goal {
                            update.goal_seen = true;
                        }
                    }
                    Some(existing) => {
                        if existing.object != cell.object || existing.state != cell.state {
                            update.changed.push(target);
                        }
                        existing.update(cell, self.step);
                        update.refreshed += 1;
                    }
                }
            }
        }
        update
    }

    /// 把自己往前挪一格。
    ///
    /// 调用方必须先确认前方可走。**这里不做碰撞检查**：地图不知道世界此刻是什么样，
    /// 它只知道"我以为会发生什么"。真伪由动作后的观测来核对。
    pub fn advance(&mut self) {
        let (dx, dy) = unit_step(self.heading);
        self.position = (self.position.0 + dx, self.position.1 + dy);
        // 站过的格子就是可走的证据。没有这一步，agent 走过的路会因为在身后被遮挡而
        // 从地图上消失，于是它连原路返回都规划不出来——这正是"地图必须有洞"的反面。
        self.mark_standable(self.position);
        self.step = self.step.saturating_add(1);
    }

    /// 把 agent 站过的格子记为已知可走。
    ///
    /// 只在没有记录时插入：如果地图此前已经看清了那一格（例如看见的是钥匙），
    /// 不能用"地板"把它覆盖掉。真正值得报错的相反情况——地图说是墙而 agent 站在上面——
    /// 若出现，说明视图约定写反了，由里程计闭合测试负责暴露。
    fn mark_standable(&mut self, cell: Cell) {
        self.cells.entry(cell).or_insert_with(|| KnownCell {
            object: MazeObject::Floor,
            color: MazeColor::None,
            state: MazeCellState::None,
            first_seen_step: self.step,
            last_seen_step: self.step,
            sightings: 0,
        });
    }

    /// 左转。
    pub fn turn_left(&mut self) {
        self.heading = (self.heading + 3) % 4;
        self.step = self.step.saturating_add(1);
    }

    /// 右转。
    pub fn turn_right(&mut self) {
        self.heading = (self.heading + 1) % 4;
        self.step = self.step.saturating_add(1);
    }

    /// 记一次不改变位置的行动（拾取、开关门），只推进步数。
    pub fn note_action(&mut self) {
        self.step = self.step.saturating_add(1);
    }

    /// 当前行动序号。
    pub fn step(&self) -> u64 {
        self.step
    }

    /// 正前方那一格的世界坐标。
    pub fn ahead(&self) -> Cell {
        let (dx, dy) = unit_step(self.heading);
        (self.position.0 + dx, self.position.1 + dy)
    }

    /// 已知可走、且旁边还有没见过格子的位置。这是探索的目标集合。
    ///
    /// 之所以要"已知可走"而不是"未知"：未知格不构成可到达的目标。真正的探索前沿是
    /// 站在已知地板上往未知处看。
    pub fn frontier(&self) -> Vec<Cell> {
        self.cells
            .iter()
            .filter(|(_, known)| known.is_passable())
            .filter(|(cell, _)| {
                neighbors(**cell).into_iter().any(|neighbor| {
                    !self.cells.contains_key(&neighbor)
                })
            })
            .map(|(cell, _)| *cell)
            .collect()
    }

    /// 已知的目标格位置。
    pub fn goals(&self) -> Vec<Cell> {
        self.cells
            .iter()
            .filter(|(_, known)| known.is_goal())
            .map(|(cell, _)| *cell)
            .collect()
    }

    /// 已知的钥匙位置。
    pub fn keys(&self) -> Vec<Cell> {
        self.cells
            .iter()
            .filter(|(_, known)| matches!(known.object, MazeObject::Key))
            .map(|(cell, _)| *cell)
            .collect()
    }

    /// 在已知可走的格子上做广度优先搜索，返回从当前位置到目标的路径（含目标，不含起点）。
    ///
    /// 只用已知信息：地图不知道的地方不算路。这是"规划依赖积累"的直接体现——地图越全，
    /// 能找到的路越短。
    pub fn path_to(&self, target: Cell) -> Option<Vec<Cell>> {
        if target == self.position {
            return Some(Vec::new());
        }
        if !self.is_passable_for_planning(target) {
            return None;
        }

        let mut came_from: BTreeMap<Cell, Cell> = BTreeMap::new();
        let mut queue: VecDeque<Cell> = VecDeque::new();
        queue.push_back(self.position);
        came_from.insert(self.position, self.position);

        while let Some(current) = queue.pop_front() {
            for neighbor in neighbors(current) {
                if came_from.contains_key(&neighbor) {
                    continue;
                }
                if !self.is_passable_for_planning(neighbor) {
                    continue;
                }
                came_from.insert(neighbor, current);
                if neighbor == target {
                    return Some(reconstruct(&came_from, target));
                }
                queue.push_back(neighbor);
            }
        }
        None
    }

    /// 到目标的曼哈顿距离下界，用于在多个前沿里挑最近的。
    pub fn distance_bound(&self, target: Cell) -> u32 {
        self.position
            .0
            .abs_diff(target.0)
            .saturating_add(self.position.1.abs_diff(target.1))
    }

    fn is_passable_for_planning(&self, cell: Cell) -> bool {
        if cell == self.position {
            return true;
        }
        self.cells.get(&cell).is_some_and(KnownCell::is_passable)
    }

    /// 把 agent 自己坐标系里的 (前进, 横向) 偏移换成世界坐标偏移。
    ///
    /// 朝向 0 面向 +y、右手为 +x；每次右转把整组坐标顺时针转 90°。
    fn to_world(&self, forward: i32, lateral: i32) -> Cell {
        let (dx, dy) = match self.heading {
            0 => (lateral, forward),
            1 => (forward, -lateral),
            2 => (-lateral, -forward),
            _ => (-forward, lateral),
        };
        (self.position.0 + dx, self.position.1 + dy)
    }
}

/// 朝向对应的单位步长。0 为 +y，右转依次顺时针。
fn unit_step(heading: u8) -> (i32, i32) {
    match heading % 4 {
        0 => (0, 1),
        1 => (1, 0),
        2 => (0, -1),
        _ => (-1, 0),
    }
}

fn neighbors(cell: Cell) -> [Cell; 4] {
    [
        (cell.0 + 1, cell.1),
        (cell.0 - 1, cell.1),
        (cell.0, cell.1 + 1),
        (cell.0, cell.1 - 1),
    ]
}

fn reconstruct(came_from: &BTreeMap<Cell, Cell>, target: Cell) -> Vec<Cell> {
    let mut path = vec![target];
    let mut current = target;
    while let Some(previous) = came_from.get(&current) {
        if *previous == current {
            break;
        }
        path.push(*previous);
        current = *previous;
    }
    path.reverse();
    // 去掉起点自身。
    path.remove(0);
    path
}
