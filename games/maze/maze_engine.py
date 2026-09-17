"""迷宫规则引擎：MiniGrid DoorKey 的公开面包装（实施规格 §10.1，工单 ENG-07）。

本模块是**私有控制面**的一部分：它持有 seed、真实地图与 RNG 状态。只有投影之后的公开
感知会离开它。三条投影规则：

1. **只给局部符号视图**：MiniGrid 的 7×7 `image` 原样映射，不补全、不插值。
   引擎遮住的格子就是 unknown，认知单元必须自己记住去过哪里。
2. **不用 `info`**：MiniGrid 的 `info` 可能含隐藏变量，一律不读、不转发。
3. **不暴露绝对位置与完整地图**：`unwrapped.agent_pos`、`unwrapped.grid`、RNG 状态
   都不进入返回结构。

动作映射固定为 §10.1 给的值，**刻意不含** `drop`(4) 与 `done`(6)：它们是引擎自带的、
能绕过任务的通道。
"""

from __future__ import annotations

from typing import Any

import gymnasium as gym
import minigrid  # noqa: F401  必须导入才能注册环境 id

#: 公开的游戏标识，与 Rust 侧 `GameKind::Maze` 对应。
GAME = "maze"

#: 第一阶段固定的任务（§10.1）。
DEFAULT_ENV_ID = "MiniGrid-DoorKey-8x8-v0"

#: 规则版本。改动动作映射、视图尺寸或投影规则都必须提升它；成绩只与同一版本可比。
RULES_VERSION = "maze-door-key-8x8-v1"

#: 公开动作到引擎动作的映射。值取自 MiniGrid 的 `Actions` 枚举。
ACTION_MAP: dict[str, int] = {
    "turn_left": 0,
    "turn_right": 1,
    "forward": 2,
    "pickup": 3,
    "toggle": 5,
}

MISSION_MAX_CHARS = 2048

# MiniGrid 的 `image` 是三通道整数编码。下表把它翻成公开协议的词汇。
_OBJECT = {
    0: "unseen",
    1: "empty",
    2: "wall",
    3: "floor",
    4: "door",
    5: "key",
    6: "ball",
    7: "box",
    8: "goal",
    9: "lava",
    10: "agent",
}
_COLOR = {0: "red", 1: "green", 2: "blue", 3: "purple", 4: "yellow", 5: "grey"}
_STATE = {0: "open", 1: "closed", 2: "locked"}
_CARRYING = {"key": "key", "ball": "ball", "box": "box"}
_DOOR_OBJECT_ID = 4
_UNSEEN_OBJECT_ID = 0


class DomainError(Exception):
    """动作不在本游戏的动作域内，或引擎拒绝执行。"""


