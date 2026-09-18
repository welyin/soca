"""MiniGrid 引擎驱动：把 MiniGrid 的 env 投成公开面（§10.1、ENG-07）。

**它服侍的是引擎，不是某个游戏。** 门钥匙房间与传统迷宫是两个游戏
（`games/door-key/`、`games/classic-maze/`，各有各的清单与规则集），
而它们共用这一份驱动——因为**规则由 MiniGrid 执行**，两边都不是我们写的。
调用方把**游戏清单**的路径给它（`--manifest`），环境、胜负条件、规则版本、
步数上限全部从那份清单来；驱动自己不认识任何一个游戏名。

把驱动按游戏复制一份会怎样：两个文件除了清单里那一个 `env_id` 之外逐字相同，
而改动（比如这条注释里说的投影修正）要在两处各做一次、且没有任何东西会提醒你漏了一处。
那才是 §10.1 那句"**不手写第二套看似相同的规则**"要防的事。

本进程是**规则引擎**，不是宿主。§8 把职责切成两半：引擎执行规则，宿主负责幂等账与
下一公开观测。所以这里的输出只有"已投影的公开感知"——隐藏真值（完整地图、绝对坐标、
RNG 状态、`info` 字典、专家动作）在构造感知时就被丢掉，宿主与客户端根本拿不到它们。

## 协议

帧 = 4 字节小端长度 + UTF-8 JSON，与 `crates/game-host/src/process.rs` 的 `write_frame`
一一对应。四类请求：

    {"type": "facts"}                  → {"type": "facts_result", "game": "maze", "game_id": "...",
                                          "rules_version": "..."}
    {"type": "reset", "seed": N}       → {"type": "reset_result", "step": {...}}
    {"type": "step",  "action": {...}} → {"type": "step_result",  "step": {...}}
    {"type": "close"}                  → {"type": "close_result"}

出错回 `{"type": "error", "kind": ..., "reason": ...}`。stderr 被宿主丢弃，所以异常
必须变成一条错误帧——否则表现是"子进程死了而没有任何原因"。

## 为什么用 MiniGrid 而不是自己写一个迷宫

§10.1 的原话是"**不手写第二套看似相同的规则**"。移动、朝向、门钥匙、可见域与碰撞全部
复用已有引擎；这个文件做的是**投影**——把 `env` 的观测翻译成公开面，此外一行规则都不加。
加一个游戏（换一个 `env_id`、换一套胜负条件）不需要碰这一行代码，只需要一份新清单。

## 投影的三条约定

1. **绝对坐标不出这道门。** `env.unwrapped.agent_pos` 与 `agent_dir` 都读，但只用来判
   "该不该扣留"，绝不进响应。
2. **`info` 整个丢掉。** Gymnasium 明说它可以携带隐藏变量（见规格 §19 的引用）。
3. **`direction` 保留。** 它在 MiniGrid 的观测里本来就是公开的，而视图随朝向旋转，
   认知单元靠它做无漂移里程计（见清单里的 `view_convention`）。
"""

import json
import pathlib
import sys

import gymnasium as gym
import minigrid  # noqa: F401  —— 导入即注册环境
import numpy as np

from protocol import ProtocolError, read_message, stdio_streams, write_message

#: 这一层自报的**域**名，与 `GameKind::as_str()` 一致（宿主用 `check_game` 核对）。
#:
#: 它**不是游戏名**。门钥匙房间与传统迷宫是两个游戏（两份清单、两套规则），
#: 而它们是同一个感知/动作域：宿主那边都是 `GameKind::Maze`，动作域都是
#: `ActionDomain::Maze`，感知都是 `Percept::Maze`。把游戏名报在这里，
#: 等于拿一个"域"去比一个"游戏"——那在只有一个游戏时看不出问题，
#: 在加第二个游戏的那一天就对不上了。
GAME = "maze"


def load_manifest(path):
    """读一份游戏清单。

    **路径由调用方给**，不再从 `__file__` 旁边找。这一份驱动服侍若干游戏，
    而"是哪一局"完全由清单决定；自己去找的话，驱动就得知道游戏目录在哪、
    有几个游戏、哪个是默认——那些都不是它该知道的事。
    """
    with pathlib.Path(path).open(encoding="utf-8") as handle:
        return json.load(handle)

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


