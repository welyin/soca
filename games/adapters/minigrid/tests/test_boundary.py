"""游戏进程边界测试：帧格式、请求应答与干净退出（实施规格 §11.4）。

规则本身的测试在 `test_conformance.py`。这里测的是**进程边界**：真实启动一个子进程，
用长度前缀 JSON 与它对话，检查超限拒绝、未知请求、引擎拒绝与正常退出。
这些正是"在本进程里直接调函数"测不到的部分。

## 也是按游戏遍历的

同一份边界测试对**每个游戏**各跑一遍。这不是凑数：`--manifest` 是这一层唯一的
"我是谁"的入口，而它接错的表现（起的是另一局）在单游戏测试里根本看不出来。

## 这一份测试抓到过两个真缺陷

它先前是对着更早的那份适配器写的，我换掉适配器之后它就红了——而那两条红的是我的问题：

1. `facts` 的形状。旧的那份在外层包了一个 `facts` 子对象，而**宿主只读顶层的
   `type`／`game`／`rules_version`**。谁对？读宿主的代码，以它真正读的为准。
2. **超长帧之后进程没有退出。** 长度前缀的流一旦超限就无法重新对齐，
   继续读只会把剩下的字节当成长度头。这一条是这条通路里最安静的一个坑：
   报一条错误帧然后 `continue`，看起来"很健壮"，实际是把流读错了位。
"""

from __future__ import annotations

import json
import pathlib
import struct
import subprocess
import sys
import unittest

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[3]
sys.path.insert(0, str(HERE.parent))

from protocol import MAX_MESSAGE_BYTES  # noqa: E402

DRIVER = HERE.parent / "driver.py"
HEADER = struct.Struct("<I")
GAMES = sorted(path.parent for path in (ROOT / "games").glob("*/manifest.json"))


def manifest_of(game_dir):
    return game_dir / "manifest.json"


class GameProcessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.process = None
        self.addCleanup(self._reap)

    def start(self, game_dir, level=None):
        arguments = [sys.executable, "-B", "-X", "utf8", str(DRIVER), "--manifest", str(manifest_of(game_dir))]
        if level is not None:
            arguments += ["--level", level]
        self.process = subprocess.Popen(
            arguments,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        return self.process

    def _reap(self) -> None:
        if self.process is None:
            return
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=10)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None and not stream.closed:
                stream.close()

    def finish(self) -> int:
        """等它退出，并把三条流收干净。

        单纯写 `self.process = None` 会把管道留在那儿（`ResourceWarning`），
        而下一轮 `start()` 一覆盖引用就再也够不着它们了。
        """
        code = self.process.wait(timeout=10)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None and not stream.closed:
                stream.close()
        self.process = None
        return code

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
        for game_dir in GAMES:
            with self.subTest(game=game_dir.name):
                self.start(game_dir)
                self.send({"type": "facts"})
                facts = self.receive()
                self.assertEqual(facts["type"], "facts_result")
                # 自报的是**域**名（宿主拿它核对 `GameKind`），游戏名另有一栏。
                self.assertEqual(facts["game"], "maze")
                self.assertEqual(facts["game_id"], game_dir.name)
                self.assertTrue(facts["rules_version"])

                self.send({"type": "reset", "seed": 7})
                reset = self.receive()
                self.assertEqual(reset["type"], "reset_result")
                self.assertEqual(reset["step"]["outcome"], "running")
                # 视图大小**按各自清单里的声明**来，不写死 7：
                # 门钥匙是 7×7、传统迷宫是 3×3，而写死的那一个会在另一个游戏上永远不成立。
                view_size = json.loads(manifest_of(game_dir).read_text("utf-8"))["public"]["view_size"]
                self.assertEqual(len(reset["step"]["percept"]["view"]), view_size)
                self.assertTrue(all(len(row) == view_size for row in reset["step"]["percept"]["view"]))

                self.send({"type": "step", "action": {"op": "turn_left"}})
                step = self.receive()
                self.assertEqual(step["type"], "step_result")
                self.assertIn(step["step"]["percept"]["direction"], (0, 1, 2, 3))

                self.send({"type": "close"})
                self.assertEqual(self.receive()["type"], "close_result")
                self.assertEqual(self.finish(), 0)

    def test_two_games_report_two_different_rule_sets(self) -> None:
        # 这一条是"新游戏真的存在"在**进程边界**上的证据：它们自报的规则版本不同。
        # 相同的表现是"我换了个游戏，而账上写的还是原来那一套"。
        versions = {}
        for game_dir in GAMES:
            self.start(game_dir)
            self.send({"type": "facts"})
            versions[game_dir.name] = self.receive()["rules_version"]
            self.send({"type": "close"})
            self.receive()
            self.finish()
        self.assertEqual(len(set(versions.values())), len(versions), versions)

    def test_engine_only_actions_are_refused_at_the_process_boundary(self) -> None:
        self.start(GAMES[0])
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
        self.start(GAMES[0])
        self.send({"type": "launch_missiles"})
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertEqual(response["kind"], "protocol")

        self.send({"type": "reset", "seed": "not-an-integer"})
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertEqual(response["kind"], "protocol")

    def test_an_oversized_frame_ends_the_process_instead_of_desyncing(self) -> None:
        # §11.4：先检查长度再分配，超限关闭。而**关闭的是这个进程**，不是这一条请求——
        # 长度前缀的流超限之后就再也对不齐了，继续读只会把剩下的字节当成长度头。
        self.start(GAMES[0])
        self.send_raw(MAX_MESSAGE_BYTES + 1, b"")
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertEqual(response["kind"], "protocol")
        self.assertNotEqual(self.finish(), 0)

    def test_missing_manifest_is_reported_as_a_frame_not_a_traceback(self) -> None:
        # 参数不对时还没有会话，而宿主在等一条 `facts_result`——它同样接受一条 `error`。
        # 带 traceback 退出的话，stderr 被丢弃，表现是"引擎起不来"，
        # 真因是"参数给错了"，而两者在界面上分不开。
        self.process = subprocess.Popen(
            [sys.executable, "-B", "-X", "utf8", str(DRIVER)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.send({"type": "facts"})
        response = self.receive()
        self.assertEqual(response["type"], "error")
        self.assertIn("--manifest", response["reason"])
        self.assertNotEqual(self.finish(), 0)

    def test_closing_stdin_ends_the_process_cleanly(self) -> None:
        self.start(GAMES[0])
        self.send({"type": "reset", "seed": 7})
        self.receive()
        self.process.stdin.close()
        self.assertEqual(self.finish(), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
