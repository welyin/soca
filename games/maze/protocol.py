"""游戏进程的消息框架：4 字节小端长度 + UTF-8 JSON（实施规格 §11.4）。

每个游戏目录自带一份，不跨目录导入。理由很实际：任何一个游戏目录都应该能被单独搬到
别处运行，因此它不能依赖同级目录里的共享模块。代价是这份框架被复制若干次——它只有
五十行，而且格式一旦冻结就很少改动。

读写都用**精确长度**循环，因为管道上的 `read(n)` 允许返回比 n 更少的字节。用一次
`read(n)` 当成长度完整，是这类实现最常见的静默截断来源。
"""

from __future__ import annotations

import json
import struct
import sys
from typing import Any, BinaryIO

#: 单条消息字节上限。先检查长度再分配，超限直接报错而不是先读进来。
MAX_MESSAGE_BYTES = 256 * 1024

_HEADER = struct.Struct("<I")


class ProtocolError(Exception):
    """框架层错误。会让游戏进程退出，而不是继续用错位的流。"""


def _read_exact(stream: BinaryIO, count: int) -> bytes | None:
    """读满 count 字节。返回 None 表示流在边界处干净结束。"""
    chunks = bytearray()
    while len(chunks) < count:
        chunk = stream.read(count - len(chunks))
        if not chunk:
            if not chunks:
                return None
            raise ProtocolError(f"消息在 {len(chunks)}/{count} 字节处被截断")
        chunks.extend(chunk)
    return bytes(chunks)


def read_message(stream: BinaryIO) -> Any | None:
    """读一条消息。返回 None 表示对端已经正常关闭。"""
    header = _read_exact(stream, _HEADER.size)
    if header is None:
        return None
    (length,) = _HEADER.unpack(header)
    if length > MAX_MESSAGE_BYTES:
        raise ProtocolError(f"声明长度 {length} 超过上限 {MAX_MESSAGE_BYTES}")
    payload = _read_exact(stream, length)
    if payload is None:
        raise ProtocolError("长度头之后没有消息体")
    try:
        return json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProtocolError(f"消息体不是合法 UTF-8 JSON：{error}") from error


def write_message(stream: BinaryIO, message: Any) -> None:
    """写一条消息并立即刷新。"""
    payload = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(payload) > MAX_MESSAGE_BYTES:
        raise ProtocolError(f"待发送消息 {len(payload)} 字节超过上限 {MAX_MESSAGE_BYTES}")
    stream.write(_HEADER.pack(len(payload)))
    stream.write(payload)
    stream.flush()


def stdio_streams() -> tuple[BinaryIO, BinaryIO]:
    """标准输入输出。游戏进程只用这两条流，不开网络、不碰文件。"""
    return sys.stdin.buffer, sys.stdout.buffer
