"""迷宫规则引擎的公开面测试（工单 ENG-07 的完成定义）。

覆盖 GM-01（固定种子 + 固定动作序列可复现）与 GM-03（公开面不泄露隐藏状态）中
属于本适配器的部分。真值隔离需要跨进程的严格身份测试，属于 ENG-14；本文件只证明
**投影之后的结构里没有地方放真值**。
"""

from __future__ import annotations

import json
import os
import sys
import unittest
from pathlib import Path

GAME_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(GAME_DIR))

from maze_engine import (  # noqa: E402
    ACTION_MAP,
    DEFAULT_ENV_ID,
    RULES_VERSION,
    DomainError,
    MazeEngine,
)

FIXTURE_DIR = Path(__file__).resolve().parent / "fixtures"

#: 固定动作序列。用于 GM-01 的可复现性检查。
FIXED_ACTIONS = [
    {"op": "turn_left"},
    {"op": "forward"},
    {"op": "forward"},
    {"op": "turn_right"},
    {"op": "forward"},
    {"op": "toggle"},
    {"op": "pickup"},
    {"op": "forward"},
    {"op": "forward"},
]

HIDDEN_FIELDS = [
    "info",
    "seed",
    "rng_state",
    "mine_positions",
    "safe_cells",
    "full_map",
    "solution",
    "unwrapped",
    "grid",
    "agent_pos",
    "agent_dir",
]


class MazeProjectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.engine = MazeEngine()

    def tearDown(self) -> None:
        self.engine.close()

    def test_public_action_set_excludes_engine_only_channels(self) -> None:
        # §10.1：drop 与 done 是引擎自带的、能绕过任务的通道，不得成为公开动作。
        self.assertEqual(
            set(ACTION_MAP),
            {"turn_left", "turn_right", "forward", "pickup", "toggle"},
        )
        self.assertEqual(ACTION_MAP["turn_left"], 0)
        self.assertEqual(ACTION_MAP["turn_right"], 1)
        self.assertEqual(ACTION_MAP["forward"], 2)
        self.assertEqual(ACTION_MAP["pickup"], 3)
        self.assertEqual(ACTION_MAP["toggle"], 5)

        self.engine.reset(7)
        with self.assertRaises(DomainError):
            self.engine.step({"op": "drop"})
        with self.assertRaises(DomainError):
            self.engine.step({"op": "done"})
        with self.assertRaises(DomainError):
            self.engine.step({"op": "reset"})

    def test_unseen_cells_stay_unknown(self) -> None:
        step = self.engine.reset(7)
        view = step["percept"]["view"]
        for row in view:
            for cell in row:
                if cell["object"] == "unseen":
                    self.assertEqual(cell["color"], "none")
                    self.assertEqual(cell["state"], "none")
                elif cell["object"] != "door":
                    # 只有门才有有意义的开关状态；其余对象照抄 ch2 会读成"门开着"。
                    self.assertEqual(cell["state"], "none")

    def test_the_view_is_square_and_within_the_protocol_limits(self) -> None:
        step = self.engine.reset(7)
        view = step["percept"]["view"]
        self.assertEqual(len(view), 7)
        for row in view:
            self.assertEqual(len(row), 7)
        self.assertIn(step["percept"]["direction"], (0, 1, 2, 3))
        self.assertEqual(step["percept"]["mode"], "symbolic_maze")
        self.assertLessEqual(len(step["percept"]["mission"]), 2048)
        self.assertIn(step["percept"]["carrying"], ("none", "key", "ball", "box"))

    def test_the_projection_carries_no_hidden_world_state(self) -> None:
        step = self.engine.reset(7)
        text = json.dumps(step, ensure_ascii=False)
        for field in HIDDEN_FIELDS:
            self.assertNotIn(f'"{field}"', text, f"公开面出现了隐藏字段 {field}")
        # 相位必须自洽。
        self.assertFalse(step["terminated"])
        self.assertFalse(step["truncated"])
        self.assertEqual(step["outcome"], "running")

    def test_projection_is_deterministic_for_a_fixed_seed_and_action_sequence(self) -> None:
        # GM-01：固定种子 + 固定动作序列，符号视图与终局必须逐字段一致。
        first = self._run_fixed_sequence()
        second = self._run_fixed_sequence()
        self.assertEqual(first, second)

        golden_path = FIXTURE_DIR / "golden_door_key_8x8_seed7.json"
        if os.environ.get("UPDATE_GOLDEN"):
            FIXTURE_DIR.mkdir(parents=True, exist_ok=True)
            golden_path.write_text(
                json.dumps(
                    {
                        "rules_version": RULES_VERSION,
                        "env_id": DEFAULT_ENV_ID,
                        "seed": 7,
                        "actions": FIXED_ACTIONS,
                        "steps": first,
                    },
                    ensure_ascii=False,
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )
            return
        golden = json.loads(golden_path.read_text(encoding="utf-8"))
        self.assertEqual(
            first,
            golden["steps"],
            "投影结果与冻结的黄金轨迹不一致；若规则确实有意变更，请提升 RULES_VERSION 并重生成",
        )

    def test_a_later_reset_does_not_inherit_the_previous_episode(self) -> None:
        self._run_fixed_sequence()
        fresh = self.engine.reset(7)
        baseline = self.engine.reset(7)
        self.assertEqual(fresh, baseline)

    def test_manifest_facts_match_the_engine(self) -> None:
        facts = self.engine.manifest_facts()
        self.assertEqual(facts["env_id"], DEFAULT_ENV_ID)
        self.assertEqual(facts["view_size"], 7)
        self.assertEqual(facts["action_ids"], ACTION_MAP)

        manifest = json.loads((GAME_DIR / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["rules_version"], RULES_VERSION)
        self.assertEqual(manifest["engine"]["env_id"], facts["env_id"])
        self.assertEqual(manifest["budget"]["max_steps"], facts["max_steps"])
        self.assertEqual(manifest["public"]["view_size"], facts["view_size"])
        self.assertEqual(manifest["public"]["action_ids"], facts["action_ids"])
        # 公开动作集合与 manifest 双向一致。
        self.assertEqual(set(manifest["public"]["actions"]), set(ACTION_MAP))

    def _run_fixed_sequence(self) -> list[dict]:
        steps = [self.engine.reset(7)]
        for action in FIXED_ACTIONS:
            steps.append(self.engine.step(action))
        return steps


if __name__ == "__main__":
    unittest.main(verbosity=2)
