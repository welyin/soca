"""迷宫适配器的规则事实测试。

`manifest.json` 的 `view_convention` 里写着：

> 这条约定是规则事实，不含隐藏状态，认知单元靠它做**无漂移里程计**。
> 由 `games/maze/tests/test_rules.py` 的位移测试守住。

这个文件就是那句话的兑现处。它测的**不是**我们的投影代码写得对不对，而是
**视图与世界的对应关系**——那条关系一旦错了，认知单元建出来的地图会整体旋转或镜像，
而它自己不会有任何感觉（每一步都"看起来对"）。

跑法（需要 `minigrid==3.1.0`）：

    python -m pytest games/maze/tests/test_rules.py -q

没有 pytest 时也能直接跑：`python games/maze/tests/test_rules.py`。
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "games" / "maze"))

import game  # noqa: E402

MANIFEST = json.loads((ROOT / "games" / "maze" / "manifest.json").read_text("utf-8"))

#: 视图里 agent 恒定所在的格子（manifest 明写）。
AGENT_ROW = MANIFEST["public"]["view_convention"]["agent_row"]
AGENT_COLUMN = MANIFEST["public"]["view_convention"]["agent_column"]
FORWARD_DELTA = MANIFEST["public"]["view_convention"]["forward_column_delta"]


def fresh(seed=7):
    maze = game.Maze()
    step = maze.reset(seed)
    return maze, step["percept"]


def test_the_manifest_and_this_adapter_agree_on_the_action_ids():
    # 动作号由 manifest 钉死。对不上就是两个规格同时在跑。
    assert game.ACTION_IDS == MANIFEST["public"]["action_ids"]
    # 而 `drop` 与 `done` **不在**表里（§10.1 要求禁用它们）。
    assert "drop" not in game.ACTION_IDS
    assert "done" not in game.ACTION_IDS
    assert game.RULES_VERSION == MANIFEST["rules_version"]


def test_the_agent_stands_where_the_manifest_says_and_the_view_is_square():
    _maze, percept = fresh()
    view = percept["view"]
    assert len(view) == MANIFEST["public"]["view_size"]
    assert all(len(row) == MANIFEST["public"]["view_size"] for row in view)
    assert view[AGENT_ROW][AGENT_COLUMN]["object"] == "agent", (
        "agent 必须落在 manifest 声明的那个格子上；否则认知单元算出来的地图会整体偏移"
    )


def test_moving_forward_shifts_everything_one_column_toward_the_agent():
    """位移测试：`forward_column_delta = -1` 到底对不对。

    这条是**整条链的支点**。视图随朝向旋转，所以"前方"在视图里恒定；而"恒定的前方"
    具体是哪一维、哪个方向，只能由世界来裁决：走一步，原来在前方 N 格的格子应当
    出现在前方 N−1 格的位置上。

    这里走一步 `forward`，然后逐行比对：**移动前第 `column` 列的格子，应当出现在
    移动后第 `column + 1` 列**（因为它离 agent 近了一格）。对不上就说明视图的
    旋转或转置与 manifest 的约定不一致。
    """
    maze, before = fresh()
    view_before = before["view"]

    # 找一个前方没有墙的种子，否则走不动，位移也就无从谈起。
    moved = None
    for seed in range(1, 40):
        maze, percept = fresh(seed)
        candidate = maze.step({"op": "forward"})
        if candidate["percept"]["view"][AGENT_ROW][AGENT_COLUMN - 1]["object"] != "wall":
            moved = (percept["view"], candidate["percept"]["view"])
            maze.env.close()
            break
        maze.env.close()
    view_before, view_after = moved or (None, None)
    assert view_before is not None, "找不到一个前方不是墙的种子"

    size = len(view_before)
    compared = 0
    for row in range(size):
        for column in range(size):
            # 前方列号更小（`forward_column_delta = -1`），所以移动之后内容整体 +1。
            if column == 0:
                continue
            source = view_before[row][column]
            if source["object"] == "unseen":
                # 看不见的格子不参与比对：它可能刚刚被看见，也可能仍然看不见。
                continue
            if row == AGENT_ROW and column + 1 == AGENT_COLUMN:
                # agent 自己那一格在投影里被换成了 `agent`（见 `game.project`），
                # 所以它不能拿来比——那不是世界的格子，是"我在哪"。
                continue
            target = view_after[row][column + 1] if column + 1 < size else None
            if target is None or target["object"] == "unseen":
                continue
            compared += 1
            assert target == source, (
                f"第 {row} 行第 {column} 列的 {source} 走一步之后应当出现在第 {column + 1} 列，"
                f"实际是 {target}"
            )
    assert compared >= 10, f"只比上了 {compared} 格，这条测试没测到东西"


def test_the_view_never_carries_an_absolute_position_or_the_seed():
    """§11.1：公开面里不许有绝对坐标、完整地图、RNG 状态或 `info`。"""
    maze, percept = fresh(seed=12345)
    encoded = json.dumps(percept)
    for leaked in ("agent_pos", "agent_dir", "seed", "rng", "info", "carrying_pos"):
        assert leaked not in encoded, f"公开感知里出现了 {leaked}"
    # 数字形式的绝对坐标也不好闻：8×8 的世界里它们都落在 0..7，而视图本来就是 7×7。
    # 所以判据不是"有没有数字"，而是**字段名**——上面那一条就是全部判据。
    assert set(percept.keys()) == {"mode", "mission", "direction", "view", "carrying"}
    maze.env.close()


def test_the_two_truncation_phases_are_different_fields():
    """§11.3：**不得把故障伪装成游戏结束**。自然终局与外部截断是两个字段。"""
    maze, _percept = fresh()
    step = maze.step({"op": "turn_left"})
    assert step["terminated"] is False
    assert step["truncated"] is False
    assert step["outcome"] == "running"
    maze.env.close()


def test_the_engine_refuses_actions_outside_the_published_set():
    maze, _percept = fresh()
    for bad in ({"op": "drop"}, {"op": "done"}, {"op": "teleport"}):
        try:
            maze.step(bad)
        except game.Refused as refused:
            assert refused.kind in {"domain", "protocol"}
        else:
            raise AssertionError(f"{bad} 不该被接受")
    # 多写一个字段也是违规：它是协议错误，不是可以忽略的注释。
    try:
        maze.step({"op": "forward", "n": 3})
    except game.Refused as refused:
        assert refused.kind == "protocol"
    else:
        raise AssertionError("多出来的字段不该被忽略")
    maze.env.close()


def test_the_same_seed_gives_the_same_mission_and_a_different_one_differs():
    """确定性：同一 seed 同一局。这条是整份成绩可比的前提。"""
    first = fresh(seed=99)[1]
    second = fresh(seed=99)[1]
    assert first["view"] == second["view"]
    assert first["direction"] == second["direction"]
    third = fresh(seed=100)[1]
    assert third["view"] != first["view"] or third["direction"] != first["direction"], (
        "换了种子还是一模一样，说明种子根本没接上"
    )


def _run():
    passed = 0
    for name, function in sorted(globals().items()):
        if not name.startswith("test_") or not callable(function):
            continue
        function()
        passed += 1
        print(f"  通过 {name}")
    print(f"{passed} 项通过")
    return 0


if __name__ == "__main__":
    sys.exit(_run())
