"""json.dumps(obj, ensure_ascii=False) 的 golden 夹具，供 gate_core::jsondump 对拍。

额度 / 自动驾驶把状态序列化成字符串写进 site_settings，所以分隔符、浮点写法、转义规则都必须逐字节相同。
"""
from __future__ import annotations

import json
import random
import struct

from _golden import Writer

rng = random.Random(20261014 + 21)

CHARS = ["a", "Z", "0", " ", '"', "\\", "/", "\n", "\r", "\t", "\b", "\f", "\x00", "\x01", "\x1f", "\x7f", "猫", "头", "鹰",
         "é", " ", " ", " ", "😀", "\U0001f989", "中文", "、", "：", "（", "）"]


def rand_str() -> str:
    return "".join(rng.choice(CHARS) for _ in range(rng.choice([0, 1, 2, 5, 12, 30])))


def rand_float() -> float:
    r = rng.random()
    if r < 0.3:
        return round(rng.uniform(-1000, 1000), rng.randint(0, 4))
    if r < 0.5:
        return rng.choice([0.0, -0.0, 1.0, 0.1, 1e16, 1e15, 1e-5, 1e-4, 123456789012345680.0, 1.5e-7, 5e-324,
                           1.7976931348623157e308, 2.5, 100.0, 1e22, 1e21, 0.30000000000000004])
    if r < 0.8:
        return rng.randint(-10**6, 10**6) / rng.choice([2, 4, 8, 10, 100, 1000, 3, 7])
    f = struct.unpack("<d", struct.pack("<Q", rng.getrandbits(62)))[0]
    return f if f == f and f not in (float("inf"), float("-inf")) else 1.0


def rand_value(depth: int = 0):
    r = rng.random()
    if depth >= 4 or r < 0.5:
        k = rng.random()
        if k < 0.15:
            return None
        if k < 0.25:
            return rng.choice([True, False])
        if k < 0.5:
            return rng.choice([0, 1, -1, 42, 2**31, 2**53, -2**63, 2**63 - 1, rng.randint(-10**9, 10**9)])
        if k < 0.75:
            return rand_float()
        return rand_str()
    if r < 0.75:
        return [rand_value(depth + 1) for _ in range(rng.choice([0, 1, 2, 3, 6]))]
    return {rand_str() or "k": rand_value(depth + 1) for _ in range(rng.choice([0, 1, 2, 4]))}


def dumps(v):
    return json.dumps(v, ensure_ascii=False)


with Writer("jsondump/dumps") as w:
    for _ in range(1500):
        w.call(dumps, rand_value())