def level_of(manifest, name):
    """按名字取一份等级配置。名字为空时取默认那一份。

    **等级属于游戏**：游戏的规则集（`levels` 里每一条都有自己的 `rules_version`），
    而"游戏"是清单这一层的事——传统迷宫的"墙与缺口"与"四房间"是同一个游戏的两关，
    门钥匙房间则是另一个游戏。

    **名字不认识就报错**，而不是悄悄退回默认：退回默认的表现是"我选了四房间，
    跑出来的却是墙与缺口"——而那不是故障，是有人以为自己在看另一局。
    """
    levels = manifest["levels"]
    key = name or manifest["default_level"]
    if key not in levels:
        raise Refused(
            "protocol",
            f"未知的等级 {key!r}；这份清单里有 {sorted(levels)}",
        )
    entry = levels[key]
    return {
        "name": key,
        "env_id": entry["env_id"],
        "rules_version": entry["rules_version"],
        "max_steps": int(entry["budget"]["max_steps"]),
    }


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
    """一个游戏的一局。一个进程一局（宿主每个回合起一个进程，见 `ProcessFactory`）。

    **哪一局由两份东西决定，而它们是两件事：**

    * **游戏清单**——规则集：环境、胜负条件、规则版本。那是这个游戏的**身份**，
      门钥匙房间与传统迷宫因此是两个游戏（各有各的清单）。
    * **等级**——同一个规则集下的不同关卡（传统迷宫的"墙与缺口"与"四房间"）。

    进程自己**不认识任何一个游戏名**：它只认识交给它的那份清单。这样加一个游戏
    不需要改这一行代码，而"这一份驱动服侍哪些游戏"这个问题在代码里根本没有答案——
    它只在 `games/` 的目录结构里。
    """

    def __init__(self, manifest_path, level=None):
        self.manifest = load_manifest(manifest_path)
        self.level = level_of(self.manifest, level)
        # 动作编号由清单钉死。4（drop）与 6（done）**故意不在表里**：
        # §10.1 要求禁用它们，而"表里没有"比"表里有但别用"更难绕过。
        self.action_ids = dict(self.manifest["public"]["action_ids"])
        self.env = gym.make(self.level["env_id"])
        # 步数上限按清单来。MiniGrid 自己按网格尺寸算出的那个通常更小
        # （DoorKey-8x8 是 256），所以显式对齐一次——而"该是多少"由清单说，不由这里说。
        self.env.unwrapped.max_steps = self.level["max_steps"]
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
                f"只允许 op ∈ {sorted(self.action_ids)}",
            )

        # 二、只允许 `op` 一个字段：多写的字段是**协议违规**，不是可以忽略的注释。
        if set(action.keys()) != {"op"}:
            raise Refused(
                "protocol",
                f"迷宫动作只允许 op 一个字段，收到 {sorted(action.keys())}",
            )
        op = action["op"]
        if op not in self.action_ids:
            raise Refused(
                "domain",
                f"未知的迷宫动作 {op!r}；只允许 {sorted(self.action_ids)}",
            )
        observation, reward, terminated, truncated, _info = self.env.step(self.action_ids[op])
        # `_info` 整个丢掉：Gymnasium 明说它可以携带隐藏变量（规格 §19）。
        return step_payload(self.env, observation, reward, terminated, truncated)


def truth(manifest_path, seed, level=None):
    """整座迷宫的真值。**私有控制面，不是引擎协议里的一条。**

    §15.2 那句"迷宫不泄露全图/绝对真值"约束的是**认知单元**：模型只收到公开观测。
    而操作员要看的那张图、以及赛后指标，本来就在私有控制面上（同一句里写着
    "Evaluator私有控制面负责reset、种子与赛后指标"）。所以它走的是**另一条入口**——
    一个命令行开关，而不是协议里的一个请求类型。

    分成两个入口不是为了好看：混进协议里之后，任何一条拿到 `Engine` 的代码都能顺手问一句
    "真值是什么"，而那一句话会以"某个单元突然很会走迷宫"的形式在很久以后暴露出来。
    走命令行则要求调用方**显式地**起一个进程，而那个动作在代码里看得见。

    `start` 是开局时 agent 的世界坐标。没有它这张图对不上——**agent 自己的地图是以
    出发点为原点的**（公开面里没有绝对坐标，它只能这么记），而这里是世界坐标。
    两者之间差的就是这一个平移。
    """
    maze = Maze(manifest_path, level)
    observation, _info = maze.env.reset(seed=int(seed))
    start = (int(maze.env.unwrapped.agent_pos[0]), int(maze.env.unwrapped.agent_pos[1]))
    grid = maze.env.unwrapped.grid
    cells = []
    for x in range(grid.width):
        for y in range(grid.height):
            value = grid.get(x, y)
            if value is None:
                continue
            if value.type in ("wall", "door", "key", "ball", "box", "goal", "lava"):
                # 只有门谈得上开／关／锁，而 `is_open` 也只有门才有——
                # 对着墙问它开没开，得到的是一个 `AttributeError`，不是 `False`。
                state = "none"
                if value.type == "door":
                    state = (
                        "open" if value.is_open
                        else "locked" if value.is_locked
                        else "closed"
                    )
                cells.append({
                    "x": x,
                    "y": y,
                    "object": value.type,
                    "color": str(value.color) if value.color else "none",
                    "state": state,
                })
    return {
        "width": grid.width,
        "height": grid.height,
        "start": {"x": start[0], "y": start[1]},
        "direction": int(observation["direction"]),
        "cells": cells,
    }


