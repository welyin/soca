"""传统迷宫：走廊网络、岔路、死胡同，从一角走到中心。

## 这是"关卡"，不是"规则"

MiniGrid 自带的环境里没有这一种：`SimpleCrossing` 是一道墙加一个缺口，
`FourRooms` 是四间房各开一个口——中间都是大片空地。而那两种都被当成"迷宫"用过，
直到有人说"这就是个空荡荡的一大块"。

`MiniGrid-WFC-MazeSimple-v0` 倒是真迷宫，但 **minigrid 3.1.0 的 wheel 里漏了它的
图案文件**（`envs/wfc/patterns/SimpleMaze.png` 不存在），所以它在这个版本里起不来。

于是自己生成。**生成的是关卡，规则一条没写**：走、撞墙、可见域、终局全部仍由
MiniGrid 的 `MiniGridEnv` 执行，这里只决定墙长在哪、起点和终点在哪。
§10.1 禁的是"手写第二套看似相同的规则"，不是"自己摆一张地图"。

## 算法

奇数格子上做**递归回溯**（深度优先 + 随机选邻），得到一棵生成树：任意两格之间恰有
一条通路，于是岔路和死胡同都是自然结果——不是拿随机墙撒出来的。

* 起点在左下角，终点在**正中**（用户要的那一种："常是中心"）。
* 尺寸必须是奇数（墙占据偶数坐标，走廊占据奇数坐标），所以构造时向上取奇。
"""

from __future__ import annotations

import random

import gymnasium as gym
from minigrid.core.grid import Grid
from minigrid.core.mission import MissionSpace
from minigrid.core.world_object import Goal, Wall
from minigrid.minigrid_env import MiniGridEnv

#: 任务文本。`MissionSpace` 要的是一个**函数**（它按需要重新生成字符串），不是一个字符串。
MISSION_SPACE = MissionSpace(lambda: "从起点走到迷宫的终点")

#: 注册名。清单里写的就是它。
ENV_ID = "MiniGrid-ClassicMaze-v0"


def carve(width: int, height: int, seed: int) -> set[tuple[int, int]]:
    """在奇数格子上做递归回溯，返回**墙**的坐标集合。

    之所以返回墙而不是走廊：MiniGrid 的 `Grid` 默认是空的（走廊），要摆的是墙。
    """
    rng = random.Random(seed)
    cells = {(x, y) for x in range(1, width, 2) for y in range(1, height, 2)}
    start = min(cells, key=lambda cell: (cell[0] + cell[1], cell))
    carved = {start}
    stack = [start]
    while stack:
        x, y = stack[-1]
        # 隔一格才是"另一个格子"，中间那一格是它们之间的墙。
        neighbours = [
            (x + dx * 2, y + dy * 2)
            for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1))
            if (x + dx * 2, y + dy * 2) in cells and (x + dx * 2, y + dy * 2) not in carved
        ]
        if not neighbours:
            stack.pop()
            continue
        nx, ny = rng.choice(neighbours)
        carved.add(((x + nx) // 2, (y + ny) // 2))
        carved.add((nx, ny))
        stack.append((nx, ny))
    return {
        (x, y)
        for x in range(1, width - 1)
        for y in range(1, height - 1)
        if (x, y) not in carved
    }


def centre(width: int, height: int) -> tuple[int, int]:
    """终点那一格：正中，并对齐到奇数坐标（走廊只在奇数坐标上）。"""
    x, y = width // 2, height // 2
    if width % 2 == 0:
        x -= 1
    if height % 2 == 0:
        y -= 1
    return max(1, x), max(1, y)


class ClassicMazeEnv(MiniGridEnv):
    """传统迷宫。尺寸取奇数，起点一角，终点正中。"""

    def __init__(self, size: int = 15, max_steps: int | None = None, **kwargs):
        size = size if size % 2 == 1 else size + 1
        self._requested_size = size
        super().__init__(
            mission_space=MISSION_SPACE,
            width=size,
            height=size,
            # 上限按"每一格都走过一遍"给：这是**规则**给的上限，不是拍出来的。
            max_steps=max_steps or size * size * 4,
            see_through_walls=False,
            **kwargs,
        )

    def _gen_grid(self, width, height):
        self.grid = Grid(width, height)
        self.grid.wall_rect(0, 0, width, height)
        # `self._rand_int` 由 `MiniGridEnv` 提供，它跟着 episode 的种子走——
        # 于是"同一 seed 同一局"这条性质由引擎保证，而不是这里自己再存一份。
        # `int(...)` 是必要的：`_rand_int` 给的是 numpy 整数，而 `random.Random`
        # 只收 Python 的 int/float/str/bytes——它会在构造时抛 TypeError。
        seed = int(self._rand_int(0, 2**31 - 1))
        for x, y in carve(width, height, seed):
            self.grid.set(x, y, Wall())

        goal_x, goal_y = centre(width, height)
        self.put_obj(Goal(), goal_x, goal_y)

        self.agent_pos = (1, 1)
        self.agent_dir = 0
        self.mission = "从起点走到迷宫的终点"


def register() -> str:
    """把这一关注册进 Gymnasium，返回环境 id。**驱动在 `gym.make` 之前调它。**

    幂等：同一个进程里被 import 两次也不会重复注册（Gymnasium 对重复 id 会抛）。
    """
    if ENV_ID not in gym.envs.registry:
        gym.register(id=ENV_ID, entry_point=ClassicMazeEnv)
    return ENV_ID


def _self_check() -> int:
    """跑一遍并把地图打出来。**看得见的证据，不是"应该没问题"。**"""
    env = ClassicMazeEnv(size=15)
    observation, _ = env.reset(seed=7)
    grid = env.unwrapped.grid
    walkable = lambda x, y: (grid.get(x, y) is None) or (grid.get(x, y).type != "wall")
    dead_ends = sum(
        1
        for x in range(grid.width)
        for y in range(grid.height)
        if walkable(x, y)
        and sum(
            1
            for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1))
            if 0 <= x + dx < grid.width
            and 0 <= y + dy < grid.height
            and walkable(x + dx, y + dy)
        )
        == 1
    )
    agent = tuple(env.unwrapped.agent_pos)
    print(f"尺寸 {grid.width}x{grid.height}　起点 {agent}　终点 {centre(grid.width, grid.height)}")
    print(f"死胡同 {dead_ends} 个　视图 {observation['image'].shape}")
    for y in range(grid.height - 1, -1, -1):
        row = ""
        for x in range(grid.width):
            value = grid.get(x, y)
            if agent == (x, y):
                row += "A "
            elif value is None:
                row += "  "
            elif value.type == "wall":
                row += "##"
            elif value.type == "goal":
                row += "GG"
            else:
                row += "  "
        print("   ", row)
    env.close()
    assert observation["image"].shape[0] == 7, "视图尺寸应当仍是清单里那 7"
    assert dead_ends >= 4, f"死胡同只有 {dead_ends} 个——这不像一张迷宫，像是空地"
    print("自检通过：有走廊、有岔路、有死胡同。")
    return 0


#: **import 即注册。** 驱动只负责 import 这个模块，不再多调一件事——
#: "注册"是这一层自己的事，多一个调用点就多一个会忘记的地方。
register()


if __name__ == "__main__":
    raise SystemExit(_self_check())
