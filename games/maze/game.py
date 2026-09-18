"""迷宫规则引擎：MiniGrid DoorKey 的公开面适配器（§10.1、ENG-07）。

本进程是**规则引擎**，不是宿主。§8 把职责切成两半：引擎执行规则，宿主负责幂等账与
下一公开观测。所以这里的输出只有"已投影的公开感知"——隐藏真值（完整地图、绝对坐标、
RNG 状态、`info` 字典、专家动作）在构造感知时就被丢掉，宿主与客户端根本拿不到它们。

## 协议

帧 = 4 字节小端长度 + UTF-8 JSON，与 `crates/game-host/src/process.rs` 的 `write_frame`
一一对应。四类请求：

    {"type": "facts"}                  → {"type": "facts_result", "game": "maze", "rules_version": "..."}
    {"type": "reset", "seed": N}       → {"type": "reset_result", "step": {...}}
    {"type": "step",  "action": {...}} → {"type": "step_result",  "step": {...}}
    {"type": "close"}                  → {"type": "close_result"}

出错回 `{"type": "error", "kind": ..., "reason": ...}`。stderr 被宿主丢弃，所以异常
必须变成一条错误帧——否则表现是"子进程死了而没有任何原因"。

## 为什么用 MiniGrid 而不是自己写一个迷宫

§10.1 的原话是"**不手写第二套看似相同的规则**"。移动、朝向、门钥匙、可见域与碰撞全部
复用已有引擎；本文件做的是**投影**——把 `env` 的观测翻译成公开面，此外一行规则都不加。

## 投影的三条约定

1. **绝对坐标不出这道门。** `env.unwrapped.agent_pos` 与 `agent_dir` 都读，但只用来判
   "该不该扣留"，绝不进响应。
2. **`info` 整个丢掉。** Gymnasium 明说它可以携带隐藏变量（见规格 §19 的引用）。
3. **`direction` 保留。** 它在 MiniGrid 的观测里本来就是公开的，而视图随朝向旋转，
   认知单元靠它做无漂移里程计（见 `manifest.json` 的 `view_convention`）。
"""

import json
import struct
import sys

import gymnasium as gym
import minigrid  # noqa: F401  —— 导入即注册环境
import numpy as np

GAME = "maze"
RULES_VERSION = "maze-door-key-8x8-v1"
ENV_ID = "MiniGrid-DoorKey-8x8-v0"

#: 动作编号由 manifest.json 钉死。4（drop）与 6（done）**故意不在表里**：
#: §10.1 要求禁用它们，而"表里没有"比"表里有但别用"更难绕过。
ACTION_IDS = {
    "turn_left": 0,
    "turn_right": 1,
    "forward": 2,
    "pickup": 3,
    "toggle": 5,
}

