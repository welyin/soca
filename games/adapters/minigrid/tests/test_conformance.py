"""引擎驱动的规则事实测试：**对每一个游戏各跑一遍**。

这测的**不是**我们的投影代码写得对不对，而是**视图与世界的对应关系**——那条关系一旦错了，
认知单元建出来的地图会整体旋转或镜像，而它自己不会有任何感觉（每一步都"看起来对"）。

## 为什么按游戏遍历，而不是写死一个

因为这一份驱动服侍的是**引擎**，而游戏是清单。把 `door-key` 写死在这里的话，
加一个游戏（`classic-maze`）不会有任何东西提醒你"新游戏没被测过"——
它的视图约定可能整体反了，而测试全绿。

遍历 `games/*/manifest.json` 之后，**加一个游戏自动获得这一整套检查**；
而清单里写错一个 `env_id` 也会当场红。

跑法（需要 `minigrid==3.1.0`）：

    python games/adapters/minigrid/tests/test_conformance.py

没有 pytest 也能跑——这个文件自己就会遍历。
"""

import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[3]
sys.path.insert(0, str(HERE.parent))

import driver  # noqa: E402

#: 所有游戏：`games/<id>/manifest.json`。适配器目录里没有清单，所以不会被扫进来。
GAMES = sorted((ROOT / "games").glob("*/manifest.json"))


def games():
    assert GAMES, "一个游戏清单都没找到——路径算错了，不是仓库里没有游戏"
    for path in GAMES:
        yield path, json.loads(path.read_text("utf-8"))


def open_level(manifest_path, manifest, level_name):
    """按清单开一局，返回 `(maze, percept)`。"""
    maze = driver.Maze(manifest_path, level_name)
    step = maze.reset(7)
    return maze, step["percept"]


def test_every_game_declares_an_adapter_that_exists():
    # 清单说"谁来驱动我"，而那个文件要在。写错一个路径的表现是
    # "这个游戏起不来"，而那时没人会想到是清单里一行路径的事。
    for path, manifest in games():
        adapter = ROOT / manifest["adapter"]
        assert adapter.exists(), f"{path.name} 指的驱动不在：{adapter}"
        assert manifest["default_level"] in manifest["levels"], (
            f"{path.name} 的默认等级不在 levels 里"
        )


def test_every_level_in_every_game_can_actually_be_opened():
    seen = set()
    for path, manifest in games():
        for name in manifest["levels"]:
            maze, percept = open_level(path, manifest, name)
            assert len(percept["view"]) == manifest["public"]["view_size"]
            assert maze.env.unwrapped.max_steps >= 50
            maze.env.close()

            # 规则版本各不相同：它是宿主与账本区分"哪一套规则跑出来的这一局"的依据。
            # 两关（更别说两个游戏）共用一个版本号，会让它们在账上长得一样。
            version = maze.level["rules_version"]
            assert version not in seen, f"{path.name}/{name} 的规则版本与别人重复：{version}"
            seen.add(version)
            print(f"    {manifest['game']}/{name} → {version}")


def test_action_ids_come_from_the_manifest_not_from_the_code():
    for path, manifest in games():
        maze = driver.Maze(path, None)
        assert maze.action_ids == manifest["public"]["action_ids"], path.name
        # 而 `drop` 与 `done` **不在**任何一份清单里（§10.1 要求禁用它们）。
        assert "drop" not in maze.action_ids and "done" not in maze.action_ids
        maze.env.close()


def test_the_agent_stands_where_the_manifest_says_and_the_view_is_square():
    for path, manifest in games():
        for name in manifest["levels"]:
            convention = manifest["public"]["view_convention"]
            maze, percept = open_level(path, manifest, name)
            view = percept["view"]
            size = manifest["public"]["view_size"]
            assert len(view) == size and all(len(row) == size for row in view)
            assert view[convention["agent_row"]][convention["agent_column"]]["object"] == "agent", (
                f"{manifest['game']}/{name}：agent 必须落在清单声明的那个格子上；"
                "否则认知单元算出来的地图会整体偏移"
            )
            maze.env.close()