def handle(maze, request):
    """一条请求 → 一条响应。返回 `None` 表示该收工了。"""
    if not isinstance(request, dict):
        raise Refused("protocol", "请求必须是一个对象")
    kind = request.get("type")
    if kind == "facts":
        return {
            "type": "facts_result",
            "game": GAME,
            # **游戏与等级分开报，因为它们是两件事。** 宿主与账本用 `rules_version`
            # 区分"哪一套规则跑出来的这一局"——门钥匙房间与传统迷宫各有各的版本号，
            # 共用的话它们会在账上长得一样。
            "game_id": maze.manifest["game"],
            "rules_version": maze.level["rules_version"],
            # 供人读：宿主不用它做判定。
            "engine": {
                "kind": maze.manifest["engine"]["kind"],
                "env_id": maze.level["env_id"],
                "level": maze.level["name"],
            },
            "supports_snapshot": False,
        }
    if kind == "reset":
        return {"type": "reset_result", "step": maze.reset(request.get("seed"))}
    if kind == "step":
        return {"type": "step_result", "step": maze.step(request.get("action"))}
    if kind == "close":
        return {"type": "close_result"}
    raise Refused("protocol", f"未知的请求类型 {kind!r}")


def arguments(argv):
    """解析这一层的开关：`--manifest`、`--level`、`--truth`。

    **`--manifest` 是必给的**：这一份驱动不认识任何一个游戏，它只认识交给它的清单。
    不给就报错，而不是去猜一个默认路径——猜的表现是"我起了传统迷宫，跑的却是门钥匙"。

    用手写而不是 argparse：三个开关只有三个取值位，而 argparse 会顺手接受
    `--level=x` 这种写法与应用户一段它没有的语法。
    """
    manifest = None
    level = None
    truth_seed = None
    index = 1
    while index < len(argv):
        if argv[index] == "--manifest" and index + 1 < len(argv):
            manifest = argv[index + 1]
            index += 2
        elif argv[index] == "--level" and index + 1 < len(argv):
            level = argv[index + 1]
            index += 2
        elif argv[index] == "--truth" and index + 1 < len(argv):
            truth_seed = argv[index + 1]
            index += 2
        else:
            raise Refused("protocol", f"看不懂的参数 {argv[index]!r}")
    if manifest is None:
        raise Refused("protocol", "必须给 --manifest：这一份驱动不认识任何一个游戏")
    return manifest, level, truth_seed


def main(argv):
    try:
        manifest, level, truth_seed = arguments(argv)
    except Refused as refused:
        # 参数不对时还没有会话，而宿主在等一条 `facts_result`——它同样接受一条 `error`。
        # 不回这一条的话，进程带着一段 traceback 退出，而 **stderr 被宿主丢弃**：
        # 表现是"引擎起不来"，真因是"参数给错了"，而两者在界面上分不开。
        write_message(
            sys.stdout.buffer,
            {"type": "error", "kind": refused.kind, "reason": refused.reason},
        )
        return 1

    # 私有控制面：`--truth <seed>` 打一份真值就退出，不进入协议循环。
    if truth_seed is not None:
        # **显式 UTF-8，不用 `print`。** `print` 的编码跟着控制台的代码页走：
        # 在 Windows 上它默认是 UTF-16 或 GBK，而调用方按 UTF-8 解——
        # 表现是"真值不是合法 JSON"，而真因是打它的时候用了哪个编码。
        # 协议那条路一直是这么做的（`sys.stdout.buffer`），这条路先前漏了。
        payload = json.dumps(truth(manifest, int(truth_seed), level), ensure_ascii=False)
        sys.stdout.buffer.write(payload.encode("utf-8"))
        sys.stdout.buffer.flush()
        return 0

    stdin, stdout = stdio_streams()

    try:
        maze = Maze(manifest, level)
    except Refused as refused:
        # 起不来的原因要能传出去。宿主在等一条 `facts_result`，而它同样接受一条 `error`——
        # 让它去等一个永远不会回话的进程，表现是"引擎超时"，而真因是"等级名写错了"。
        write_message(stdout, {"type": "error", "kind": refused.kind, "reason": refused.reason})
        return 1

    while True:
        try:
            request = read_message(stdin)
        except ProtocolError as error:
            # **框架错误之后不再继续。** 长度前缀的流一旦超限或截断就**无法重新对齐**：
            # 继续读只会把剩下的字节当成长度头，然后对着垃圾报一串看不懂的错。
            # 这一点先前是错的（报一条错误帧然后 `continue`），
            # 由 `tests/test_boundary.py` 那条超限测试逼出来。
            write_message(stdout, {"type": "error", "kind": "protocol", "reason": str(error)})
            return 1
        if request is None:
            return 0
        try:
            response = handle(maze, request)
        except Refused as refused:
            write_message(
                stdout, {"type": "error", "kind": refused.kind, "reason": refused.reason}
            )
            continue
        except Exception as error:  # noqa: BLE001 —— stderr 被丢弃，异常只能是错误帧
            write_message(stdout, {
                "type": "error",
                "kind": "engine",
                "reason": f"{type(error).__name__}: {error}",
            })
            continue
        if response is None:
            return 0
        write_message(stdout, response)
        if response["type"] == "close_result":
            maze.env.close()
            return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