# MiniGrid 的三条编码表（`minigrid.core.constants`）。这里手抄而不是 `import`，是为了让
# "我们往外说了什么"在这一屏里读得完；对不上的风险由 `tests/test_rules.py` 的往返测试守。
OBJECT_BY_IDX = {
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
COLOR_BY_IDX = {
    0: "red",
    1: "green",
    2: "blue",
    3: "purple",
    4: "yellow",
    5: "grey",
}
#: 只有门才谈得上开／关／锁。其余格子一律报 `none`——把地板的 state 也报出来，
#: 会让"这扇门锁着"淹没在一堆没有意义的 `open` 里。
STATE_BY_IDX = {
    0: "open",
    1: "closed",
    2: "locked",
}


class Refused(Exception):
    """请求本身不合法。回一条错误帧，不终止进程。"""

    def __init__(self, kind, reason):
        super().__init__(reason)
        self.kind = kind
        self.reason = reason


def project_cell(cell):
    """把一格 `(object_idx, color_idx, state_idx)` 投成公开面。

    **看不见的格子把颜色报成 `none`。** MiniGrid 给视野外的格子填的是默认颜色（红），
    那是编码的填充值而不是世界的颜色；照抄出去会让"我看不见这里"和"这里有个红东西"
    在颜色这一维上长得一样。对象维上它仍然是 `unseen`，所以没有多出信息——只是不再
    多出一条**看起来像观测**的噪声。
    """
    object_idx, color_idx, state_idx = int(cell[0]), int(cell[1]), int(cell[2])
    obj = OBJECT_BY_IDX.get(object_idx)
    if obj is None:
        raise Refused("projection", f"未知的对象编号 {object_idx}")
    color = COLOR_BY_IDX.get(color_idx)
    if color is None:
        raise Refused("projection", f"未知的颜色编号 {color_idx}")

    if obj == "unseen":
        color = "none"
    state = "none"
    if obj == "door":
        state = STATE_BY_IDX.get(state_idx)
        if state is None:
            raise Refused("projection", f"未知的门状态编号 {state_idx}")
    return {"object": obj, "color": color, "state": state}


def agent_cell(image_shape):
    """agent 在视图里的位置（见 `manifest.json` 的 `view_convention`）。

    MiniGrid 的 `gen_obs_grid` 把网格转到 agent 恒定落在 `(width//2, height-1)`，
    然后 `encode` 产出的是 `array[x][y]`。于是"行"是第一下标、"列"是第二下标，
    而 agent 就在最后那一列的中点上——`forward_column_delta = -1` 说的正是
    "往前走 = 列号变小"。这条约定是**规则事实**，由 `tests/test_rules.py` 的位移测试守住。
    """
    return image_shape[0] // 2, image_shape[1] - 1


def carrying_of(env):
    """携带物。**只读 `carrying` 这一个属性**，不去翻 `grid`。"""
    held = env.unwrapped.carrying
    if held is None:
        return "none"
    name = getattr(held, "type", None)
    return {"key": "key", "ball": "ball", "box": "box"}.get(name, "none")


def project(env, observation):
    """把一步的观测投成公开感知（§11.1）。"""
    image = observation["image"]
    view = [[project_cell(image[row][column]) for column in range(image.shape[1])]
            for row in range(image.shape[0])]

    # agent 自己那一格。
    #
    # MiniGrid 在那一格放的是**手上拿的东西**（没有就放空），也就是"看不见自己"。
    # 而契约层有一个 `MazeObject::Agent`——它存在的理由就是这里。报成 `agent` 比报成
    # `empty` 少一次推理：否则"我站在哪"只能靠视图外的常数去推，而那正是漂移的来源。
    agent_row, agent_column = agent_cell(image.shape)
    view[agent_row][agent_column] = {"object": "agent", "color": "none", "state": "none"}

    return {
        "mode": "symbolic_maze",
        "mission": str(observation["mission"]),
        "direction": int(observation["direction"]),
        "view": view,
        "carrying": carrying_of(env),
    }


def step_payload(env, observation, reward, terminated, truncated):
    """一步的公开结果。相位必须与两个标志一致——宿主会再校验一次。

    §11.3："**不得把故障伪装成游戏结束**。"所以自然终局走 `won`／`lost`，
    外部截断走 `timeout`，两者在这里就是两个不同的字段。
    """
    if terminated:
        outcome = "won" if reward > 0 else "lost"
    elif truncated:
        outcome = "timeout"
    else:
        outcome = "running"
    return {
        "percept": project(env, observation),
        "terminated": bool(terminated),
        "truncated": bool(truncated),
        "outcome": outcome,
        "reward": float(reward),
    }


class Maze:
    """一局 DoorKey。一个进程一局（宿主每个回合起一个进程，见 `ProcessFactory`）。"""

    def __init__(self):
        self.env = gym.make(ENV_ID)
        # 预算来自 manifest.json 的 `budget.max_steps`。MiniGrid 自己按网格尺寸算出的
        # max_steps 比它小（8×8 是 256），所以这里显式对齐一次并记下差异：规格要的是 640。
        self.env.unwrapped.max_steps = 640
        self.reset_done = False

    def reset(self, seed):
        if isinstance(seed, bool) or not isinstance(seed, int):
            raise Refused("protocol", "seed 必须是一个整数")
        if seed < 0 or seed > 2**63 - 1:
            raise Refused("protocol", f"seed 超出范围：{seed}")
        # `seed` 只在私有控制面流动（§13）：它进 `reset`，出不去任何响应。
        observation, _info = self.env.reset(seed=int(seed))
        self.reset_done = True
        return step_payload(self.env, observation, 0.0, False, False)

    def step(self, action):
        if not self.reset_done:
            raise Refused("protocol", "还没有 reset 就 step")
        if not isinstance(action, dict):
            raise Refused("protocol", "action 必须是一个对象")

        # 一、先分清"这是**另一个游戏**的动作"与"这个迷宫动作写坏了"。
        #
        # 两者都该被拒，但**不是同一件事**：宿主拿 M 种错误映射到 M 种处置，
        # 而 `kind` 就是那个映射的键——`domain` 会被译成 `InvalidAction`（引擎认为动作不在
        # 自己的规则内），其余会被译成"进程协议失步"。把"扫雷动作送到迷宫"报成后者，
        # 会让一次**规规矩矩的跨域请求**看起来像子进程崩了。
        if set(action.keys()) & {"row", "column", "flagged"} or action.get("op") in {
            "reveal",
            "chord",
            "set_flag",
        }:
            raise Refused(
                "domain",
                f"这是另一个游戏的动作（{sorted(action.keys())}），迷宫不接受；"
                f"只允许 op ∈ {sorted(ACTION_IDS)}",
            )

        # 二、只允许 `op` 一个字段：多写的字段是**协议违规**，不是可以忽略的注释。
        if set(action.keys()) != {"op"}:
            raise Refused(
                "protocol",
                f"迷宫动作只允许 op 一个字段，收到 {sorted(action.keys())}",
            )
        op = action["op"]
        if op not in ACTION_IDS:
            raise Refused(
                "domain",
                f"未知的迷宫动作 {op!r}；只允许 {sorted(ACTION_IDS)}",
            )
        observation, reward, terminated, truncated, _info = self.env.step(ACTION_IDS[op])
        # `_info` 整个丢掉：Gymnasium 明说它可以携带隐藏变量（规格 §19）。
        return step_payload(self.env, observation, reward, terminated, truncated)


def read_frame(stream):
    """读一帧。返回 `None` 表示对端正常关闭。"""
    header = stream.read(4)
    if len(header) == 0:
        return None
    if len(header) != 4:
        raise Refused("protocol", "帧头不完整")
    (declared,) = struct.unpack("<I", header)
    if declared > 256 * 1024:
        raise Refused("protocol", f"帧长度 {declared} 超过上限")
    payload = stream.read(declared)
    if len(payload) != declared:
        raise Refused("protocol", "帧体不完整")
    return json.loads(payload.decode("utf-8"))


def write_frame(stream, message):
    payload = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    stream.write(struct.pack("<I", len(payload)))
    stream.write(payload)
    stream.flush()


def handle(maze, request):
    """一条请求 → 一条响应。返回 `None` 表示该收工了。"""
    if not isinstance(request, dict):
        raise Refused("protocol", "请求必须是一个对象")
    kind = request.get("type")
    if kind == "facts":
        return {
            "type": "facts_result",
            "game": GAME,
            "rules_version": RULES_VERSION,
            # 供人读：宿主不用它做判定。
            "engine": {"kind": "minigrid", "env_id": ENV_ID},
            "supports_snapshot": False,
        }
    if kind == "reset":
        return {"type": "reset_result", "step": maze.reset(request.get("seed"))}
    if kind == "step":
        return {"type": "step_result", "step": maze.step(request.get("action"))}
    if kind == "close":
        return {"type": "close_result"}
    raise Refused("protocol", f"未知的请求类型 {kind!r}")


def main():
    stdin = sys.stdin.buffer
    stdout = sys.stdout.buffer
    maze = Maze()
    while True:
        try:
            request = read_frame(stdin)
        except Refused as refused:
            write_frame(stdout, {"type": "error", "kind": refused.kind, "reason": refused.reason})
            continue
        if request is None:
            return 0
        try:
            response = handle(maze, request)
        except Refused as refused:
            write_frame(stdout, {"type": "error", "kind": refused.kind, "reason": refused.reason})
            continue
        except Exception as error:  # noqa: BLE001 —— stderr 被丢弃，异常只能是错误帧
            write_frame(stdout, {
                "type": "error",
                "kind": "engine",
                "reason": f"{type(error).__name__}: {error}",
            })
            continue
        if response is None:
            return 0
        write_frame(stdout, response)
        if response["type"] == "close_result":
            maze.env.close()
            return 0


if __name__ == "__main__":
    sys.exit(main())