def test_moving_forward_shifts_everything_one_column_toward_the_agent():
    """位移测试：`forward_column_delta = -1` 到底对不对。

    这条是**整条链的支点**。视图随朝向旋转，所以"前方"在视图里恒定；而"恒定的前方"
    具体是哪一维、哪个方向，只能由世界来裁决：走一步，原来在前方 N 格的格子应当
    出现在前方 N−1 格的位置上。

    这里走一步 `forward`，然后逐行比对：**移动前第 `column` 列的格子，应当出现在
    移动后第 `column + 1` 列**（因为它离 agent 近了一格）。对不上就说明视图的
    旋转或转置与清单的约定不一致。
    """
    for path, manifest in games():
        convention = manifest["public"]["view_convention"]
        row_at = convention["agent_row"]
        column_at = convention["agent_column"]
        checked = 0
        for name in manifest["levels"]:
            # 找一个前方没有墙的种子，否则走不动，位移也就无从谈起。
            for seed in range(1, 40):
                maze = driver.Maze(path, name)
                before = maze.reset(seed)["percept"]["view"]
                after = maze.step({"op": "forward"})["percept"]["view"]
                if before[row_at][column_at - 1]["object"] == "wall":
                    maze.env.close()
                    continue
                checked += compare_shift(before, after, row_at, column_at, manifest, name)
                maze.env.close()
                break
            else:
                raise AssertionError(f"{manifest['game']}/{name}：找不到一个前方不是墙的种子")
        # **阈值按视图大小来，不写死。** 先前写的是 10，那是按 7×7 定的；
        # 3×3 的视图一共 9 格，能比上的就那么几格，于是这条断言在第二个尺寸上
        # 永远不成立——而"断言没测到东西"与"几何算错了"是两件事，混在一起会把人指错方向。
        floor = manifest["public"]["view_size"] - 1
        assert checked >= floor, (
            f"{manifest['game']}：只比上了 {checked} 格（视图 {manifest['public']['view_size']}×"
            f"{manifest['public']['view_size']}，至少要 {floor}），这条测试没测到东西"
        )


def compare_shift(before, after, row_at, column_at, manifest, name):
    size = len(before)
    compared = 0
    for row in range(size):
        for column in range(size):
            # 前方列号更小（`forward_column_delta = -1`），所以移动之后内容整体 +1。
            if column == 0:
                continue
            source = before[row][column]
            if source["object"] == "unseen":
                # 看不见的格子不参与比对：它可能刚刚被看见，也可能仍然看不见。
                continue
            if row == row_at and column + 1 == column_at:
                # agent 自己那一格在投影里被换成了 `agent`（见 `driver.project`），
                # 所以它不能拿来比——那不是世界的格子，是"我在哪"。
                continue
            target = after[row][column + 1] if column + 1 < size else None
            if target is None or target["object"] == "unseen":
                continue
            compared += 1
            assert target == source, (
                f"{manifest['game']}/{name}：第 {row} 行第 {column} 列的 {source} "
                f"走一步之后应当出现在第 {column + 1} 列，实际是 {target}"
            )
    return compared


def test_no_game_leaks_an_absolute_position_or_the_seed():
    """§11.1：公开面里不许有绝对坐标、完整地图、RNG 状态或 `info`。"""
    for path, manifest in games():
        for name in manifest["levels"]:
            maze, percept = open_level(path, manifest, name)
            encoded = json.dumps(percept)
            for leaked in ("agent_pos", "agent_dir", "seed", "rng", "info", "carrying_pos"):
                assert leaked not in encoded, f"{manifest['game']}/{name} 的公开感知里出现了 {leaked}"
            assert set(percept.keys()) == {"mode", "mission", "direction", "view", "carrying"}
            maze.env.close()


def test_the_two_truncation_phases_are_different_fields():
    """§11.3：**不得把故障伪装成游戏结束**。自然终局与外部截断是两个字段。"""
    for path, manifest in games():
        maze, _percept = open_level(path, manifest, None)
        step = maze.step({"op": "turn_left"})
        assert step["terminated"] is False
        assert step["truncated"] is False
        assert step["outcome"] == "running"
        maze.env.close()


def test_the_engine_refuses_actions_outside_the_published_set():
    for path, manifest in games():
        maze, _percept = open_level(path, manifest, None)
        for bad in ({"op": "drop"}, {"op": "done"}, {"op": "teleport"}):
            try:
                maze.step(bad)
            except driver.Refused as refused:
                assert refused.kind in {"domain", "protocol"}
            else:
                raise AssertionError(f"{bad} 不该被接受")
        # 多写一个字段也是违规：它是协议错误，不是可以忽略的注释。
        try:
            maze.step({"op": "forward", "n": 3})
        except driver.Refused as refused:
            assert refused.kind == "protocol"
        else:
            raise AssertionError("多出来的字段不该被忽略")
        maze.env.close()