class MazeEngine:
    """MiniGrid DoorKey 的包装。

    对外只暴露 `reset` / `step` / `snapshot` / `restore` / `close`，与 Rust 侧 `Engine`
    接口一一对应。
    """

    def __init__(self, env_id: str = DEFAULT_ENV_ID) -> None:
        self.env_id = env_id
        self._env = gym.make(env_id)
        self._observation: dict[str, Any] | None = None
        self._steps = 0
        self._reward = 0.0
        self._terminated = False
        self._truncated = False

    # ---- 私有控制面 --------------------------------------------------------

    def reset(self, seed: int) -> dict[str, Any]:
        """重置到由 seed 决定的局面，返回公开的初始感知。

        `seed` 只在这里使用，绝不进入返回值。
        """
        observation, _info = self._env.reset(seed=int(seed))
        self._observation = observation
        self._steps = 0
        self._reward = 0.0
        self._terminated = False
        self._truncated = False
        return self._project(outcome="running")

    def step(self, action: dict[str, Any]) -> dict[str, Any]:
        """执行一步。`action` 是公开协议里的动作对象。"""
        if self._terminated or self._truncated:
            raise DomainError("回合已经结束")

        operation = action.get("op")
        if not isinstance(operation, str) or operation not in ACTION_MAP:
            # drop / done / 未知操作都在这里被挡住：它们根本不在公开动作集合里。
            raise DomainError(f"动作 {operation!r} 不在迷宫公开动作集合内")

        observation, reward, terminated, truncated, _info = self._env.step(
            ACTION_MAP[operation]
        )
        self._observation = observation
        self._steps += 1
        self._reward = float(reward)
        self._terminated = bool(terminated)
        self._truncated = bool(truncated)

        if self._terminated:
            # DoorKey 只在抵达目标或出错时自然结束，只有前者给正奖励。
            outcome = "won" if self._reward > 0 else "lost"
        elif self._truncated:
            outcome = "timeout"
        else:
            outcome = "running"
        return self._project(outcome=outcome)

    def snapshot(self) -> str:
        """私有 checkpoint（base64 的 npz）。仅在引擎确实可恢复时才对外承诺。"""
        import base64
        import io

        buffer = io.BytesIO()
        import numpy as np

        np.savez_compressed(
            buffer,
            image=self._observation["image"],
            direction=self._observation["direction"],
            steps=self._steps,
            reward=self._reward,
            terminated=self._terminated,
            truncated=self._truncated,
        )
        return base64.b64encode(buffer.getvalue()).decode("ascii")

    def restore(self, checkpoint: str) -> dict[str, Any]:
        """从私有 checkpoint 恢复。

        注意：MiniGrid 的**地图本身**不在这个 checkpoint 里（它只保存了观测）。
        因此本方法恢复的是"公开面 + 计数"，不足以在崩溃后继续玩同一局。
        引擎的 `supports_snapshot` 因此为假——§13 要求未验证可恢复的适配器
        把回合标为基础设施截断，而不是假装能接着玩。
        """
        raise DomainError("MiniGrid 适配器未实现完整局面恢复；不得据此继续同一回合")

    def close(self) -> None:
        self._env.close()

    def manifest_facts(self) -> dict[str, Any]:
        """引擎实测出来的事实，供测试与 manifest 核对。"""
        return {
            "env_id": self.env_id,
            "max_steps": int(self._env.unwrapped.max_steps),
            "view_size": int(self._env.unwrapped.agent_view_size),
            "action_ids": dict(ACTION_MAP),
        }

    # ---- 投影 --------------------------------------------------------------

    def _project(self, outcome: str) -> dict[str, Any]:
        """把引擎观测投影成公开感知，并附上相位。"""
        observation = self._observation
        if observation is None:
            raise DomainError("尚未 reset")

        image = observation["image"]
        view: list[list[dict[str, str]]] = []
        for row in range(len(image)):
            cells: list[dict[str, str]] = []
            for column in range(len(image[row])):
                object_id, color_id, state_id = (int(value) for value in image[row][column])
                if object_id == _UNSEEN_OBJECT_ID:
                    # 引擎看不到的格子：颜色与状态字段没有意义，必须整体保持 unknown。
                    # 照抄 ch1/ch2 会把"未观测"读成"红色的开着的东西"。
                    cells.append({"object": "unseen", "color": "none", "state": "none"})
                    continue
                cells.append(
                    {
                        "object": _OBJECT.get(object_id, "unseen"),
                        "color": _COLOR.get(color_id, "none"),
                        # 只有门才有有意义的开关状态：其余对象的 ch2 恒为 0，
                        # 直接映射会读成"门开着"。
                        "state": _STATE.get(state_id, "none")
                        if object_id == _DOOR_OBJECT_ID
                        else "none",
                    }
                )
            view.append(cells)

        return {
            "percept": {
                "mode": "symbolic_maze",
                "mission": str(observation["mission"])[:MISSION_MAX_CHARS],
                "direction": int(observation["direction"]),
                "view": view,
                "carrying": self._carrying(),
            },
            "terminated": self._terminated,
            "truncated": self._truncated,
            "outcome": outcome,
            "reward": self._reward if self._terminated else 0.0,
        }

    def _carrying(self) -> str:
        carried = self._env.unwrapped.carrying
        if carried is None:
            return "none"
        return _CARRYING.get(getattr(carried, "type", ""), "none")
