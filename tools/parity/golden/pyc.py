"""Python 内建语义（round / sum / repr / int / float 解析）的 golden 夹具，供 gate_core::pyc 对拍。"""
from __future__ import annotations

import random
import struct

from _golden import Writer

rng = random.Random(20261014)


def floats(n: int) -> list[float]:
    out = [0.0, -0.0, 0.5, 1.5, 2.5, -2.5, 0.125, 2.675, 1.005, 1e-5, 1e16, 1e15, 123456.0, 1 / 3]
    for _ in range(n):
        kind = rng.random()
        if kind < 0.3:
            out.append(rng.uniform(-1000, 1000))
        elif kind < 0.5:
            out.append(round(rng.uniform(-100, 100), rng.randint(0, 4)) + rng.choice([0, 0.005, 0.0005]))
        elif kind < 0.7:
            out.append(rng.randint(-10**6, 10**6) / rng.choice([2, 4, 8, 10, 100, 1000]))
        else:
            out.append(struct.unpack("<d", struct.pack("<Q", rng.getrandbits(62)))[0])
    return out


with Writer("pyc/round") as w:
    for x in floats(1500):
        w.call(round, x)
        for nd in (0, 1, 2, 3, 4):
            w.call(round, x, nd)

with Writer("pyc/sum") as w:
    for _ in range(600):
        xs = floats(rng.randint(0, 12))[-rng.randint(1, 12):]
        rng.shuffle(xs)
        w.call(sum, xs)

with Writer("pyc/repr") as w:
    for x in floats(2500):
        w.call(repr, x)

with Writer("pyc/parse") as w:
    samples = ["0", "15", " 15 ", "+5", "-5", "1_000", "1__0", "_1", "1_", "12.0", "12.5", "-15.9", "1e3", "1E-2",
               ".5", "5.", "inf", "-Infinity", "nan", "", " ", "abc", "0x10", "1,000", "１２", "3.0e+2", "1.5e", "--1"]
    for s in samples:
        w.call(int, s)
        w.call(float, s)