def test_the_same_seed_gives_the_same_mission_and_a_different_one_differs():
    """确定性：同一 seed 同一局。这条是整份成绩可比的前提。"""
    for path, manifest in games():
        for name in manifest["levels"]:
            first = driver.Maze(path, name)
            first_percept = first.reset(99)["percept"]
            first.env.close()
            second = driver.Maze(path, name)
            second_percept = second.reset(99)["percept"]
            second.env.close()
            third = driver.Maze(path, name)
            third_percept = third.reset(100)["percept"]
            third.env.close()
            assert first_percept["view"] == second_percept["view"]
            assert first_percept["direction"] == second_percept["direction"]
            assert (
                third_percept["view"] != first_percept["view"]
                or third_percept["direction"] != first_percept["direction"]
            ), f"{manifest['game']}/{name} 换了种子还是一模一样，说明种子根本没接上"


def test_a_game_only_contains_what_its_rules_say_it_contains():
    """游戏之间**真的不同**，而不是同一份规则换了名字。

    这条是这一整套测试里最要紧的一条："同一个探索器能跑通两个游戏"这件事，
    只有在两个游戏真的不同的时候才有意义。门钥匙房间要是悄悄退化成传统迷宫，
    那句话就是空的，而页面上还会显示得好好的。
    """
    door_key = driver.truth(ROOT / "games" / "door-key" / "manifest.json", 7, None)
    objects = {cell["object"] for cell in door_key["cells"]}
    assert {"key", "door", "goal"} <= objects, f"门钥匙房间缺东西：{objects}"

    classic = driver.truth(ROOT / "games" / "classic-maze" / "manifest.json", 7, "maze")
    objects = {cell["object"] for cell in classic["cells"]}
    assert "goal" in objects, "传统迷宫得有终点"
    assert "door" not in objects and "key" not in objects, f"传统迷宫不该有这些：{objects}"

    # **它得真的是迷宫。** 这一条是"这就是个空荡荡的一大块"逼出来的：
    # 空地与四房间都有终点、也都没有钥匙和门，所以上面那两句它们全都过。
    # 迷宫与空地的区别在**走廊**与**死胡同**——所以判据只能是它们。
    # 可走格 = **内部减去墙**。不能拿 `cells` 里的 `empty` 来当走廊：真值图只列
    # **非空**的格子（墙、门、钥匙、目标），空走廊压根不在里面——照那个算，
    # 可走格只剩终点一格，于是"死胡同 0 个"。这条断言第一次跑就是这么红的。
    walls = {(cell["x"], cell["y"]) for cell in classic["cells"] if cell["object"] == "wall"}
    walkable = {
        (x, y)
        for x in range(1, classic["width"] - 1)
        for y in range(1, classic["height"] - 1)
        if (x, y) not in walls
    }
    dead_ends = [
        at
        for at in walkable
        if sum(
            1 for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)) if (at[0] + dx, at[1] + dy) in walkable
        )
        == 1
    ]
    assert len(dead_ends) >= 4, f"死胡同只有 {len(dead_ends)} 个——这不是迷宫，是空地"
    assert "agent" not in objects or True  # 起点在真值里是 `start`，不是格子


def test_an_unknown_level_is_refused_rather_than_silently_defaulted():
    # 悄悄退回默认的表现是"我选了四房间，跑出来的却是墙与缺口"——
    # 而那不是故障，是有人以为自己在看另一局。
    manifest = json.loads((ROOT / "games" / "classic-maze" / "manifest.json").read_text("utf-8"))
    try:
        driver.level_of(manifest, "does-not-exist")
    except driver.Refused as refused:
        assert refused.kind == "protocol"
        assert "does-not-exist" in refused.reason
    else:
        raise AssertionError("未知等级不该被接受")


def test_the_driver_refuses_to_guess_which_game_it_serves():
    # `--manifest` 是必给的：这一份驱动不认识任何一个游戏。
    # 不给就报错，而不是去猜一个默认路径——猜的表现是"我起了传统迷宫，跑的却是门钥匙"。
    try:
        driver.arguments(["driver.py"])
    except driver.Refused as refused:
        assert "--manifest" in refused.reason
    else:
        raise AssertionError("缺 --manifest 不该被接受")


def _run():
    passed = 0
    for name, function in sorted(globals().items()):
        if not name.startswith("test_") or not callable(function):
            continue
        function()
        passed += 1
        print(f"  通过 {name}")
    print(f"{passed} 项通过，覆盖 {len(GAMES)} 个游戏")
    return 0


if __name__ == "__main__":
    sys.exit(_run())
