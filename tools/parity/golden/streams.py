"""SSE 解码 / 图片事件追踪 / 请求体 JSON 解析的 golden 夹具（app/sse.py、app/image_events.py、app/body.py）。

    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/streams.py

全部是合成数据、固定种子，重复运行输出逐字节一致。生成的夹具：
    sse/decode            SSEDecoder：随机事件流 × 随机切分（含空块、半个 UTF-8、拆开的 \\r\\n、超限、finish）
    sse/encode            encode_sse
    json_body/loads       json.loads(raw)（NaN/Infinity 放行，无深度限制）——图片事件流用的就是这个语义
    json_body/parse_json  body.parse_json(raw)
    json_body/body        body.parse_json_body 的判定（400 文案）
    json_body/deep        深层嵌套（只记结果类别，不存巨大的树）
    image_events/encode   encode_image_event
    image_events/envelope _image_envelope
    image_events/b64      base64.b64decode(s, validate=True)
    image_events/unpackb  msgpack.unpackb(raw=False)
    image_events/tracker  ImageEventTracker：合成流 × 随机切分 × 小限额覆盖
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import math
import random
import re
import struct
import sys

import msgpack
from fastapi import HTTPException

from _golden import Writer, enc

from app import body, image_events, sse
from app.image_events import ImageEventTracker, ImageStreamProtocolError, encode_image_event
from app.sse import SSEDecoder, SSEProtocolError, encode_sse

rng = random.Random(20261015)

# 写源码时不直接出现 \uXXXX 代理对，一律拼出来
BS = "\\"
HI = BS + "ud800"            # JSON 里的孤立高代理转义
HI2 = BS + "ud83d"
LO2 = BS + "ude00"
LO = BS + "udc00"
PAIR = HI2 + LO2             # 一对合法代理 = 😀
SUR_RE = re.compile("[" + chr(0xD800) + "-" + chr(0xDFFF) + "]")


def lossy(s: str) -> tuple[str, bool]:
    """孤立代理 → U+FFFD（Rust 的 String 装不下代理）。"""
    return SUR_RE.sub("�", s), bool(SUR_RE.search(s))


def b64(x: bytes) -> str:
    return base64.b64encode(x).decode()


def chunkify(data: bytes) -> list[bytes]:
    """随机切分：整块 / 逐字节 / 随机切点 / 夹杂空块。"""
    mode = rng.random()
    if not data:
        return [b""] if rng.random() < .5 else []
    if mode < .12:
        out = [data]
    elif mode < .27 and len(data) <= 500:
        out = [data[i:i + 1] for i in range(len(data))]
    else:
        k = rng.randint(1, min(10, len(data)))
        cuts = sorted(rng.sample(range(0, len(data) + 1), k))
        out, prev = [], 0
        for c in cuts + [len(data)]:
            out.append(data[prev:c])
            prev = c
        out = [c for c in out if c or rng.random() < .3]
    if rng.random() < .25:  # 夹杂空块
        for _ in range(rng.randint(1, 3)):
            out.insert(rng.randint(0, len(out)), b"")
    return out


# =============================================================== SSE

BOM8 = b"\xef\xbb\xbf"
TERMS = [b"\n", b"\r\n", b"\r"]
TEXT_POOL = [b"a", b"b", b"x", b"Z", b"0", b"9", b" ", b":", b"{", b"}", b'"', b"-", b"_", b",", "猫".encode(),
             "é".encode(), "😀".encode(), b"data", b"event", b"\\", b"[DONE]"]


def rtext(maxlen: int = 10, noise: bool = False) -> bytes:
    pool = TEXT_POOL + ([bytes([rng.randrange(0x80, 0x100)]), b"\r", b"\n", b"\x00"] if noise else [])
    return b"".join(rng.choice(pool) for _ in range(rng.randint(0, maxlen)))


def gen_sse_stream() -> bytes:
    mixed = rng.random() < .3
    fixed = rng.choice(TERMS)

    def term() -> bytes:
        return rng.choice(TERMS) if mixed else fixed

    noise = rng.random() < .1
    out = bytearray()
    if rng.random() < .2:
        out += BOM8
    for _ in range(rng.randint(0, 6)):
        for _ in range(rng.choice([0, 0, 1, 2])):
            kind = rng.choice(["comment", "id", "retry", "unknown", "colon", "upper", "lead"])
            if kind == "comment":
                out += b":" + rtext(8, noise)
            elif kind == "id":
                out += b"id: " + str(rng.randint(0, 99)).encode()
            elif kind == "retry":
                out += b"retry: 3000"
            elif kind == "unknown":
                out += b"foo: " + rtext(6, noise)
            elif kind == "colon":
                out += b":"
            elif kind == "upper":
                out += b"DATA: ignored"
            else:
                out += b" data: ignored"
            out += term()
        if rng.random() < .4:
            out += rng.choice([b"event: ", b"event:", b"event:  ", b"event"]) + rtext(6, noise) + term()
        for _ in range(rng.choice([0, 1, 1, 1, 2, 3])):
            out += rng.choice([b"data: ", b"data:", b"data:  ", b"data"]) + rtext(10, noise) + term()
        out += term()
        if rng.random() < .2:
            out += term()
    if rng.random() < .3:
        out += rng.choice([b"data: partial", b"event: x", b"da", b":c", b""]) + rng.choice([b"", b"\r", b"\n"])
    return bytes(out)


def run_decoder(event_limit: int, stream_limit: int, ops: list) -> list:
    dec = SSEDecoder(event_limit, stream_limit)
    results = []
    for op in ops:
        if op["op"] == "feed":
            events, err = [], False
            try:
                for item in dec.feed(op["chunk"]):
                    events.append([item[0], item[1]])
            except SSEProtocolError:
                err = True
            results.append({"events": events, "err": err})
        else:
            dec.finish()
            results.append({})
    return results


def min_event_limit(stream: bytes) -> int:
    """单块喂入时刚好不超限的最小 event_limit（用来造恰好卡边界的用例）。"""
    for limit in range(1, len(stream) + 2):
        d = SSEDecoder(limit, 10 ** 9)
        try:
            list(d.feed(stream))
            return limit
        except SSEProtocolError:
            continue
    return len(stream) + 1


def sse_decode_cases(w: Writer, n: int) -> None:
    for _ in range(n):
        stream = gen_sse_stream()
        r = rng.random()
        big = 1 << 30
        if r < .55:
            ev_limit, st_limit = big, big
        elif r < .8:
            m = min_event_limit(stream)
            ev_limit, st_limit = max(1, m + rng.choice([-1, 0, 0, 1])), big
        elif r < .9:
            ev_limit, st_limit = rng.randint(1, 60), big
        else:
            ev_limit = big
            st_limit = max(0, len(stream) + rng.choice([-3, -1, 0, 0, 1, 5]))
        ops = []
        chunks = chunkify(stream)
        for i, c in enumerate(chunks):
            ops.append({"op": "feed", "chunk": c})
            if rng.random() < .08:
                ops.append({"op": "finish"})
        if rng.random() < .3:
            ops.append({"op": "finish"})
            if rng.random() < .5:
                ops.append({"op": "feed", "chunk": gen_sse_stream()[:rng.randint(0, 40)]})
        results = run_decoder(ev_limit, st_limit, ops)
        w.record(fn="sse_decode", event_limit=ev_limit, stream_limit=st_limit, ops=ops, results=results)


def sse_encode_cases(w: Writer, n: int) -> None:
    datas = [b"", b"x", b"\n", b"\n\n", b"a\nb", b"a\n", b"\na", b"a\r\nb", b"a\rb", b"data: x", "猫头鹰".encode(), b"\xff\xfe"]
    events = [b"", b"message", b"final", b"error", b"a b", "猫".encode(), b"\xff"]
    for d in datas:
        for e in events:
            w.call(encode_sse, d, e)
    for _ in range(n):
        w.call(encode_sse, rtext(20, True), rng.choice(events + [rtext(6)]))
    for d in datas:  # 省略 event 参数
        w.call(encode_sse, d)


# =============================================================== JSON

JS_ESC = [BS + '"', BS + BS, BS + "/", BS + "b", BS + "f", BS + "n", BS + "r", BS + "t", BS + "u0041", BS + "u00e9",
          BS + "u732b", BS + "u0000", BS + "u001f", BS + "u007f", BS + "uD83D", BS + "uDE00"]


def ws() -> str:
    return rng.choice(["", "", "", " ", "\n", "\t", "\r\n", "  ", " \t "])


def jstr() -> str:
    parts = ['"']
    for _ in range(rng.randint(0, 7)):
        r = rng.random()
        if r < .45:
            parts.append(rng.choice(["a", "b", "Z", "0", " ", "猫", "é", "😀", ":", "{", ",", "xxxxx", " ", "\x7f"]))
        elif r < .7:
            parts.append(rng.choice(JS_ESC))
        elif r < .78:
            parts.append(PAIR)
        elif r < .84:
            parts.append(rng.choice([HI, LO]))
        elif r < .9:
            parts.append(rng.choice([HI + "A", LO + HI, HI + HI2 + LO2, HI2 + BS + "u0041"]))
        else:
            parts.append(rng.choice(["\x01", "\n", "\t", BS + "x", BS + "u12", BS + "uZZZZ", BS, BS + "u12\"", "\x1f"]))
    parts.append('"')
    return "".join(parts)


def jnum() -> str:
    r = rng.random()
    if r < .25:
        return str(rng.randint(-10 ** 6, 10 ** 6))
    if r < .35:
        return rng.choice(["0", "-0", "1", "-1", "9223372036854775807", "9223372036854775808", "-9223372036854775808",
                           "-9223372036854775809", "18446744073709551615", "18446744073709551616",
                           "170141183460469231731687303715884105727", "170141183460469231731687303715884105728",
                           "-170141183460469231731687303715884105728", "-170141183460469231731687303715884105729"])
    if r < .45:
        return rng.choice(["1.5", "-0.0", "0.0", "1.0", "0.1", "123456789.123456789", "1E5", "1e-5", "1e+2", "1.5E-7",
                           "5e-324", "2.2250738585072014e-308", "1.7976931348623157e308", "1e308", "1e309", "-1e999",
                           "1e-999", "0e0", "0.5e1", "100e-2"])
    if r < .6:
        digits = "".join(rng.choice("0123456789") for _ in range(rng.randint(1, 25)))
        digits = digits.lstrip("0") or "0"
        return rng.choice(["", "-"]) + digits
    if r < .75:
        a = "".join(rng.choice("0123456789") for _ in range(rng.randint(1, 18))).lstrip("0") or "0"
        b = "".join(rng.choice("0123456789") for _ in range(rng.randint(1, 20)))
        e = rng.choice(["", "e" + str(rng.randint(-330, 320)), "E+" + str(rng.randint(0, 20))])
        return rng.choice(["", "-"]) + a + "." + b + e
    return rng.choice(["1", "2", "3", "42", "-7", "255", "65536", "0.25", "3.14159", "1e3", "-2.5e-3"])


def jtext(depth: int = 0) -> str:
    r = rng.random()
    if depth >= 4 or r < .4:
        k = rng.random()
        if k < .15:
            return rng.choice(["null", "true", "false"])
        if k < .6:
            return jnum()
        return jstr()
    if r < .7:
        items = [jtext(depth + 1) for _ in range(rng.randint(0, 4))]
        return "[" + ws() + ("," + ws()).join(it + ws() for it in items) + "]"
    pool = ["a", "b", "c"] if rng.random() < .3 else None
    members = []
    for i in range(rng.randint(0, 4)):
        key = '"' + (rng.choice(pool) if pool else rng.choice(["k", "name", "e", "猫", "x y", ""]) + str(i)) + '"'
        if rng.random() < .08:
            key = jstr()
        members.append(key + ws() + ":" + ws() + jtext(depth + 1))
    return "{" + ws() + ("," + ws()).join(m + ws() for m in members) + "}"


BYTES_POOL = [b",", b":", b'"', b"\\", b"{", b"}", b"[", b"]", b"0", b"1", b"9", b"a", b"e", b"E", b"-", b"+", b".",
              b" ", b"\n", b"\x00", b"\x7f", b"\xff", b"\xed", b"\xa0", b"n", b"t", b"N", b"I", b"u"]


def mutate(b: bytes) -> bytes:
    for _ in range(rng.randint(1, 2)):
        if not b:
            return b
        i = rng.randrange(len(b))
        op = rng.randrange(5)
        if op == 0:
            b = b[:i] + b[i + 1:]
        elif op == 1:
            b = b[:i] + rng.choice(BYTES_POOL) + b[i:]
        elif op == 2:
            b = b[:i] + rng.choice(BYTES_POOL) + b[i + 1:]
        elif op == 3:
            b = b[:i]
        else:
            j = rng.randrange(i, len(b) + 1)
            b = b[:j] + b[i:j] + b[j:]
    return b


def encode_variant(text: str):
    r = rng.random()
    if r < .78:
        return text.encode("utf-8")
    if r < .82:
        return BOM8 + text.encode("utf-8")
    if r < .86:
        return text.encode("utf-16")
    if r < .89:
        return text.encode("utf-16-le")
    if r < .92:
        return text.encode("utf-16-be")
    if r < .94:
        return text.encode("utf-32")
    if r < .96:
        return text.encode("utf-32-le") if rng.random() < .5 else text.encode("utf-32-be")
    if text.startswith("﻿"):
        return text.encode("utf-8")
    return text


HAND_JSON: list = [
    # 标量 / 空白
    "", " ", "\n", "null", "true", "false", " null ", "nul", "nulll", "True", "NULL", "tru", "falsee",
    "0", "-0", "-0.0", "0.0", "1", "-1", "1.0", "1.5", "1e2", "1E2", "1e+2", "1e-2", "0e0", "0.5", "-0.5",
    "01", "-01", "00", "1.", ".5", "-.5", "+1", "1e", "1e+", "1e-", "1.e1", "--1", "-", "1_0", "0x10", "1,", "1 2",
    "NaN", "Infinity", "-Infinity", "-Inf", "nan", "inf", "+Infinity", "-NaN", "Nan", "Infinit", "-Infinit",
    "[NaN]", "[Infinity]", "[-Infinity]", "{\"a\":NaN}", "[1, NaN, }", "[1, }", "{\"a\": -Infinity, \"b\": }",
    "[NaN, 1,]", "[1,]", "[,1]", "[1 2]", "[1,,2]", "{\"a\":1,}", "{,}", "{\"a\"}", "{\"a\":}", "{a:1}", "{'a':1}",
    "{\"a\":1 \"b\":2}", "{1:2}", "[", "]", "{", "}", "[]", "{}", "[ ]", "{ }", "[[]]", "[{}]", "{\"a\":[]}",
    "9223372036854775807", "9223372036854775808", "-9223372036854775808", "-9223372036854775809",
    "18446744073709551615", "18446744073709551616", "123456789012345678901234567890",
    "1e999", "-1e999", "1e-999", "1.7976931348623157e308", "1.7976931348623159e308", "5e-324", "2e-324", "4e-324",
    "123456789012345678901234567890.5", "0.1e1", "1" + "0" * 400 + ".0",
    "9" * 4300, "9" * 4301, "-" + "9" * 4300, "-" + "9" * 4301, "1" + "0" * 4299, "[" + "9" * 4301 + "]",
    "0." + "0" * 5000 + "1", "1e" + "9" * 30, "1e-" + "9" * 30,
    # 字符串
    '""', '"a"', '"\\n"', '"\\u0041"', '"\\u00e9\\u732b"', '"\\/"', '"\\b\\f\\n\\r\\t"', '"\\x"', '"\\u12"', '"\\uZZZZ"',
    '"\\u123"', '"abc', '"abc\\"', '"\t"', '"\n"', '"\x01"', '"\x1f"', '"\x7f"', '" "', "'a'", '"  "',
    '"' + PAIR + '"', '"' + HI + '"', '"' + LO + '"', '"' + HI + LO + '"', '"' + LO + HI + '"', '"' + HI2 + 'A"',
    '"' + HI2 + BS + 'u0041"', '"' + HI + HI2 + LO2 + '"', '"' + HI2 + BS + 'uZZZZ"', '"' + HI2 + BS + 'u12"',
    '"' + HI2 + BS + '"', '"' + HI2, '"' + PAIR, '"x' + PAIR + 'y"', '"猫头鹰😀"', '"\U0001f600"',
    '{"' + HI + '":1}', '{"' + HI + '":1,"' + LO + '":2}', '["' + HI + '","' + HI + '"]',
    # 重复键
    '{"a":1,"a":2}', '{"a":1,"b":2,"a":3}', '{"a":1,"b":2,"a":3,"b":4,"c":5}', '{"a":{"x":1},"a":{"y":2}}',
    '{"":1,"":2}', '{"a":1,"a":[1,2],"a":null}',
    # 嵌套 / 额外数据
    "[1,2,3]", "[1, 2, [3, [4, [5]]]]", '{"a":{"b":{"c":[1,{"d":2}]}}}', "[] x", "{} {}", "[]]", "1 1", "null null",
    '{"a":1}}', "[1]2", " [1] ", "\t[1]\r\n",
    # BOM / 编码
    "﻿{}", "﻿", " ﻿{}", "{﻿}",
    # NUL / 控制字符在 JSON 之外
    "\x00", "{}\x00", "\x0b{}", "\x0c{}", "\xa0{}", "　{}", "{ }", "[1,\x0b2]",
]

HAND_BYTES: list = [
    b"\xef\xbb\xbf{}", b"\xef\xbb\xbf", b"\xef\xbb\xbf\xef\xbb\xbf{}", b"\xef\xbb{}", b"\xef\xbb\xbf[1]",
    b'"\xed\xa0\x80"', b'"\xed\xb0\x80"', b'"\xed\xa0\xbd\xed\xb8\x80"', b'"\xed\xa0\x7f"', b'"\xed\xa0"', b'"\xed"',
    b'"\xc0\x80"', b'"\xff"', b'"\xc3"', b'"\xc3\x28"', b'"\xe0\x80\x80"', b'"\xf0\x80\x80\x80"', b'"\xf4\x90\x80\x80"',
    b'"\xf8\x88\x80\x80\x80"', b'["\xed\xa0\x80", "\xed\xa0\x80"]', b'{"\xed\xa0\x80": 1}', b"\xed\xa0\x80",
    b"{} \xff", b"\xff{}", b'"abc\xe7"',
    # UTF-16 / UTF-32（带 / 不带 BOM、奇数长度、孤立代理）
    b"\xff\xfe{\x00}\x00", b"\xfe\xff\x00{\x00}", b"{\x00}\x00", b"\x00{\x00}", b"{\x00}", b"\x00{", b"{\x00",
    b"[\x001\x00]\x00", b"\x00[\x001\x00]", b"\xff\xfe", b"\xfe\xff", b"\xff\xfe\x00", b"\xff\xfe[",
    b"\xff\xfe\"\x00\x00\xd8\"\x00", b"\xff\xfe\"\x00\x3d\xd8\x00\xde\"\x00", b"\xff\xfe\"\x00\x3d\xd8\"\x00",
    b"\xff\xfe\"\x00\x00\xdc\"\x00", b"\x00\"\xd8\x00\"", b"\"\x00\x00\xd8",
    b"\xff\xfe\x00\x00{\x00\x00\x00}\x00\x00\x00", b"\x00\x00\xfe\xff\x00\x00\x00{\x00\x00\x00}",
    b"{\x00\x00\x00}\x00\x00\x00", b"\x00\x00\x00{\x00\x00\x00}", b"\x00\x00\x00{\x00\x00\x00", b"\x00\x00\x00{\x00\x00",
    b"\xff\xfe\x00\x00\"\x00\x00\x00\x00\xd8\x00\x00\"\x00\x00\x00",
    b"\x00\x00\x00\"\x00\x11\x00\x00\x00\x00\x00\"", b"\xff\xfe\x00\x00\"\x00\x00\x00\xff\xff\x11\x00\"\x00\x00\x00",
    b"\x00\x00\x00", b"\x00\x00", b"\x00", b"\x00\x00\x00\x00", b"\x00\x00\x00\x00\x00",
    b"{}", b"{} ", b" {}", b"\x00{}", b"{}\x00",
]


def jenc_loads(v, ctx: dict):
    """json.loads 结果 → 带类型标记的 JSON（供 pyobj 对拍）。"""
    if v is None:
        return ["n"]
    if v is True or v is False:
        return ["b", v]
    if isinstance(v, int):
        return ["i", str(v)]
    if isinstance(v, float):
        return ["f", enc(v)]
    if isinstance(v, str):
        s, had = lossy(v)
        ctx["lone"] = ctx["lone"] or had
        return ["s", s]
    if isinstance(v, bytes):
        return ["B", b64(v)]
    if isinstance(v, list):
        return ["l", [jenc_loads(x, ctx) for x in v]]
    if isinstance(v, dict):
        out = []
        index = {}
        for k, x in v.items():
            if isinstance(k, str):
                k2, had = lossy(k)
                ctx["lone"] = ctx["lone"] or had
                key = ["s", k2]
            else:
                key = ["B", b64(k)]
            kid = json.dumps(key)
            val = jenc_loads(x, ctx)
            if kid in index:           # 键相撞：位置取首次、值取最后
                out[index[kid]][1] = val
            else:
                index[kid] = len(out)
                out.append([key, val])
        return ["d", out]
    if isinstance(v, msgpack.ExtType):
        return ["x", v.code, b64(v.data)]
    if isinstance(v, msgpack.Timestamp):
        return ["t", v.seconds, v.nanoseconds]
    raise TypeError(type(v))


def depth_of(v) -> int:
    best, stack = 0, [(v, 0)]
    while stack:
        x, d = stack.pop()
        if isinstance(x, dict):
            best = max(best, d + 1)
            stack.extend((c, d + 1) for c in x.values())
        elif isinstance(x, list):
            best = max(best, d + 1)
            stack.extend((c, d + 1) for c in x)
    return best


def dedup_lossy_dict(items):
    """dict 键里的孤立代理会被替换成同一个 U+FFFD：按 Python dict 语义（位置取首次、值取最后）合并。"""
    out = {}
    for k, v in items:
        out[k] = v
    return out


def sign_of(v) -> float:
    return -1.0 if v < 0 else 1.0


def policy(v):
    """parse_json 的「不可约差异」策略：孤立代理 → U+FFFD；超出 int64/uint64 的整数 → 最近的 float；inf → ±MAX。"""
    if v is None or v is True or v is False:
        return v
    if isinstance(v, int):
        if -2 ** 63 <= v < 2 ** 64:
            return v
        try:
            return float(v)
        except OverflowError:
            return math.copysign(sys.float_info.max, sign_of(v))
    if isinstance(v, float):
        if math.isinf(v):
            return math.copysign(sys.float_info.max, v)
        return v
    if isinstance(v, str):
        return lossy(v)[0]
    if isinstance(v, list):
        return [policy(x) for x in v]
    if isinstance(v, dict):
        return dedup_lossy_dict((policy(k), policy(x)) for k, x in v.items())
    raise TypeError(type(v))


def has_surrogate(v) -> bool:
    if isinstance(v, str):
        return lossy(v)[1]
    if isinstance(v, list):
        return any(has_surrogate(x) for x in v)
    if isinstance(v, dict):
        return any(has_surrogate(k) or has_surrogate(x) for k, x in v.items())
    return False


def doc_flags(raw) -> dict:
    """「文档级」标记：文档里出现过这类内容就为真，即使它所在的值后来被重复键覆盖掉（保守，不会漏报）。
    用 parse_int / parse_float / object_pairs_hook 看到文档里的每个 token。"""
    flags = {"lone_surrogate": False, "big_int": False, "float_overflow": False}

    def pi(text):
        v = int(text)
        if not -2 ** 63 <= v < 2 ** 64:
            flags["big_int"] = True
        return v

    def pf(text):
        v = float(text)
        if math.isinf(v):
            flags["float_overflow"] = True
        return v

    def hook(pairs):
        for k, x in pairs:
            if has_surrogate(k) or has_surrogate(x):
                flags["lone_surrogate"] = True
        return dict(pairs)

    v = json.loads(raw, parse_int=pi, parse_float=pf, object_pairs_hook=hook)
    if has_surrogate(v):
        flags["lone_surrogate"] = True
    return flags


def json_cases(wl: Writer, wp: Writer, wb: Writer, n: int) -> None:
    corpus: list = list(HAND_JSON) + list(HAND_BYTES)
    for _ in range(n):
        raw = encode_variant(jtext())
        if rng.random() < .5:
            if isinstance(raw, str):
                raw = raw.encode("utf-8")
            raw = mutate(raw)
        corpus.append(raw)
    for raw in corpus:
        # --- json.loads
        try:
            v = json.loads(raw)
            ctx = {"lone": False}
            if depth_of(v) > 60:
                wl.record(fn="loads", raw=raw, ok=True, depth=depth_of(v))
            else:
                wl.record(fn="loads", raw=raw, ok=True, value=jenc_loads(v, ctx), lone=doc_flags(raw)["lone_surrogate"])
        except Exception as e:  # noqa: BLE001
            wl.record(fn="loads", raw=raw, ok=False, err=type(e).__name__)
        # --- body.parse_json
        try:
            v = body.parse_json(raw)
            wp.record(fn="parse_json", raw=raw, ok=True, value=policy(v), lossy=doc_flags(raw))
        except Exception as e:  # noqa: BLE001
            wp.record(fn="parse_json", raw=raw, ok=False, err=type(e).__name__,
                      msg=str(e) if type(e).__name__ == "ValueError" and not str(e).startswith("Exceeds") else None)
        # --- body.parse_json_body 的判定（只对 bytes，与线上一致）
        if isinstance(raw, bytes):
            try:
                data = asyncio.run(body.parse_json_body(raw))
                wb.record(fn="parse_json_body", raw=raw, ok=True, value=policy(data), lossy=doc_flags(raw))
            except HTTPException as e:
                wb.record(fn="parse_json_body", raw=raw, ok=False, status=e.status_code, detail=e.detail)


def deep_cases(w: Writer) -> None:
    """深层嵌套：只记结果类别。shape: arr / obj；tail: ''（闭合）/ 'x'（闭合后多余字符）/ 'open'（不闭合）。"""
    def build(shape: str, n: int, tail: str) -> bytes:
        if shape == "arr":
            head, leaf, close = b"[" * n, b"1", b"]" * n
        else:
            head, leaf, close = b'{"a":' * n, b"1", b"}" * n
        if tail == "open":
            return head + leaf
        return head + leaf + close + (b" x" if tail == "x" else b"")

    # 先探测本机 Python 的 RecursionError 起点，确保用例落在清晰的区间里
    lo, hi = 1, 200000
    while lo < hi:
        mid = (lo + hi) // 2
        try:
            json.loads(b"[" * mid + b"]" * mid)
            lo = mid + 1
        except RecursionError:
            hi = mid
    assert 5000 < lo < 20000, f"本机 Python 的 JSON 递归上限 {lo} 不在 (5000, 20000)，需调整用例"
    w.record(fn="recursion_probe", first_recursion_depth=lo)
    for shape in ("arr", "obj"):
        for n in (1, 2, 31, 32, 33, 34, 40, 64, 100, 500, 1000, 3000, 5000, lo - 1, lo, lo + 1, 20000, 100000):
            for tail in ("", "x", "open"):
                # 递归上限附近，构造 JSONDecodeError 本身要调 Python 代码，会先撞上 RecursionError：
                # 只有「正常闭合」的用例在边界上有确定的结果
                if tail and abs(n - lo) <= 100:
                    continue
                raw = build(shape, n, tail)
                for name, fn in (("loads", json.loads), ("parse_json", body.parse_json)):
                    try:
                        v = fn(raw)
                        w.record(fn=name, shape=shape, n=n, tail=tail, ok=True, depth=depth_of(v))
                    except Exception as e:  # noqa: BLE001
                        w.record(fn=name, shape=shape, n=n, tail=tail, ok=False, err=type(e).__name__,
                                 msg=str(e) if name == "parse_json" and type(e).__name__ == "ValueError" else None)
    # 混合嵌套 + 空容器在最深处
    for n in (32, 33):
        raw = b"[" * (n - 1) + b"[]" + b"]" * (n - 1)
        for name, fn in (("loads", json.loads), ("parse_json", body.parse_json)):
            try:
                v = fn(raw)
                w.record(fn=name, shape="arr_empty_leaf", n=n, tail="", ok=True, depth=depth_of(v))
            except Exception as e:  # noqa: BLE001
                w.record(fn=name, shape="arr_empty_leaf", n=n, tail="", ok=False, err=type(e).__name__,
                         msg=str(e) if name == "parse_json" else None)


# =============================================================== 图片 / msgpack

def crc4() -> bytes:
    return bytes(rng.randrange(256) for _ in range(4))


def make_png(w_: int | None = None, h_: int | None = None) -> bytes:
    w_ = rng.randint(1, 4096) if w_ is None else w_
    h_ = rng.randint(1, 4096) if h_ is None else h_
    ihdr = struct.pack(">I", 13) + b"IHDR" + struct.pack(">IIBBBBB", w_, h_, 8, 2, 0, 0, 0) + crc4()
    idat_body = bytes(rng.randrange(256) for _ in range(rng.randint(0, 24)))
    idat = struct.pack(">I", len(idat_body)) + b"IDAT" + idat_body + crc4()
    iend = b"\0\0\0\0IEND\xaeB`\x82"
    return b"\x89PNG\r\n\x1a\n" + ihdr + (idat if rng.random() < .7 else b"") + iend


def make_jpeg() -> bytes:
    return b"\xff\xd8\xff" + bytes(rng.randrange(256) for _ in range(rng.randint(0, 20))) + b"\xff\xd9"


def make_webp() -> bytes:
    fourcc = rng.choice([b"VP8 ", b"VP8L", b"VP8X"])
    payload = bytes(rng.randrange(256) for _ in range(rng.randint(0, 20)))
    rest = b"WEBP" + fourcc + payload
    return b"RIFF" + struct.pack("<I", len(rest)) + rest


def make_image() -> bytes:
    return rng.choice([make_png, make_png, make_jpeg, make_webp])()


def bad_image() -> bytes:
    kind = rng.randrange(14)
    if kind == 0:
        return make_png(0, 5)
    if kind == 1:
        return make_png(5, 0)
    if kind == 2:
        return make_png()[:-1]
    if kind == 3:
        return make_png()[:44]
    if kind == 4:
        return make_jpeg()[:-1]
    if kind == 5:
        return make_jpeg()[:-2] + b"\xd9\xff"
    if kind == 6:
        d = bytearray(make_webp())
        d[4] ^= 1
        return bytes(d)
    if kind == 7:
        d = bytearray(make_webp())
        d[12] = ord("X")
        return bytes(d)
    if kind == 8:
        return b"GIF89a" + bytes(30)
    if kind == 9:
        return b"\x89PNG\r\n\x1a\n"
    if kind == 10:
        return b"RIFF"
    if kind == 11:
        return b"\xff\xd8"
    if kind == 12:
        return bytes(rng.randrange(256) for _ in range(rng.randint(1, 50)))
    d = bytearray(make_png())
    d[rng.randrange(8, 24)] ^= 0xFF
    return bytes(d)


def envelope_cases(w: Writer, n: int) -> None:
    for _ in range(n):
        base = make_image() if rng.random() < .6 else bad_image()
        r = rng.random()
        if r < .35 and base:
            d = bytearray(base)
            for _ in range(rng.randint(1, 3)):
                if d:
                    d[rng.randrange(len(d))] = rng.randrange(256)
            base = bytes(d)
        elif r < .5:
            base = base[:rng.randint(0, len(base))]
        w.call(image_events._image_envelope, base)
    for d in (b"", b"\x89", b"RIFF" + bytes(3), b"RIFF" + bytes(4) + b"WEBP", b"\xff\xd8\xff", b"\xff\xd8\xff\xd9"):
        w.call(image_events._image_envelope, d)
    # 长度边界：签名 + IHDR 头 + 宽高 + 填充 + IEND，填充 8 字节时总长 44（< 45 必须拒绝），9 字节时 45
    iend = b"\0\0\0\0IEND\xaeB`\x82"
    for fill in range(0, 14):
        w.call(image_events._image_envelope, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR" + struct.pack(">II", 1, 1) + b"x" * fill + iend)
    # JPEG / WebP 的长度下限：JPEG 至少 5 字节，WebP 至少 20 字节
    for n in range(0, 4):
        w.call(image_events._image_envelope, b"\xff\xd8\xff" + b"\xff\xd9"[:0] + bytes(n) + b"\xff\xd9")
    for n in range(0, 6):
        rest = b"WEBPVP8 " + bytes(n)
        w.call(image_events._image_envelope, b"RIFF" + struct.pack("<I", len(rest)) + rest)


B64_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"


def b64_cases(w: Writer, n: int) -> None:
    fixed = ["", "A", "AA", "AAA", "AAAA", "AA==", "AAA=", "AAAA=", "AAAA==", "AAAA====", "AA=", "A=", "=", "====", "=AAA",
             "AA=A", "AA==AA", "AAAA AAAA", "AA\nAA", "AB==", "AA=\n=", "AAA==", "AAA===", "AA===", "A===", "AAAAA",
             "AAAAAA", "AAAAAAA", "AAAAAAAA=", "AAAAAAAA==", "A-_A", "AA==\n", "é", "AAA\x00", " AAAA", "AAAA ",
             "AAAA\n", "AAA=AAAA", "AA==AAAA", "AAAAAAAAAAAA=", "AAAAAAAAAAA=", "AAAAAAAAAA==", "AAAAAAAAA==="]
    for s in fixed:
        w.call(base64.b64decode, s, validate=True)
    for _ in range(n):
        r = rng.random()
        if r < .45:
            raw = bytes(rng.randrange(256) for _ in range(rng.randint(0, 12)))
            s = b64(raw)
            if rng.random() < .6:
                for _ in range(rng.randint(1, 2)):
                    i = rng.randint(0, len(s))
                    op = rng.randrange(3)
                    ch = rng.choice(list(B64_CHARS[:6]) + ["=", "-", "_", " ", "\n", "\x00", "é", "+", "/"])
                    s = s[:i] + ch + s[i:] if op == 0 else (s[:i] + s[i + 1:] if op == 1 else s[:i] + ch + s[i + 1:])
        elif r < .8:
            s = "".join(rng.choice("AB=+/ -") for _ in range(rng.randint(0, 12)))
        else:
            s = "".join(rng.choice(B64_CHARS + "=") for _ in range(rng.randint(0, 14)))
        w.call(base64.b64decode, s, validate=True)


def rand_json_value(depth: int = 0):
    r = rng.random()
    if depth >= 3 or r < .5:
        if rng.random() < .02:
            return rng.choice(["x" * 255, "x" * 256, "x" * 300])
        return rng.choice([None, True, False, 0, 1, -1, 127, 128, 255, 256, 65535, 65536, 2 ** 31 - 1, 2 ** 32, -32, -33, -128,
                           -129, -32768, -32769, -2 ** 31, -2 ** 31 - 1, 2 ** 63 - 1, 2 ** 63, 2 ** 64 - 1, -2 ** 63,
                           0.0, -0.0, 1.5, 1e16, 1e-5, 0.1, 1 / 3, 1e300, 5e-324, 3.0,
                           "", "a", "é", "猫头鹰", "😀", "a\nb", "\x00", "\x1f\x7f", "q\"u\\o", "x" * 31, "x" * 32])
    sizes = [0, 1, 2, 3, 15, 16, 17] if depth == 0 else [0, 1, 2, 3]
    n = rng.choice(sizes)
    if r < .75:
        return [rand_json_value(depth + 1) for _ in range(n)]
    return {rng.choice(["k", "key", "猫", "event_type", "x"]) + str(i): rand_json_value(depth + 1) for i in range(n)}


def encode_cases(w: Writer, n: int) -> None:
    fixed = [
        {"event_type": "error", "error": "出错了", "message": "出错了", "status_code": 502},
        {"event_type": "final", "samp_ix": 0, "image": "AAAA"},
        {"event_type": "", "a": 1},
        {"event_type": "intermediate", "x": [1, 2.5, None, True], "y": {"z": "é\n"}},
        {"event_type": "e", "ints": [0, 127, 128, 255, 256, 65535, 65536, 2 ** 32 - 1, 2 ** 32, 2 ** 63 - 1, 2 ** 63, 2 ** 64 - 1,
                                    -1, -32, -33, -128, -129, -32768, -32769, -2 ** 31, -2 ** 31 - 1, -2 ** 63]},
        {"event_type": "f", "floats": [0.0, -0.0, 1.0, 1.5, 1e16, 1e-5, 0.1, 1e300, 5e-324, 123456789.123456789, 1e22, 1e21, 1e15]},
        {"event_type": "s", "strs": ["x" * 31, "x" * 32, "x" * 255, "x" * 256, "\x00\x01\x1f\x7f\x80",
                                     "  ", "猫头鹰😀"]},
        {"event_type": "c", "arrays": [[None] * 15, [None] * 16, [None] * 17], "maps": [
            {str(i): i for i in range(15)}, {str(i): i for i in range(16)}, {str(i): i for i in range(17)}]},
    ]
    payloads = list(fixed)
    for _ in range(n):
        p = rand_json_value()
        if not isinstance(p, dict):
            p = {"v": p}
        r = rng.random()
        if r < .85:
            p = {"event_type": rng.choice(["final", "error", "intermediate", "x", "猫", ""]), **p}
        elif r < .93:
            p = {"event_type": rng.choice([1, None, True, ["a"]]), **p}
        payloads.append(p)
    # 没有 event_type 的
    payloads += [{}, {"a": 1}]
    # 超长的只记长度和 SHA-256（夹具里不存 130KB 的字符串）
    for kind, size in (("str", 65535), ("str", 65536), ("arr", 65535), ("arr", 65536)):
        payload = {"event_type": "big", "v": "x" * size if kind == "str" else [None] * size}
        for wire in ("msgpack", "sse"):
            out = encode_image_event(payload, wire)
            w.record(fn="encode_image_event_big", kind=kind, size=size, wire=wire, out_len=len(out),
                     out_sha256=hashlib.sha256(out).hexdigest())
    for idx, p in enumerate(payloads):
        wires = ("msgpack", "sse", "json", "SSE", "", "msgpack ") if idx < len(fixed) or idx >= len(payloads) - 2 else ("msgpack", "sse")
        for wire in wires:
            try:
                out = encode_image_event(p, wire)
                w.record(fn="encode_image_event", payload=p, wire=wire, ok=True, out=out)
            except (KeyError, AttributeError, TypeError) as e:
                w.record(fn="encode_image_event", payload=p, wire=wire, ok=False, err=type(e).__name__)


def pack_py(obj) -> bytes:
    return msgpack.packb(obj, use_bin_type=True)


def msgpack_bodies(n: int) -> list[bytes]:
    out: list[bytes] = []
    simple = [None, True, False, 0, 127, 128, -1, -32, -33, 2 ** 64 - 1, -2 ** 63, 1.5, float("nan"), float("inf"),
              "", "a", "é", b"", b"\x00\xff", [], {}, [1, [2, [3]]], {"a": 1}, {b"a": 1}, {"a": {"b": {"c": []}}}]
    for o in simple:
        out.append(pack_py(o))
    out += [
        b"", b"\xc1", b"\xc0\xc0", b"\x92\x01", b"\x81\x01\x02", b"\x81\xc3\x02", b"\x81\xc0\x02", b"\x81\x92\x01\x02\x02",
        b"\x81\xc4\x01a\x02", b"\x82\xc4\x01a\x01\xa1a\x02", b"\xa2\xff\xfe", b"\xa1\xff", b"\xd9\x01\xc3", b"\xc4\x05ab",
        b"\xd4\x01\x00", b"\xd4\x01", b"\xd5\x7f\x00\x01", b"\xd6\x05\x00\x00\x00\x01", b"\xd7\x10" + b"\x00" * 8,
        b"\xd8\x01" + b"\x00" * 16, b"\xc7\x00\x05", b"\xc7\x03\x05abc", b"\xc8\x00\x01\x05a", b"\xc9\x00\x00\x00\x01\x05a",
        b"\xd6\xff\x00\x00\x00\x01", b"\xd7\xff" + b"\x00" * 8, b"\xd7\xff\x00\x00\x00\x04\x00\x00\x00\x01",
        b"\xd7\xff\xff\xff\xff\xff\xff\xff\xff\xff", b"\xc7\x0c\xff" + b"\x00" * 12, b"\xc7\x0c\xff" + b"\xff" * 12,
        b"\xc7\x0c\xff\x3b\x9a\xc9\xff" + b"\x00" * 8, b"\xc7\x0c\xff\x3b\x9a\xca\x00" + b"\x00" * 8,
        b"\xc7\x03\xff\x00\x00\x00", b"\xc7\x00\xff", b"\xd4\xff\x00", b"\xd5\xff\x00\x00", b"\xd8\xff" + b"\x00" * 16,
        b"\xca\x3f\x80\x00\x00", b"\xca\x7f\xc0\x00\x00", b"\xcb\x7f\xf8\x00\x00\x00\x00\x00\x00", b"\xcb\x00",
        b"\xcc", b"\xcd\x01", b"\xce\x00\x00\x00", b"\xcf" + b"\xff" * 8, b"\xd0\x80", b"\xd1\x80\x00", b"\xd2\x80\x00\x00\x00",
        b"\xd3\x80" + b"\x00" * 7, b"\xdc\x00\x02\x01\x02", b"\xdc\x00\x02\x01", b"\xdd\x00\x00\x00\x01\x01",
        b"\xdd\xff\xff\xff\xff", b"\xde\x00\x01\xa1a\x01", b"\xdf\x00\x00\x00\x01\xa1a\x01", b"\xdf\xff\xff\xff\xff",
        b"\xc4\xff", b"\xc5\x00\x03abc", b"\xc6\x00\x00\x00\x03abc", b"\xc6\xff\xff\xff\xff", b"\xdb\xff\xff\xff\xff",
        b"\xc9\xff\xff\xff\xff\x01", b"\xda\x00\x01a", b"\xdb\x00\x00\x00\x01a", b"\xa1",
        b"\x82\xa1a\x01\xa1a\x02", b"\x82\xa1a\x01\xc4\x01a\x02", b"\x81\xa1a\x81\xa1b\x81\xa1c\x90",
    ]
    for _ in range(n):
        o = rand_json_value()
        if rng.random() < .3 and isinstance(o, (dict, list)):
            o = {"event_type": "final", "samp_ix": 0, "image": o}
        b = pack_py(o)
        if rng.random() < .4:
            b = mutate(b)
        out.append(b)
    # 嵌套层数：非空数组 / map，叶子分别是 nil、空数组、空 map
    for n_ in (1000, 1023, 1024, 1025, 1026, 2000):
        for leaf in (b"\xc0", b"\x90", b"\x80"):
            out.append(b"\x91" * n_ + leaf)
        out.append(b"\x81\xa1a" * n_ + b"\xc0")
        out.append(b"\x81\xa1a" * n_ + b"\x80")
        out.append(b"\x91\x81\xa1a" * (n_ // 2) + b"\xc0")
    return out


def unpackb_cases(w: Writer, n: int) -> None:
    for raw in msgpack_bodies(n):
        try:
            v = msgpack.unpackb(raw, raw=False)
            ctx = {"lone": False}
            d = depth_of(v)
            if d > 60:
                w.record(fn="unpackb", raw=raw, ok=True, depth=d)
            else:
                w.record(fn="unpackb", raw=raw, ok=True, value=jenc_loads(v, ctx))
        except Exception as e:  # noqa: BLE001
            w.record(fn="unpackb", raw=raw, ok=False, err=type(e).__name__)


# =============================================================== tracker

def frame_msgpack(body_: bytes) -> bytes:
    return len(body_).to_bytes(4, "big") + body_


class Gen:
    """造一条合成的图片事件流（SSE 或 msgpack）及其说明。"""

    def __init__(self, wire: str, expected: int):
        self.wire, self.expected = wire, expected
        self.big = 10 ** 30 if wire == "sse" else 2 ** 64 - 1     # msgpack 装不下超过 u64 的整数
        self.items: list[bytes] = []

    # ---- 单个事件的编码
    def sse(self, payload, name: str | None = None, *, term: bytes = b"\n", style: str | None = None) -> bytes:
        style = style or rng.choice(["compact", "default", "ascii", "multi"])
        if isinstance(payload, (bytes, bytearray)):
            data = bytes(payload)
        elif style == "compact":
            data = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode()
        elif style == "ascii":
            data = json.dumps(payload).encode()
        elif style == "multi":
            data = json.dumps(payload, indent=1, ensure_ascii=False).encode()
        else:
            data = json.dumps(payload, ensure_ascii=False).encode()
        out = b""
        if name is not None:
            out += rng.choice([b"event: ", b"event:"]) + name.encode() + term
        for line in re.split(rb"\r\n|\r|\n", data):
            out += rng.choice([b"data: ", b"data:"]) + line + term
        return out + term

    def mp(self, payload) -> bytes:
        return frame_msgpack(payload if isinstance(payload, (bytes, bytearray)) else pack_py(payload))

    def event(self, payload, name: str | None = None, **kw) -> bytes:
        if self.wire == "msgpack":
            return self.mp(payload)
        return self.sse(payload, name, **kw)

    # ---- 内容
    def image_field(self, img: bytes):
        if self.wire == "msgpack" and rng.random() < .5:
            return img            # bin
        return b64(img)

    def final(self, sample: int | None = None, img: bytes | None = None, **over) -> bytes:
        sample = rng.randrange(self.expected) if sample is None else sample
        payload = {"event_type": "final", "samp_ix": sample, "gen_id": rng.randint(1, 999),
                   "image": self.image_field(make_image() if img is None else img)}
        payload.update(over)
        name = "final" if self.wire == "sse" and rng.random() < .6 else None
        return self.event(payload, name)

    def progress(self) -> bytes:
        payload = {"event_type": rng.choice(["intermediate", "progress", "start", "message"]), "samp_ix": rng.randrange(self.expected),
                   "step_ix": rng.randint(0, 40), "image": b64(bytes(rng.randrange(256) for _ in range(rng.randint(0, 20))))}
        name = payload["event_type"] if self.wire == "sse" and rng.random() < .5 else None
        return self.event(payload, name)

    def error(self) -> bytes:
        msg = rng.choice([
            "upstream failed", "  spaced \t out\n message  ", "x" * 200, "猫" * 130, "a b " * 50, "　 uni space x",
            "", 0, 500, 2, 1.5, True, False, None, ["a", "b"], {"k": "v", "n": [1, 2]}, [], {}, [None, 1.5, "é'\"", True],
            {"nested": {"deep": ["x" * 10] * 3}}, float("nan"), float("inf"), 10 ** 30 if self.wire == "sse" else 10 ** 19,
            "tab\tnl\ncr\rend",
        ])
        kind = rng.randrange(8)
        if kind == 0:
            payload = {"event_type": "error", "message": msg}
        elif kind == 1:
            payload = {"error": msg}
        elif kind == 2:
            payload = {"event_type": "error", "error": msg, "message": rng.choice(["m", "", None, 0])}
        elif kind == 3:
            payload = {"statusCode": rng.choice([500, 0, 429, "x", None]), "error": msg}
        elif kind == 4:
            payload = {"event_type": "final", "samp_ix": 0, "image": "AAAA", "error": msg}
        elif kind == 5:
            payload = {"message": msg, "event_type": "error", "statusCode": 502}
        elif kind == 6:
            payload = {"event_type": "intermediate", "error": msg}
        else:
            payload = {"event_type": "error"}
        if self.wire == "msgpack" and rng.random() < .15:
            # 有的键换成 bin / ext / timestamp
            payload = rng.choice([{b"error": "bin-key"}, {"error": msgpack.ExtType(5, b"ab")},
                                  {"error": msgpack.Timestamp(5, 6)}, {"error": b"raw\xff"}, {"error": b"it's"}])
        name = "error" if self.wire == "sse" and rng.random() < .5 else None
        return self.event(payload, name)

    def anomaly(self) -> bytes:
        k = rng.randrange(26)
        W = self.wire
        good_img = b64(make_image())
        if W == "sse":
            raw_cases = [b"not-json", b"[]", b"null", b'"str"', b"123", b"{}", b"", b"{", b'{"a":NaN}', b'{"event_type":"final","x":Infinity}',
                         b'{"event_type":"x","y":-Infinity,"z":1e999}', b'{"a":1,"a":2}', b'{"event_type":"final","event_type":"x"}',
                         b'{"event_type":"progress","n":' + b"9" * 50 + b"}", b'{"event_type":"progress","n":' + b"9" * 4301 + b"}",
                         b"\xef\xbb\xbf{}", b'{"event_type":"progress","s":"' + HI.encode() + b'"}', b"\xff", b'{"event_type":"progress"} x',
                         b'{"event_type":"error","message":"' + HI.encode() + b'ok"}']
            if k == 0:
                return self.sse(rng.choice(raw_cases), rng.choice([None, "final"]))
        else:
            raw_cases = [b"", b"\xc1", b"\xc0", b"\x90", b"\x81\x01\x02", b"\x82\xa1a\x01", b"\x81\xa1a", b"\x81\xa1a\x01\x00",
                         b"\xa1a", b"\x81\xa1a\xa2\xff\xfe", b"\x81\xc3\x02", b"\x81\xa1a" + b"\x91" * 1100 + b"\xc0"]
            if k == 0:
                return self.mp(rng.choice(raw_cases))
        if k == 1:
            return self.event({"event_type": rng.choice([True, 1, None, ["final"], {"a": 1}, 1.5]), "samp_ix": 0, "image": good_img})
        if k == 2:
            return self.event({"event_type": "final", "samp_ix": 0, "image": good_img}, "intermediate" if W == "sse" else None)
        if k == 3:
            return self.event({"event_type": "final", "samp_ix": rng.choice([None, -1, self.expected, self.expected + 5, True, False, 0.0, 1.0, "0", [0], self.big, -self.big // 2]),
                               "image": good_img})
        if k == 4:
            return self.event({"event_type": "final", "image": good_img})
        if k == 5:
            return self.event({"event_type": "final", "samp_ix": 0, "image": rng.choice(["", None, 42, [], {}, True, "not-base64%", "é", " " + good_img, good_img + "\n", good_img[:-1],
                                                                                    "data:image/png;base64," + good_img, good_img + "=", good_img + "==", b"" if W == "msgpack" else "", good_img.replace("A", "-", 1)])})
        if k == 6:
            return self.final(img=bad_image())
        if k == 7:
            return self.event({"event_type": "final", "samp_ix": 0})
        if k == 8:
            return self.event({"samp_ix": 0, "image": good_img}, "final" if W == "sse" else None)
        if k == 9:
            return self.event({"samp_ix": 0, "image": good_img}, "intermediate" if W == "sse" else None)
        if k == 10:
            return self.event({"event_type": "message", "samp_ix": 0, "image": good_img}, "message" if W == "sse" else None)
        if k == 11:
            return self.event({"success": True, "done": True})
        if k == 12:
            return self.event({"event_type": "final", "samp_ix": 0, "image": good_img, "extra": {"nested": [float("nan")] if W == "sse" else [1.5]}})
        if k == 13:
            return self.event({"event_type": "done"}, "done" if W == "sse" else None)
        if k == 14 and W == "sse":
            return self.sse({"event_type": "final", "samp_ix": 0, "image": good_img}, "final", term=rng.choice([b"\r\n", b"\r"]))
        if k == 15:
            return self.event({"event_type": "progress", "pad": "x" * rng.randint(50, 400)})
        if k == 16 and W == "msgpack":
            return self.mp({b"event_type": b"final", b"samp_ix": 0, b"image": good_img})
        if k == 17 and W == "msgpack":
            return self.mp({"event_type": "final", "samp_ix": 0, "image": good_img, "t": msgpack.Timestamp(1, 2), "e": msgpack.ExtType(9, b"zz")})
        if k == 18 and W == "msgpack":
            return pack_py({"event_type": "final", "samp_ix": 0, "image": good_img}) + b""  # 缺长度头
        if k == 19 and W == "msgpack":
            body_ = pack_py({"event_type": "progress"}) + b"\x00"   # 帧内多余字节
            return frame_msgpack(body_)
        if k == 20 and W == "msgpack":
            body_ = pack_py({"event_type": "final", "samp_ix": 0, "image": good_img})
            return frame_msgpack(body_[:-3]) + b""                     # 帧内截断
        if k == 21 and W == "msgpack":
            return (0).to_bytes(4, "big")
        if k == 22 and W == "msgpack":
            return (rng.choice([2 ** 32 - 1, 2 ** 31, 33554433, 100000])).to_bytes(4, "big")
        if k == 23 and W == "msgpack":
            return rng.choice([b"\x00", b"\x00\x00", b"\x00\x00\x00"])
        if k == 24:
            return self.event({"event_type": "final", "samp_ix": rng.randrange(self.expected), "image": good_img, "event_type ": "x"})
        return self.progress()

    def build(self) -> bytes:
        out = bytearray()
        mode = rng.random()
        n = rng.randint(0, 7)
        for _ in range(n):
            r = rng.random()
            if mode < .65:
                piece = (self.final() if r < .45 else self.progress() if r < .75 else
                         self.error() if r < .8 else self.anomaly() if r < .88 else
                         (b": heartbeat\n\n" if self.wire == "sse" else self.progress()))
            else:
                piece = rng.choice([self.final, self.progress, self.error, self.anomaly, self.anomaly])()
            out += piece
        if self.wire == "sse" and rng.random() < .15:
            out = bytearray(BOM8) + out
        if rng.random() < .1 and self.wire == "sse":
            out += rng.choice([b"data: {", b"event: final", b":x"])
        return bytes(out)


def run_tracker(expected: int, wire: str, max_event: int, max_stream: int, ops: list) -> list:
    old = (image_events.MAX_EVENT_BYTES, image_events.MAX_STREAM_BYTES)
    image_events.MAX_EVENT_BYTES, image_events.MAX_STREAM_BYTES = max_event, max_stream
    steps = []
    try:
        t = ImageEventTracker(expected, wire)
        for op in ops:
            step: dict = {}
            if op["op"] in ("frames", "feed"):
                frames, comp, err = [], [], False
                try:
                    if op["op"] == "frames":
                        for fr in t.frames(op["chunk"]):
                            frames.append(fr)
                            comp.append(t.completed_images)
                    else:
                        t.feed(op["chunk"])
                except ImageStreamProtocolError as e:
                    err = True
                    assert str(e) == image_events._SAFE_ERROR
                step["frames"] = frames
                step["completed_at"] = comp
                step["err"] = err
            else:
                t.finish()
            detail, had = lossy(t.error_detail)
            # 紧凑：默认值（空列表 / False / "" / None）省略
            out = {"c": t.completed_images}
            if step.get("frames"):
                out["f"] = step["frames"]
                out["n"] = step["completed_at"]
            if step.get("err"):
                out["e"] = 1
            if t.failed:
                out["x"] = 1
            if detail:
                out["d"] = detail
            if had:
                out["dl"] = 1
            if t.first_image is not None:
                out["i"] = t.first_image
            steps.append(out)
    finally:
        image_events.MAX_EVENT_BYTES, image_events.MAX_STREAM_BYTES = old
    return steps


def tracker_cases(w: Writer, n: int) -> None:
    big_event, big_stream = image_events.MAX_EVENT_BYTES, image_events.MAX_STREAM_BYTES
    for i in range(n):
        wire = "msgpack" if i % 2 else "sse"
        if rng.random() < .03:
            wire = rng.choice(["json", "SSE", ""])    # 非 msgpack 的一律走 SSE
        expected = rng.choice([1, 1, 2, 3, 4])
        stream = Gen("msgpack" if wire == "msgpack" else "sse", expected).build()
        r = rng.random()
        if r < .6:
            max_event, max_stream = big_event, big_stream
        elif r < .8:
            max_event, max_stream = rng.randint(30, 600), big_stream
        elif r < .9:
            max_event, max_stream = big_event, max(0, len(stream) + rng.choice([-5, -1, 0, 0, 1, 10]))
        else:
            max_event, max_stream = rng.randint(40, 500), rng.randint(100, 2500)
        ops = []
        for c in chunkify(stream):
            ops.append({"op": rng.choice(["frames"] * 9 + ["feed"]), "chunk": c})
            if rng.random() < .05:
                ops.append({"op": "finish"})
        if rng.random() < .7:
            ops.append({"op": "finish"})
            if rng.random() < .3:
                ops.append({"op": "frames", "chunk": rng.choice([b"", b"x", stream[:10]])})
        steps = run_tracker(expected, wire, max_event, max_stream, ops)
        w.record(fn="tracker", expected=expected, wire=wire, max_event=max_event, max_stream=max_stream, ops=ops, steps=steps)
    # 深层嵌套（整块喂入，不逐字节切）：unrelated 字段里的深层结构 / error 消息本身是深层 list
    final_sse = Gen("sse", 1).final(0, make_png(), event_type="final")
    final_mp = Gen("msgpack", 1).final(0, make_png())
    cases = []
    for depth in (50, 1000, 3000, 9990, 12000):
        deep = b"[" * depth + b"]" * depth
        cases.append(("sse", 1, b'data: {"event_type":"progress","d":' + deep + b'}\n\n' + final_sse))
        cases.append(("sse", 1, b'data: {"event_type":"error","message":' + deep + b'}\n\n' + final_sse))
        cases.append(("sse", 1, b'data: {"event_type":"error","message":{"k":' + deep + b'}}\n\n'))
    for depth in (500, 1023, 1024, 1025, 1100):
        for leaf in (b"\xc0", b"\x90"):
            deep_body = b"\x81\xa1d" + b"\x91" * depth + leaf
            prog = pack_py({"event_type": "progress"})
            # 把 progress 的 map 头换成 2 个键：event_type + d
            body_ = b"\x82\xaaevent_type\xa8progress\xa1d" + b"\x91" * depth + leaf
            cases.append(("msgpack", 1, frame_msgpack(body_) + final_mp))
            err_body = b"\x82\xaaevent_type\xa5error\xa7message" + b"\x91" * depth + leaf
            cases.append(("msgpack", 1, frame_msgpack(err_body)))
    for wire, expected, stream in cases:
        ops = [{"op": "frames", "chunk": stream}, {"op": "finish"}]
        steps = run_tracker(expected, wire, big_event, big_stream, ops)
        w.record(fn="tracker", expected=expected, wire=wire, max_event=big_event, max_stream=big_stream, ops=ops, steps=steps)
    # 构造参数校验
    for bad in (0, -1, -100):
        try:
            ImageEventTracker(bad)
            w.record(fn="tracker_new", expected=bad, ok=True)
        except ValueError:
            w.record(fn="tracker_new", expected=bad, ok=False)
    for good in (1, 2, 10 ** 9):
        w.record(fn="tracker_new", expected=good, ok=True)


# =============================================================== main

if __name__ == "__main__":
    with Writer("sse/decode") as w:
        sse_decode_cases(w, 1800)
    with Writer("sse/encode") as w:
        sse_encode_cases(w, 300)
    with Writer("json_body/loads") as wl, Writer("json_body/parse_json") as wp, Writer("json_body/body") as wb:
        json_cases(wl, wp, wb, 2200)
    with Writer("json_body/deep") as w:
        deep_cases(w)
    with Writer("image_events/encode") as w:
        encode_cases(w, 250)
    with Writer("image_events/envelope") as w:
        envelope_cases(w, 1500)
    with Writer("image_events/b64") as w:
        b64_cases(w, 2500)
    with Writer("image_events/unpackb") as w:
        unpackb_cases(w, 2200)
    with Writer("image_events/tracker") as w:
        tracker_cases(w, 1600)
    with Writer("streams/consts") as w:
        w.record(fn="consts", max_event_bytes=image_events.MAX_EVENT_BYTES, max_stream_bytes=image_events.MAX_STREAM_BYTES,
                 safe_error=image_events._SAFE_ERROR, media_types=image_events.STREAM_MEDIA_TYPES,
                 max_json_depth=body.MAX_JSON_DEPTH, thread_parse_bytes=body._THREAD_PARSE_BYTES)
