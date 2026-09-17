"""迷宫游戏进程入口：标准输入读请求，标准输出写响应。

这个进程是公开面与私有面的边界。它内部持有 seed、真实地图与 RNG 状态；只有投影之后的
公开感知会离开它（实施规格 §13）。Rust 侧的宿主通过长度前缀 JSON 与它对话（§11.4）。

单独运行：

    .venv/Scripts/python games/maze/game.py

搬到别的目录也能跑：它只依赖同目录的 `protocol.py` 与 `maze_engine.py`，不导入任何
同级游戏目录的东西。
"""

from __future__ import annotations

import sys
from typing import Any

from maze_engine import GAME, RULES_VERSION, DomainError, MazeEngine
from protocol import ProtocolError, read_message, stdio_streams, write_message


def handle(engine: MazeEngine, message: Any) -> dict[str, Any]:
    """处理一条请求，返回一条响应。任何异常都变成 error 响应，不让进程带错继续跑。"""
    if not isinstance(message, dict):
        return {"type": "error", "kind": "protocol", "reason": "消息必须是 JSON 对象"}

    kind = message.get("type")
    try:
        if kind == "facts":
            return {
                "type": "facts_result",
                "game": GAME,
                "rules_version": RULES_VERSION,
                "facts": engine.manifest_facts(),
            }
        if kind == "reset":
            seed = message.get("seed")
            if not isinstance(seed, int) or isinstance(seed, bool):
                return {"type": "error", "kind": "protocol", "reason": "seed 必须是整数"}
            return {"type": "reset_result", "step": engine.reset(seed)}
        if kind == "step":
            action = message.get("action")
            if not isinstance(action, dict):
                return {"type": "error", "kind": "protocol", "reason": "action 必须是对象"}
            return {"type": "step_result", "step": engine.step(action)}
        if kind == "close":
            return {"type": "close_result"}
        return {"type": "error", "kind": "protocol", "reason": f"未知请求类型 {kind!r}"}
    except DomainError as error:
        return {"type": "error", "kind": "domain", "reason": str(error)}
    except Exception as error:  # noqa: BLE001  引擎故障必须变成响应，不能静默退出
        return {"type": "error", "kind": "engine", "reason": f"{type(error).__name__}: {error}"}


def main() -> int:
    inbound, outbound = stdio_streams()
    engine = MazeEngine()
    try:
        while True:
            try:
                message = read_message(inbound)
            except ProtocolError as error:
                write_message(outbound, {"type": "error", "kind": "protocol", "reason": str(error)})
                return 2
            if message is None:
                return 0
            response = handle(engine, message)
            write_message(outbound, response)
            if response.get("type") == "close_result":
                return 0
    finally:
        engine.close()


if __name__ == "__main__":
    sys.exit(main())
