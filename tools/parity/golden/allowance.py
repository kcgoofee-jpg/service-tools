"""app/allowance.py（V5 订阅额度缓存）的 golden 夹具：阈值解析 + 操作轨迹。

轨迹 = 种子固定的一串操作（resolve / note_period / period / snapshot / 改阈值），在真实的 AllowanceCache 上执行，
时钟（time.time / time.monotonic）和 HTTP 客户端都换成可控的假件；每步记录返回值、缓存行、写库内容、告警和
HTTP 调用次数。Rust 侧用 AllowanceCore 的 check_cache / finish_resolve / note_period 重放，逐步比较。

注意：本文件里不要写 \\uXXXX 转义；非 ASCII 字符一律用 chr()。
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import random
import sys
import types

import logging

import httpx

from _golden import Writer, enc

from app import allowance

# resolve() 里 `from .nai import default_browser_headers` / `_wait_cleanup`：和判定无关，换成桩，免得依赖 nai 的第三方库
_nai = types.ModuleType("app.nai")
_nai.default_browser_headers = lambda token: {}


async def _wait_cleanup(task):
    await task


_nai._wait_cleanup = _wait_cleanup
sys.modules["app.nai"] = _nai

LOOP = asyncio.new_event_loop()


def run(coro):
    return LOOP.run_until_complete(coro)


class Clock:
    def __init__(self):
        self.wall = 1_790_000_000.0
        self.mono = 5000.0

    def time(self):
        return self.wall

    def monotonic(self):
        return self.mono

    def advance(self, dt):
        self.wall += dt
        self.mono += dt


CLK = Clock()
allowance.time = CLK          # allowance 模块里的 `time`；asyncio 自己的时钟不受影响
logging.disable(logging.CRITICAL)   # log.warning 的告警在 stdout 里没用

CAT = chr(0x732B)
EMOJI = chr(0x1F600)
ARABIC_300 = chr(0x663) + chr(0x660) + chr(0x660)      # 阿拉伯-印度数字 300：isdecimal() 为真，int() 认
FULL_120 = chr(0xFF11) + chr(0xFF12) + chr(0xFF10)     # 全角 120


class KV:
    def __init__(self, kv=None):
        self.kv = dict(kv or {})
        self.writes: list[tuple[str, str]] = []

    async def get_setting(self, key, default=None):
        return self.kv.get(key, default)

    async def set_setting(self, key, value):
        self.kv[key] = str(value)
        self.writes.append((key, str(value)))


# ---------------------------------------------------------------- 假 HTTP
class FakeResp:
    def __init__(self, status, headers, body, read_error=False):
        self.status_code = status
        self.headers = {k.lower(): v for k, v in headers.items()}
        self._body = body
        self._read_error = read_error

    async def aiter_bytes(self, n=8192):
        if self._read_error:
            raise httpx.ReadError("boom")
        for i in range(0, len(self._body), n):
            yield self._body[i:i + n]

    async def aclose(self):
        return None


class FakeClient:
    def __init__(self):
        self.calls = 0
        self.plan = None

    def build_request(self, method, url, headers=None, timeout=None):
        return (method, url)

    async def send(self, request, stream=True, follow_redirects=False):
        self.calls += 1
        plan = self.plan
        CLK.advance(plan["dt"])
        kind = plan["kind"]
        if kind == "error":
            raise httpx.ConnectError("boom")
        if kind == "timeout":
            raise TimeoutError()
        headers = {}
        if plan.get("retry_after") is not None:
            headers["Retry-After"] = plan["retry_after"]
        if "body_size" in plan:
            body = b"x" * plan["body_size"]
        elif "body_b64" in plan:
            body = base64.b64decode(plan["body_b64"])
        else:
            body = plan["body"].encode("utf-8")
        return FakeResp(plan["status"], headers, body, read_error=kind == "read_error")


def sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def norm_row(row):
    """缺失的键 = None = 默认值：去掉 None 和 retry_at==0，两边都这样归一化再比。"""
    if row is None:
        return {}
    return {k: v for k, v in row.items() if not (v is None or (k == "retry_at" and v == 0))}


def sub_body(percent, negative, nps=None, active=True, tier=3, usage_extra=None):
    usage = {"percent": percent, "isNegative": negative}
    if nps is not None:
        usage["timeUntilNextPercent"] = nps
    if usage_extra:
        usage.update(usage_extra)
    return json.dumps({"active": active, "tier": tier, "usage": usage})


class Trace:
    """在一个全新的 AllowanceCache 上顺序执行操作并记录。"""

    def __init__(self, name, init_kv=None):
        self.name = name
        self.db = KV(init_kv)
        self.cache = allowance.AllowanceCache(self.db)
        self.cache.on_low = lambda kind, text, cooldown: self.alerts.append([kind, text, cooldown])
        self.alerts: list = []
        self.client = FakeClient()
        self.steps: list = []
        self.init = {"kv": dict(self.db.kv)}
        CLK.wall, CLK.mono = 1_790_000_000.0, 5000.0

    def _snap(self, op, **fields):
        return {"op": op, "wall": CLK.wall, "mono": CLK.mono, **fields}

    def _writes(self, n_before):
        out = []
        for k, v in self.db.writes[n_before:]:
            out.append({"key": k, "len": len(v), "sha256": sha(v), "text": v if len(v) < 700 else None})
        return out

    def advance(self, dt):
        CLK.advance(dt)

    def set_threshold(self, raw):
        if raw is None:
            self.db.kv.pop(allowance.SETTING, None)
        else:
            self.db.kv[allowance.SETTING] = raw
        self.steps.append(self._snap("set_threshold", raw=raw))

    def resolve(self, token, plan):
        self.alerts.clear()
        self.client.plan = plan
        self.client.calls = 0
        n = len(self.db.writes)
        wall0, mono0 = CLK.wall, CLK.mono
        try:
            res = {"ok": run(self.cache.resolve(self.client, "https://example.invalid/", token, "secret"))}
        except allowance.AllowanceUnavailable as e:
            res = {"err": "AllowanceUnavailable", "msg": str(e)}
        self.steps.append({
            "op": "resolve", "token": token, "wall": wall0, "mono": mono0, "plan": plan,
            "threshold_raw": self.db.kv.get(allowance.SETTING),
            "out": res, "http_calls": self.client.calls, "alerts": list(self.alerts),
            "row": norm_row(self.cache._rows.get(token)), "writes": self._writes(n),
            "wall_after": CLK.wall, "mono_after": CLK.mono})

    def note_period(self, token, seconds):
        n = len(self.db.writes)
        run(self.cache._note_period(token, seconds))
        self.steps.append(self._snap("note_period", token=token, seconds=seconds, writes=self._writes(n)))

    def period(self, token):
        self.steps.append(self._snap("period", token=token, out=self.cache.period(token)))

    def snapshot(self, tokens):
        pool = [types.SimpleNamespace(token_id=t) for t in tokens]
        out = run(self.cache.snapshot(pool))
        self.steps.append(self._snap("snapshot", tokens=tokens, threshold_raw=self.db.kv.get(allowance.SETTING), out=out))

    def finish(self, w):
        w.record(fn="trace", name=self.name, init=self.init, steps=self.steps,
                 final_periods=self.cache._periods)


def response(status=200, retry_after=None, body=None, dt=0.25, **extra):
    plan = {"kind": "response", "status": status, "dt": dt, "retry_after": retry_after}
    if body is not None:
        plan["body"] = body
    plan.update(extra)
    return plan


# ---------------------------------------------------------------- 阈值
THRESHOLD_VALUES = ["20", "7", "100", "101", "0", "1", "007", "999", "1000", "0100", "abc", "", " 5", "5 ", "+5", "-5", "5.0", "٣", "１２",
                    20, 5, 100, 101, 0, -1, 1, True, False, 5.5, 50.0, None, [], [5], {}]
with Writer("allowance/threshold") as w:
    for v in THRESHOLD_VALUES:
        for present in (True,):
            db = KV({allowance.SETTING: v})
            out = run(allowance.read_alert_threshold(db))
            w.record(fn="read_alert_threshold", args=[v], present=True, out=out)
    w.record(fn="read_alert_threshold", args=[None], present=False, out=run(allowance.read_alert_threshold(KV())))

# ---------------------------------------------------------------- 订阅响应解析（每个响应体一条单步轨迹）
GOOD = {"active": True, "tier": 3, "usage": {"percent": 40, "isNegative": False, "timeUntilNextPercent": 3600}}


def variant(**kw):
    d = json.loads(json.dumps(GOOD))
    for k, v in kw.items():
        if k.startswith("u_"):
            if v == "DEL":
                d["usage"].pop(k[2:], None)
            else:
                d["usage"][k[2:]] = v
        elif v == "DEL":
            d.pop(k, None)
        else:
            d[k] = v
    return json.dumps(d)


BODIES = [
    json.dumps(GOOD),
    variant(u_percent=0), variant(u_percent=1), variant(u_percent=100), variant(u_percent=101), variant(u_percent=-1),
    variant(u_percent=5.5), variant(u_percent=5.0), variant(u_percent=True), variant(u_percent="5"), variant(u_percent=None),
    variant(u_percent="DEL"), variant(u_percent=10 ** 12), variant(u_percent=2 ** 63 - 1),
    variant(u_isNegative=True), variant(u_isNegative=0), variant(u_isNegative=1), variant(u_isNegative="true"),
    variant(u_isNegative=None), variant(u_isNegative="DEL"),
    variant(u_timeUntilNextPercent=0), variant(u_timeUntilNextPercent=1), variant(u_timeUntilNextPercent=9_999_999),
    variant(u_timeUntilNextPercent=10_000_000), variant(u_timeUntilNextPercent=-1), variant(u_timeUntilNextPercent=1.5),
    variant(u_timeUntilNextPercent=True), variant(u_timeUntilNextPercent="5"), variant(u_timeUntilNextPercent=None),
    variant(u_timeUntilNextPercent="DEL"), variant(u_timeUntilNextPercent=500.0),
    variant(active=False), variant(active="true"), variant(active=1), variant(active=None), variant(active="DEL"),
    variant(tier=3.0), variant(tier=4), variant(tier=2), variant(tier="3"), variant(tier=True), variant(tier=None), variant(tier="DEL"),
    variant(usage="DEL"), variant(usage=None), variant(usage=[]), variant(usage=[{"percent": 5}]), variant(usage="x"), variant(usage={}),
    variant(extra={"a": [1, 2]}, u_extra=1),
    "[]", "null", "5", '"s"', "true", "", "   ", "{", "not json", '{"usage": {"percent": 5, "isNegative": false}}',
    json.dumps(GOOD, ensure_ascii=False, indent=2), "\n" + json.dumps(GOOD) + "\n",
    json.dumps({"active": True, "tier": 3, "usage": {"percent": 40, "isNegative": False, "note": CAT}}, ensure_ascii=False),
    json.dumps({"active": True, "tier": 3, "usage": {"percent": 40, "isNegative": False}}).replace("40", "4" + "0"),
]
with Writer("allowance/bodies") as w:
    for i, body in enumerate(BODIES):
        t = Trace(f"body{i}")
        t.resolve("tk", response(200, body=body))
        t.finish(w)
    # 带 UTF-8 BOM、非法 UTF-8、过大
    t = Trace("body_bom")
    t.resolve("tk", response(200, body_b64=base64.b64encode(b"\xef\xbb\xbf" + json.dumps(GOOD).encode()).decode()))
    t.finish(w)
    t = Trace("body_bad_utf8")
    t.resolve("tk", response(200, body_b64=base64.b64encode(b"\xff\xfe{}").decode()))
    t.finish(w)
    for size in (65535, 65536, 65537, 70000):
        t = Trace(f"body_size_{size}")
        t.resolve("tk", response(200, body_size=size))
        t.finish(w)
    # 合法 JSON 但紧贴 64 KiB：用很长的无关字段把总长撑到 65536 / 65537
    for total in (65536, 65537):
        base = json.dumps({"active": True, "tier": 3, "usage": {"percent": 40, "isNegative": False}, "pad": ""})
        pad = "p" * (total - len(base))
        text = json.dumps({"active": True, "tier": 3, "usage": {"percent": 40, "isNegative": False}, "pad": pad})
        assert len(text.encode()) == total
        t = Trace(f"body_exact_{total}")
        t.resolve("tk", response(200, body=text))
        t.finish(w)

# ---------------------------------------------------------------- 状态码与 retry-after
RETRY_AFTER = [None, "", "0", "1", "120", "299", "300", "301", "3600", "86400", "86401", "999999", "1000000", "12345678", "abc", "-5", "+5", "1.5",
               " 10", "10 ", ARABIC_300, FULL_120, "1" * 6, "1" * 7, "٣٠٠٠٠٠", "9" * 6]
with Writer("allowance/status") as w:
    for status in (200, 201, 204, 301, 400, 401, 403, 404, 429, 500, 502, 503):
        for ra in RETRY_AFTER:
            t = Trace(f"status{status}")
            t.resolve("tk", response(status, retry_after=ra, body=json.dumps(GOOD)))
            t.finish(w)
    for kind in ("error", "timeout", "read_error"):
        t = Trace(f"kind_{kind}")
        t.resolve("tk", {"kind": kind, "status": 200, "dt": 0.5, "retry_after": None, "body": json.dumps(GOOD)})
        t.finish(w)


# ---------------------------------------------------------------- 随机轨迹
TOKENS = ["token-a1", "token-b2", "tok-" + CAT, "tok-" + EMOJI, "t" * 40]
GAPS = [0.0, 0.5, 1.0, 5.0, 14.999, 15.0, 15.001, 30.0, 59.999, 60.0, 60.001, 120.0, 299.999, 300.0, 300.001, 900.0, 3600.0, 21600.0]
NPS = [None, None, None, 0, 1, 500, 3600, 7200, 14400, 86400, 9_999_999, 10_000_000, -1, 1.5, True, "5"]
PCTS = [0, 1, 2, 3, 10, 19, 20, 21, 33, 50, 99, 100, 101]


def rand_plan(rng, cache, token):
    r = rng.random()
    dt = rng.choice([0.0, 0.1, 0.25, 1.5, 7.75])
    if r < 0.55:
        pct = rng.choice(PCTS)
        neg = rng.random() < 0.15
        return response(200, retry_after=rng.choice([None, None, "5"]), body=sub_body(pct, neg, rng.choice(NPS)), dt=dt)
    if r < 0.65:
        return response(429, retry_after=rng.choice(RETRY_AFTER), body="{}", dt=dt)
    if r < 0.72:
        return response(rng.choice([500, 502, 503, 403]), retry_after=rng.choice([None, "30"]), body="oops", dt=dt)
    if r < 0.78:
        return response(200, body=rng.choice(["{", "[]", json.dumps({"active": False, "tier": 3, "usage": {"percent": 5, "isNegative": False}}),
                                               variant(tier=4), variant(u_percent=-3), variant(u_isNegative="x")]), dt=dt)
    if r < 0.85:
        return response(200, body_size=rng.choice([65536, 65537, 90000]), dt=dt)
    return {"kind": rng.choice(["error", "timeout", "read_error"]), "status": 200, "dt": dt, "retry_after": None,
            "body": sub_body(50, False, 100)}


with Writer("allowance/traces") as w:
    # 手工的周期用例：48 小时窗口边界、四舍五入、500 条上限、非 ASCII 账号
    t = Trace("period_edges", {allowance.PERIOD_KEY: json.dumps({"old": [[1_789_000_000, 77], [1_789_827_200, 88], [1_789_827_201, 99]], CAT: [[1, 2]]})})
    t.period("old")
    t.note_period("old", 900)
    t.period("old")
    t.advance(172799.0)
    t.period("old")
    t.advance(1.0)
    t.period("old")
    t.advance(1.0)
    t.period("old")
    t.note_period(CAT, 55)
    t.snapshot(["old", CAT, "never"])
    t.finish(w)

    t = Trace("period_rounding")
    for frac in (0.0, 0.5, 1.5, 2.5, 3.5, 0.4999, 0.5001, 0.999999):
        CLK.wall = 1_790_000_000.0 + frac
        t.note_period("t1", 100 + int(frac * 10))
        CLK.wall = 1_790_000_100.0 + frac
    t.finish(w)

    t = Trace("period_cap_500")
    for i in range(620):
        t.note_period("t1", 1 + (i * 37) % 9000)
        CLK.advance(1.0)
        if i % 100 == 0:
            t.note_period("t2", 5)
    t.period("t1")
    t.finish(w)

    # 库里的值读不懂（坏 JSON）就当空；形状不对的（数组 / 非对象）旧版直接崩，不对拍
    for raw in (None, "{}", "", "garbage", '{"t1": [[1790000000, 500]]}', '{"t1": [[1789000000, 500], [1789999999, 40000]]}'):
        t = Trace("period_load", {} if raw is None else {allowance.PERIOD_KEY: raw})
        t.period("t1")
        t.note_period("t1", 321)
        t.note_period("a", 1)
        t.period("a")
        t.finish(w)

    # 随机轨迹
    for seed in range(1, 31):
        rng = random.Random(seed * 7919)
        init = {}
        r = rng.random()
        if r < 0.15:
            init[allowance.PERIOD_KEY] = json.dumps({TOKENS[0]: [[int(CLK.wall) - rng.randint(0, 200000), rng.randint(100, 20000)] for _ in range(rng.randint(0, 8))]})
        if rng.random() < 0.6:
            init[allowance.SETTING] = rng.choice(["20", "30", "90", "5", "abc", "100"])
        t = Trace(f"random{seed}", init)
        tokens = rng.sample(TOKENS, rng.randint(1, 4))
        for _ in range(rng.randint(60, 160)):
            # 时间推进：很多步贴着缓存 / 退避的边界
            tok = rng.choice(tokens)
            row = t.cache._rows.get(tok, {})
            r = rng.random()
            if r < 0.4 and row.get("at") is not None:
                CLK.mono = max(CLK.mono, row["at"] + rng.choice([0.0, 14.999, 15.0, 59.999, 60.0, 299.999, 300.0, 300.5]))
                CLK.wall += rng.choice([0.0, 1.0, 20.0])
            elif r < 0.6 and row.get("retry_at"):
                CLK.mono = max(CLK.mono, row["retry_at"] + rng.choice([-0.001, 0.0, 0.001, 1.0, -30.0]))
                CLK.wall += 5.0
            else:
                t.advance(rng.choice(GAPS))
            op = rng.random()
            if op < 0.62:
                t.resolve(tok, rand_plan(rng, t.cache, tok))
            elif op < 0.72:
                t.note_period(tok, rng.choice([1, 300, 900, 3600, 7800, 16000, 40000, 86400]))
            elif op < 0.80:
                t.period(tok)
            elif op < 0.92:
                t.snapshot(rng.sample(tokens + ["unseen"], rng.randint(1, len(tokens) + 1)))
            else:
                t.set_threshold(rng.choice(["20", "30", "90", "5", "abc", "0", None, "100", "1"]))
        t.finish(w)
