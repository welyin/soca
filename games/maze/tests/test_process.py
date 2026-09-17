"""游戏进程边界测试：帧格式、请求应答与干净退出（实施规格 §11.4）。

规则本身的测试在 `test_rules.py`。这里测的是**进程边界**：真实启动一个子进程，用长度前缀
JSON 与它对话，检查超限拒绝、未知请求、引擎拒绝与正常退出。这些正是"在本进程里直接调函数"
测不到的部分。
"""

from __future__ import annotations

import json
import struct
import subprocess
import sys
import unittest
from pathlib import Path

GAME_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(GAME_DIR))

from protocol import MAX_MESSAGE_BYTES  # noqa: E402

ENTRY = GAME_DIR / "game.py"
HEADER = struct.Struct("<I")


class GameProcessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.process = subprocess.Popen(
            [sys.executable, "-B", "-X", "utf8", str(ENTRY)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.addCleanup(self._reap)

    def _reap(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=10)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None and not stream.closed:
                stream.close()

    def send(self, message: object) -> None:
        payload = json.dumps(message, ensure_ascii=False).encode("utf-8")
        self.process.stdin.write(HEADER.pack(len(payload)))
        self.process.stdin.write(payload)
        self.process.stdin.flush()

    def send_raw(self, declared_length: int, payload: bytes) -> None:
        self.process.stdin.write(HEADER.pack(declared_length))
        self.process.stdin.write(payload)
        self.process.stdin.flush()

    def receive(self) -> dict:
        header = self._read_exact(4)
        (length,) = HEADER.unpack(header)
        return json.loads(self._read_exact(length).decode("utf-8"))

    def _read_exact(self, count: int) -> bytes:
        chunks = bytearray()
        while len(chunks) < count:
            chunk = self.process.stdout.read(count - len(chunks))
            if not chunk:
                raise AssertionError(
                    f"进程提前结束：退出码 {self.process.returncode}，"
                    f"stderr={self.process.stderr.read().decode('utf-8', 'replace')}"
                )
            chunks.extend(chunk)
        return bytes(chunks)

    def test_a_full_exchange_over_the_framing(self) -> None:
        self.send({"type": "facts"})
        facts = self.receive()
        self.assertEqual(facts["type"], "facts_result")
        self.assertEqual(facts["game"], "maze")
        self.assertEqual(facts["facts"]["view_size"], 7)

        self.send({"type": "reset", "seed": 7})
        reset = self.receive()
        self.assertEqual(reset["type"], "reset_result")
        self.assertEqual(reset["step"]["outcome"], "running")
        self.assertEqual(len(reset["step"]["percept"]["view"]), 7)

        self.send({"type": "step", "action": {"op": "turn_left"}})
        step = self.receive()
        self.assertEqual(step["type"], "step_result")
        self.assertIn(step["step"]["percept"]["direction"], (0, 1, 2, 3))

        self.send({"type": "close"})
        self.assertEqual(self.receive()["type"], "close_result")
        self.assertEqual(self.process.wait(timeout=10), 0)

    def test_engine_only_actions_are_refused_at_the_process_boundary(self) -> None:
        self.send({"type": "reset", "seed": 7})
        self.receive()

        for operation in ("drop", "done", "reset", "solve"):
            self.send({"type": "step", "action": {"op": operation}})
            response = self.receive()
            self.assertEqual(response["type"], "error")
            self.assertEqual(response["kind"], "domain", f"{operation} 必须被拒绝")

        # 被拒绝之后进程仍然可用：一次非法动作不该把整局打掉。
        self.send({"type": "step", "action": {"op": "forward"}})
        self.assertEqual(self.receive()["type"], "step_result")

    def test_unknown_requests_are_refused_without_killing_the_process(self) -> None:
        self.send({"type": "launch_missiles"})
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertEqual(response["kind"], "protocol")

        self.send({"type": "reset", "seed": "not-an-integer"})
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertEqual(response["kind"], "protocol")

    def test_an_oversized_frame_is_refused_before_being_allocated(self) -> None:
        # §11.4：先检查长度再分配，超限关闭该请求。
        self.send_raw(MAX_MESSAGE_BYTES + 1, b"")
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertEqual(response["kind"], "protocol")
        self.assertNotEqual(self.process.wait(timeout=10), 0)

    def test_closing_stdin_ends_the_process_cleanly(self) -> None:
        self.send({"type": "reset", "seed": 7})
        self.receive()
        self.process.stdin.close()
        self.assertEqual(self.process.wait(timeout=10), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
