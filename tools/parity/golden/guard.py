"""app/guard.py（账号保护与排队）的 golden 夹具：常量 / 设置解析 / 网格 / 操作轨迹。

轨迹 = 种子固定的一串操作，在真实的 Guard 上执行（time.time、random、P、数据库都换成可控的假件），
每步记录：操作与参数、返回值或异常、写库内容、执行后的全部内存状态（时间戳队列用 FNV-1a 摘要）。
Rust 侧用 GuardCore 重放，逐步比较。覆盖：小时 / 10 分钟窗口边界、7 点服务日重置、安静时段
（含跨午夜）、AIMD（429 减半 / 每日 +10 / 天花板 80%）、熔断、抖动（记录每次消耗的随机数）、排队。

注意：本文件里不要写 \\uXXXX 转义；非 ASCII 字符一律用 chr()。
"""
from __future__ import annotations

import asyncio
import copy
import logging
import random
import struct
from datetime import datetime
from zoneinfo import ZoneInfo

from _golden import Writer

import app.guard as G
from app import params
from app.guard import Guard

logging.disable(logging.CRITICAL)
LOOP = asyncio.new_event_loop()


def run(coro):
    return LOOP.run_until_complete(coro)


TZ = ZoneInfo("Asia/Shanghai")
CAT = chr(0x732B)
FULL_5 = chr(0xFF15)


class Clock:
    def __init__(self):
        self.t = 0.0

    def time(self):
        return self.t


CLK = Clock()
G.time = CLK                 # guard 模块里的 `time`（只影响 guard 里的 time.time()）


class RecRandom:
    """random.uniform 的替身：公式与标准库一致（a + (b-a)*random()），并记录每次消耗的随机数。"""

    def __init__(self, seed):
        self.rng = random.Random(seed)
        self.log: list[float] = []

    def uniform(self, a, b):
        r = self.rng.random()
        self.log.append(r)
        return a + (b - a) * r


PDATA: dict = {}
params._load = lambda: PDATA          # P("capacity.*", 默认) 读这里


class GDB:
    """Guard 用到的数据库接口：设置键值 / 账号计数器 / usage_log 查询。"""

    def __init__(self, kv=None):
        self.kv = dict(kv or {})
        self.writes: list[list[str]] = []
        self.counters: dict = {}
        self.hit = False
        self.first = None
        self.rows: list = []
        self._db = self

    async def get_setting(self, key, default=None):
        return self.kv.get(key, default)

    async def set_setting(self, key, value):
        self.kv[key] = str(value)
        self.writes.append([key, str(value)])

    async def set_settings_bulk(self, values):
        for k, v in values.items():
            await self.set_setting(k, v)

    async def get_upstream_counter(self, token_id, day):
        return self.counters.get(token_id, {"images": 0, "v5": 0, "units": 0.0})

    async def execute_fetchall(self, sql, params=()):
        if sql.startswith("SELECT 1 FROM usage_log"):
            return [(1,)] if self.hit else []
        if sql.startswith("SELECT MIN(ts) FROM usage_log"):
            return [(self.first,)]
        if sql.startswith("SELECT ts, images FROM usage_log"):
            return list(self.rows)
        raise AssertionError(sql)


def _fnv_bytes(chunks) -> str:
    h = 0xCBF29CE484222325
    for chunk in chunks:
        for b in chunk:
            h ^= b
            h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return format(h, "016x")


def fnv(q) -> str:
    """时间戳队列摘要：FNV-1a 64，逐个 f64 的小端字节。"""
    return _fnv_bytes(struct.pack("<d", x) for x in q)


def fnv_marks(marks) -> str:
    """cap_marks 摘要：逐条 (f64 时间, i64 上限) 的小端字节。"""
    return _fnv_bytes(struct.pack("<dq", t, c) for t, c in marks)


def state(g: Guard):
    return {
        "v": [g.values[n] for n in G.FIELDS],
        "s": {tok: [len(q), fnv(q)] for tok, q in sorted(g._starts.items())},
        "f": sorted([k, v] for k, v in g.image_inflight.items()),
        "e": [[e["id"], e["key"], e["since"], e["running"]] for e in g.entries],
        "q": g._seq,
        "b": [g.breaker_until, g._breaker_reason],
        "m": [len(g._cap_marks), list(g._cap_marks[-1]) if g._cap_marks else None, fnv_marks(g._cap_marks)],
    }


def attempt(fn):
    try:
        return fn(), None
    except Exception as e:  # noqa: BLE001 — 异常本身就是要对拍的行为
        return None, {"type": type(e).__name__, "msg": str(e)}


def pair(r):
    return None if r is None else [str(r), r.code]


def local_ts(y, mo, d, h, mi, s=0):
    return datetime(y, mo, d, h, mi, s, tzinfo=TZ).timestamp()


# ---------------------------------------------------------------- 常量与设置
with Writer("guard/constants") as w:
    w.record(fn="constants", out={
        "HOUR": G.HOUR, "WINDOW_3H": G.WINDOW_3H, "WINDOW_10M": G.WINDOW_10M, "BURST_DIVISOR": G.BURST_DIVISOR,
        "HOURLY_MIN": G.HOURLY_MIN, "HOURLY_MAX": G.HOURLY_MAX, "HOURLY_STEP": G.HOURLY_STEP, "IDLE_SHARE": G.IDLE_SHARE,
        "FIELDS": [[n, *spec] for n, spec in G.FIELDS.items()],
    })
    # Guard 构造：reset_hour 夹在 0..23；默认值
    for rh in (-5, -1, 0, 1, 7, 23, 24, 99):
        g = Guard(reset_hour=rh)
        w.record(fn="new", args=[rh], out={"reset_hour": g.reset_hour, "values": g.values})

STORED = ["0", "1", "5", "7", "-3", "150", "151", "240", "241", "9999", "1000000", "5.9", "-5.9", "1e3", "1E2", " 7 ", "1_0", "1__0", "abc", "",
          " ", "nan", "NaN", "inf", "-inf", "1e999", "-1e999", "0.5", "0x10", FULL_5, "+3", "23", "24", "100000", "100001", "20000", "20001"]
with Writer("guard/settings") as w:
    rng = random.Random(31)
    for _ in range(400):
        stored = {}
        for n in G.FIELDS:
            if rng.random() < 0.6:
                stored["guard_" + n] = rng.choice(STORED)
        if rng.random() < 0.3:
            stored["guard_other"] = "1"
        # 先给一些字段非默认值，看「没存的键保持现值」
        init = {n: rng.randint(lo, hi) for n, (d, lo, hi, _) in G.FIELDS.items() if rng.random() < 0.4}
        db = GDB(stored)
        g = Guard(db)
        g.values.update(init)
        out, err = attempt(lambda: run(g.load()))
        w.record(fn="load", init=init, stored=stored, out=dict(g.values), err=err)
    bodies = []
    VALS = [True, False, 0, 1, 5, 100, 150, 239, 240, 241, -1, 1.0, 150.0, 150.5, 2.5, "5", "", None, [], [1], {}, 10 ** 30, -10 ** 30,
            2 ** 63, 2 ** 64, 1e30, -0.0, 23, 24, 20000, 20001, 100000, 100001, 30, 31, 100, 3, 4]
    for _ in range(400):
        body = {}
        for n in rng.sample(list(G.FIELDS) + ["bogus", "x"], rng.randint(0, 5)):
            body[n] = rng.choice(VALS)
        bodies.append(body)
    for n in G.FIELDS:
        for v in VALS:
            bodies.append({n: v})
    for body in bodies:
        db = GDB()
        g = Guard(db)
        before = dict(g.values)
        out, err = attempt(lambda: run(g.save(copy.deepcopy(body))))
        w.record(fn="save", args=[body], out=out, err=err, writes=db.writes, values=dict(g.values), before=before)

# `float(await get_setting(k, 0) or 0)` 的读法（on_upstream_429 / adapt_daily 读时间戳和 guard_429_cap_at 都这样）
with Writer("guard/setting_float") as w:
    for raw in [None, "", "0", "0.0", "125", "125.0", " 7 ", "1e3", "1_0", "abc", "nan", "inf", "-inf", "1790000000.123456", "-5", FULL_5, "0x1", "1,5"]:
        db = GDB({} if raw is None else {"k": raw})

        async def read():
            return float(await db.get_setting("k", 0) or 0)

        out, err = attempt(lambda: run(read()))
        w.record(fn="setting_float", args=[raw], out=out, err=err)

# ---------------------------------------------------------------- 网格：安静时段 / 服务日
with Writer("guard/grids") as w:
    base = local_ts(2026, 10, 14, 0, 0)
    quiet_nows = [base + h * 3600 + (h * 977) % 3600 + 0.25 for h in range(24)]      # 北京时间 0..23 点各一个
    w.record(fn="quiet_nows", out=quiet_nows)
    for start in range(24):
        for end in range(24):
            g = Guard(reset_hour=7)
            g.values["quiet_start"], g.values["quiet_end"] = start, end
            w.record(fn="in_quiet_grid", args=[start, end], out=[g.in_quiet(t) for t in quiet_nows])
    nows = [base + k * 2203.25 for k in range(0, 60)]
    for rh in range(24):
        for daily, burst in ((1000, 150), (2400, 100), (0, 150)):
            g = Guard(reset_hour=rh)
            g.values["account_daily_cap"], g.values["daily_burst"] = daily, burst
            useds = [float((k * 37) % (daily + 40)) if daily else float(k) for k in range(len(nows))]
            w.record(fn="paced_grid", args=[rh, daily, burst], nows=nows, useds=useds,
                     elapsed=[g._day_elapsed(t) for t in nows], allowance=[g.paced_allowance(t) for t in nows],
                     minutes=[g.minutes_until_paced(u, t) for u, t in zip(useds, nows)])
    for daily, burst in ((500, 0), (20000, 20000), (1, 0)):
        for rh in (0, 7):
            g = Guard(reset_hour=rh)
            g.values["account_daily_cap"], g.values["daily_burst"] = daily, burst
            useds = [float((k * 37) % (daily + 40)) for k in range(len(nows))]
            w.record(fn="paced_grid", args=[rh, daily, burst], nows=nows, useds=useds,
                     elapsed=[g._day_elapsed(t) for t in nows], allowance=[g.paced_allowance(t) for t in nows],
                     minutes=[g.minutes_until_paced(u, t) for u, t in zip(useds, nows)])
    # 服务日边界附近每一秒
    for rh in (0, 7, 23):
        g = Guard(reset_hour=rh)
        boundary = local_ts(2026, 10, 14, rh, 0)
        edge = [boundary + d for d in (-86400.0, -3601.0, -2.0, -1.0, -0.5, -0.25, 0.0, 0.25, 0.5, 1.0, 2.0, 3599.0, 3600.0, 86399.0, 86400.0)]
        w.record(fn="paced_grid", args=[rh, 1000, 150], nows=edge, useds=[float(i * 70) for i in range(len(edge))],
                 elapsed=[g._day_elapsed(t) for t in edge], allowance=[g.paced_allowance(t) for t in edge],
                 minutes=[g.minutes_until_paced(float(i * 70), t) for i, t in enumerate(edge)])


# ---------------------------------------------------------------- 轨迹
TOKENS = ["token-a1", "token-b2", "tok-" + CAT]
STARTS = {
    "morning_reset": local_ts(2026, 10, 14, 6, 55),
    "pre_quiet": local_ts(2026, 10, 14, 2, 40),
    "noon": local_ts(2026, 10, 14, 11, 59, 30),
    "late": local_ts(2026, 10, 14, 23, 50),
    "evening": local_ts(2026, 10, 13, 21, 30),
    "dawn": local_ts(2026, 10, 14, 6, 59, 50),
}


class Env:
    def __init__(self, name, seed, start, reset_hour=7, values=None, pdata=None, kv=None, tokens=None, tz="Asia/Shanghai", low_used=False, time_scale=1.0):
        self.name = name
        self.low_used = low_used
        self.time_scale = time_scale
        self.rng = random.Random(seed)
        self.t = start
        CLK.t = start
        PDATA.clear()
        PDATA.update(pdata or {})
        self.rr = RecRandom(seed + 1)
        G.random = self.rr
        self.db = GDB(kv)
        self.g = Guard(self.db, tz, reset_hour=reset_hour)
        for k, v in (values or {}).items():
            self.g.values[k] = v
        self.tokens = tokens or TOKENS
        self.steps: list = []
        self.init = {"tz": tz, "reset_hour_arg": reset_hour, "reset_hour": self.g.reset_hour, "values": dict(self.g.values),
                     "kv": dict(self.db.kv), "params": dict(PDATA)}
        self.next_key = 1

    # ---- 时间
    def advance(self):
        r = self.rng.random()
        if r < 0.5:
            dt = self.rng.randint(0, 80) * 0.25
        elif r < 0.75:
            dt = self.rng.uniform(20, 400)
        elif r < 0.87:
            dt = self.rng.uniform(400, 4000)
        elif r < 0.95:
            dt = self.rng.choice([600.0, 3600.0, 10800.0, 86400.0, 599.75, 3599.75, 10799.75])
        else:
            dt = self.rng.uniform(4000, 40000)
        self.t = round(self.t + dt * self.time_scale, 3)
        CLK.t = self.t

    def tok(self):
        return self.rng.choice(self.tokens)

    def at(self, ts):
        self.t = ts
        CLK.t = ts

    def set_value(self, name, v):
        self.g.values[name] = v
        self.steps.append({"op": "set_value", "now": self.t, "name": name, "value": v, "state": state(self.g)})

    def step(self, op, call, **args):
        n = len(self.db.writes)
        rr_before = len(self.rr.log)
        out, err = attempt(call)
        rec = {"op": op, "now": self.t, **args, "out": out, "err": err,
               "writes": [list(x) for x in self.db.writes[n:]], "state": state(self.g)}
        draws = self.rr.log[rr_before:]
        if draws:
            rec["draws"] = list(draws)
        self.steps.append(rec)

    def finish(self, w):
        w.record(fn="trace", name=self.name, init=self.init, steps=self.steps)


def stamp_choice(env):
    r = env.rng.random()
    if r < 0.4:
        return env.t
    if r < 0.6:
        return env.t - env.rng.choice([0.25, 1.0, 5.0, 30.0, 599.0, 600.0, 601.0, 1200.0, 3599.0, 3600.0, 3601.0, 7000.0, 10799.0, 10800.0, 10801.0, 12000.0])
    return round(env.t - env.rng.uniform(0, 700), 3)


def op_record_starts(env):
    tok = env.tok()
    n = env.rng.choice([1, 1, 2, 3, 5, 10, 25, 40, 80, 120, 160])
    mono = env.rng.random() < 0.5
    entries = []
    for _ in range(n):
        stamp = env.t if mono else stamp_choice(env)
        images = env.rng.choice([1, 1, 1, 1, 2, 4, 0, -1])
        entries.append([stamp, images])

    def call():
        for s, im in entries:
            env.g.record_start(tok, now=s, images=im)

    env.step("record_starts", call, token=tok, entries=entries)


def simple(env, op, fn_name, with_token=True, **extra):
    t = env.t
    if with_token:
        tok = env.tok()
        env.step(op, lambda: getattr(env.g, fn_name)(tok, t, **extra), token=tok)
    else:
        env.step(op, lambda: getattr(env.g, fn_name)(t), **{})


def op_hourly_cap(env):
    simple(env, "hourly_cap", "hourly_cap", with_token=False)


def op_ceiling(env):
    simple(env, "hourly_cap_ceiling", "hourly_cap_ceiling", with_token=False)


def op_in_quiet(env):
    simple(env, "in_quiet", "in_quiet", with_token=False)


def op_burst_cap(env):
    simple(env, "burst_cap", "burst_cap", with_token=False)


def op_hour_count(env):
    simple(env, "hour_count", "hour_count")


def op_count_10m(env):
    simple(env, "count_10m", "count_10m")


def op_burst_blocked(env):
    simple(env, "burst_blocked", "burst_blocked")


def op_minutes_free(env):
    simple(env, "minutes_until_free", "minutes_until_free")


def op_minutes_free_10m(env):
    simple(env, "minutes_until_free_10m", "minutes_until_free_10m")


def op_paced_allowance(env):
    simple(env, "paced_allowance", "paced_allowance", with_token=False)


def pick_used(env):
    g = env.g
    allow = g.paced_allowance(env.t)
    daily = g.values["account_daily_cap"]
    r = env.rng.random()
    if env.low_used and r < 0.85:
        return env.rng.choice([0.0, 0.0, 1.0, 5.0])      # 让每日 / 时段检查放行，才走得到每小时和突发窗口
    if r < 0.15:
        return 0.0
    if r < 0.4:
        return max(0.0, allow + env.rng.choice([-2.0, -1.0, -0.5, 0.0, 0.25, 0.5, 1.0, 5.0]))
    if r < 0.55:
        return float(daily + env.rng.choice([-2, -1, 0, 1, 7]))
    return round(env.rng.uniform(0, daily + 50 if daily else 300), 2)


def op_minutes_paced(env):
    used = pick_used(env)
    t = env.t
    env.step("minutes_until_paced", lambda: env.g.minutes_until_paced(used, t), used=used)


def op_block_reason(env):
    tok = env.tok()
    used = pick_used(env)
    units = env.rng.random() < 0.8
    env.db.counters[tok] = {"images": int(used), "v5": 0, "units": used} if units else {"images": int(used)}
    shown = used if units else int(used)
    t = env.t
    env.step("token_block_reason", lambda: pair(run(env.g.token_block_reason(env.db, tok, "d", t))), token=tok, used=shown)


def op_headroom(env):
    toks = env.rng.sample(env.tokens, env.rng.randint(0, len(env.tokens)))
    used = []
    for tk in toks:
        u = pick_used(env)
        has_units = env.rng.random() < 0.8
        env.db.counters[tk] = {"images": int(u), "v5": 0, "units": u} if has_units else {"images": int(u)}
        used.append(u if has_units else int(u))
    t = env.t
    env.step("paced_headroom", lambda: run(env.g.paced_headroom(env.db, toks, "d", t)), tokens=toks, used=used)


def op_429(env):
    t = env.t
    env.step("on_upstream_429", lambda: run(env.g.on_upstream_429(t)))


def op_adapt(env):
    if env.rng.random() < 0.6:
        env.t = round(env.t + env.rng.choice([86400.0, 90000.0, 100000.0, 200000.0]), 3)
        CLK.t = env.t
    env.db.hit = env.rng.random() < 0.7
    env.db.first = env.rng.choice([None, 0, env.t - 100.0, env.t - 30000.0, env.t - 72000.0, env.t - 79200.0, env.t - 90000.0, env.t - 400000.0, env.t + 5.0])
    hit, first, t = env.db.hit, env.db.first, env.t
    env.step("adapt_daily", lambda: run(env.g.adapt_daily(t)), hit=hit, first=first)


def op_set_kv(env):
    key = env.rng.choice(["guard_429_cap_at", "guard_last_upstream_429", "guard_hourly_adapted_at"])
    if key == "guard_429_cap_at":
        val = env.rng.choice([None, "0", "", "100", "100.0", "125", "125.0", "150", "163.5", "90"])
    else:
        val = env.rng.choice([None, "0", "", str(env.t - 30000.0), str(env.t - 100000.0), str(env.t - 86400.0), str(env.t - 86399.75), str(env.t - 5.0)])
    if val is None:
        env.db.kv.pop(key, None)
    else:
        env.db.kv[key] = val
    env.steps.append({"op": "set_kv", "now": env.t, "key": key, "value": val})


def op_trip(env):
    secs = env.rng.choice([0, 1, 30, 300, 300.7, 60.0, -5, 0.5])
    reason = env.rng.choice(["", "", "autopilot: " + CAT, "x"])
    t = env.t
    env.step("trip_breaker", lambda: env.g.trip_breaker(secs, reason, t), seconds=secs, reason=reason)


def op_jitter(env):
    env.step("jitter", lambda: env.g.jitter())


def op_admit(env):
    key = env.rng.randint(1, 6)
    accounts = env.rng.choice([0, 1, 1, 2, 3])
    env.step("admit_image", lambda: pair(env.g.admit_image(key, accounts)), key=key, accounts=accounts)


def op_mark(env):
    key = env.rng.randint(1, 6)
    eid = env.rng.choice([None, None, env.g._seq, max(1, env.g._seq - 1), env.rng.randint(1, 12)])
    env.step("mark_running", lambda: env.g.mark_running(key, eid), key=key, entry_id=eid)


def op_release(env):
    key = env.rng.randint(1, 6)
    eid = env.rng.choice([None, None, env.g._seq, max(1, env.g._seq - 1), env.rng.randint(1, 12)])
    env.step("release_image", lambda: env.g.release_image(key, eid), key=key, entry_id=eid)


def op_queue_view(env):
    key = env.rng.choice([None, 1, 2, 3, 4, 5, 6])
    env.step("queue_view", lambda: env.g.queue_view(key), key=key)


def op_waiting(env):
    env.step("waiting_keys", lambda: env.g.waiting_keys())


def op_site_idle(env):
    key = env.rng.randint(0, 7)
    toks = env.rng.sample(env.tokens, env.rng.randint(0, len(env.tokens)))
    t = env.t
    env.step("site_idle", lambda: env.g.site_idle(key, toks, t), key=key, tokens=toks)


def op_set_value(env):
    name = env.rng.choice(list(G.FIELDS))
    _, lo, hi, _ = G.FIELDS[name]
    if name in ("quiet_start", "quiet_end"):
        v = env.rng.choice([0, 3, 7, 22, 6, 5, 23])
    elif name == "account_hourly_cap":
        v = env.rng.choice([0, 8, 20, 40, 100, 120, 150, 160, 200])
    elif name == "quiet_hourly_cap":
        v = env.rng.choice([0, 0, 5, 20, 50])
    elif name == "interval_jitter":
        v = env.rng.choice([0, 5, 30, 1])
    elif name == "key_image_queue":
        v = env.rng.randint(0, 3)
    elif name == "queue_per_account":
        v = env.rng.choice([0, 1, 2, 5, 10])
    elif name == "account_daily_cap":
        v = env.rng.choice([0, 100, 500, 1000, 2400])
    elif name == "daily_burst":
        v = env.rng.choice([0, 50, 150, 500])
    else:
        v = env.rng.randint(lo, min(hi, 300))
    env.g.values[name] = v
    env.steps.append({"op": "set_value", "now": env.t, "name": name, "value": v, "state": state(env.g)})


def op_save(env):
    body = {}
    for n in env.rng.sample(list(G.FIELDS) + ["bogus"], env.rng.randint(1, 3)):
        if n not in G.FIELDS:
            body[n] = 5
            continue
        _, lo, hi, _ = G.FIELDS[n]
        r = env.rng.random()
        body[n] = env.rng.randint(lo, min(hi, 250)) if r < 0.7 else env.rng.choice([hi + 1, -1, 1.5, True, "3", None, float(lo)])
    env.step("save", lambda: run(env.g.save(copy.deepcopy(body))), body=body)


def op_describe(env):
    env.step("describe", lambda: env.g.describe())


def op_seed_hour(env):
    toks = env.rng.sample(env.tokens, env.rng.randint(1, len(env.tokens)))
    rows = sorted([[round(env.t - env.rng.uniform(0, 10800), 2), env.rng.randint(1, 3)] for _ in range(env.rng.randint(0, 25))])
    if env.rng.random() < 0.3 and rows:
        rows.append(list(rows[0]))          # 重复时间戳：并集里会去重
    env.db.rows = [tuple(r) for r in rows]
    t = env.t
    env.step("seed_hour", lambda: run(env.g.seed_hour(env.db, toks, t)), tokens=toks, rows=rows)


OPS = {
    "record_starts": op_record_starts, "hourly_cap": op_hourly_cap, "ceiling": op_ceiling, "in_quiet": op_in_quiet,
    "burst_cap": op_burst_cap, "hour_count": op_hour_count, "count_10m": op_count_10m, "burst_blocked": op_burst_blocked,
    "minutes_free": op_minutes_free, "minutes_free_10m": op_minutes_free_10m, "paced_allowance": op_paced_allowance,
    "minutes_paced": op_minutes_paced, "block": op_block_reason, "headroom": op_headroom, "429": op_429, "adapt": op_adapt,
    "set_kv": op_set_kv, "trip": op_trip, "jitter": op_jitter, "admit": op_admit, "mark": op_mark, "release": op_release,
    "queue_view": op_queue_view, "waiting": op_waiting, "site_idle": op_site_idle, "set_value": op_set_value, "save": op_save,
    "describe": op_describe, "seed_hour": op_seed_hour,
}

MIX = {
    "general": {"record_starts": 14, "hourly_cap": 3, "ceiling": 3, "in_quiet": 2, "burst_cap": 2, "hour_count": 4, "count_10m": 4,
                "burst_blocked": 5, "minutes_free": 3, "minutes_free_10m": 3, "paced_allowance": 2, "minutes_paced": 2, "block": 14,
                "headroom": 2, "429": 2, "adapt": 3, "set_kv": 2, "trip": 1, "jitter": 3, "admit": 3, "mark": 1, "release": 2,
                "queue_view": 1, "waiting": 1, "site_idle": 3, "set_value": 5, "save": 2, "describe": 2, "seed_hour": 1},
    "aimd": {"record_starts": 6, "block": 6, "429": 14, "adapt": 14, "set_kv": 10, "hourly_cap": 4, "ceiling": 6, "set_value": 3, "save": 2,
             "describe": 2, "site_idle": 2},
    "queue": {"admit": 22, "mark": 10, "release": 16, "queue_view": 8, "waiting": 4, "site_idle": 8, "record_starts": 5, "set_value": 5,
              "save": 3, "describe": 1},
    "paced": {"block": 24, "minutes_paced": 10, "paced_allowance": 6, "headroom": 8, "set_value": 8, "record_starts": 6, "save": 2},
    "burst": {"record_starts": 24, "block": 14, "burst_blocked": 14, "burst_cap": 4, "minutes_free": 8, "minutes_free_10m": 8,
              "hour_count": 6, "count_10m": 6, "set_value": 4, "ceiling": 3},
    "breaker": {"trip": 8, "block": 14, "jitter": 14, "set_value": 8, "describe": 3, "record_starts": 4},
}


def gen(w, name, seed, mix, n_ops, start, reset_hour=7, values=None, pdata=None, kv=None, tz="Asia/Shanghai", low_used=False,
        time_scale=1.0, tokens=None):
    env = Env(name, seed, start, reset_hour, values, pdata, kv, tokens=tokens, tz=tz, low_used=low_used, time_scale=time_scale)
    names = list(mix)
    weights = [mix[k] for k in names]
    for _ in range(n_ops):
        env.advance()
        OPS[env.rng.choices(names, weights)[0]](env)
    env.finish(w)


def h_record(env, tok, entries):
    env.step("record_starts", lambda: [env.g.record_start(tok, now=s, images=im) for s, im in entries] and None,
             token=tok, entries=entries)


def h_probe(env, tok):
    t = env.t
    for op in ("hour_count", "count_10m", "burst_blocked", "minutes_until_free_10m", "minutes_until_free"):
        env.step(op, lambda op=op: getattr(env.g, op)(tok, t), token=tok)
    env.step("burst_cap", lambda: env.g.burst_cap(t))
    env.db.counters[tok] = {"images": 0, "v5": 0, "units": 0.0}
    env.step("token_block_reason", lambda: pair(run(env.g.token_block_reason(env.db, tok, "d", t))), token=tok, used=0.0)


def hand_burst(w):
    """test_burst_guard.py 里的几个形态：宽裕 / 踩线 / 真突发 / 快见底，以及批量请求按张数记。"""
    env = Env("burst_hand", 777, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0, "account_hourly_cap": 100,
                                                         "account_daily_cap": 0, "daily_burst": 0})
    t = env.t
    for label, n, older, span in (("room", 25, 0, 600.0), ("last-night", 40, 0, 600.0), ("burst", 50, 0, 600.0), ("tight", 25, 60, 600.0),
                                  ("n37", 37, 0, 600.0), ("n38", 38, 0, 600.0), ("n39", 39, 0, 600.0), ("n24", 24, 0, 600.0),
                                  ("wait", 25, 0, 500.0), ("edge10m", 30, 0, 600.0), ("all", 100, 0, 3000.0), ("over", 120, 0, 3300.0)):
        tok = "tok-" + label
        if older:
            h_record(env, tok, [[t - 3500 + i * 46, 1] for i in range(older)])
        h_record(env, tok, [[t - i * span / max(1, n), 1] for i in range(n)])
        h_probe(env, tok)
    h_record(env, "tok-batch", [[t, 4]])
    h_probe(env, "tok-batch")
    h_record(env, "tok-batch", [[t - 600.0, 4], [t - 3600.0, 4], [t - 10800.0, 2]])      # 恰好在窗口边界上
    h_probe(env, "tok-batch")
    for dt in (0.0, 0.25, 1.0, 59.0, 60.0, 61.0, 599.0, 600.0, 601.0, 3599.0, 3600.0, 3601.0, 10799.0, 10800.0, 10801.0):
        env.at(t + dt)
        h_probe(env, "tok-batch")
        h_probe(env, "tok-burst")
    env.finish(w)


def hand_burst_tiny(w):
    """很小的每小时上限：10 分钟额度只有 1～4 张，逐张加到超限，覆盖 hard = max(burst+1, ceil(1.5*burst)) 的每个分支。"""
    env = Env("burst_tiny_hand", 4242, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0, "account_daily_cap": 0, "daily_burst": 0})
    for cap in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 16, 20, 40):
        env.set_value("account_hourly_cap", cap)
        tok = f"tok-cap{cap}"
        for _ in range(cap + 3):
            h_record(env, tok, [[env.t, 1]])
            h_probe(env, tok)
    env.finish(w)


def hand_aimd(w):
    """429 减半 + guard_429_cap_at 的 min(prev, old) 取值：前值与当前上限相等 / 大于 / 小于 / 为 0 / 空串。"""
    cases = [(150, "150", {}), (150, "150.0", {}), (140, "125", {}), (130, "150", {}), (150, "0", {}), (150, "", {}), (120, "119.5", {}),
             (101, "101", {}), (100, "150", {}), (99, "150", {}), (150, "149.999", {}), (160, "160", {"capacity.hourly_min": 90}),
             (200, "200.0", {}), (240, "100", {}), (110, "110", {"capacity.hourly_decrease": 0.9})]
    for i, (cap, prev, pdata) in enumerate(cases):
        env = Env(f"aimd_hand{i}", 900 + i, STARTS["noon"], values={"account_hourly_cap": cap, "quiet_start": 0, "quiet_end": 0},
                  pdata=pdata, kv={"guard_429_cap_at": prev})
        for _ in range(4):
            env.advance()
            op_429(env)
        env.finish(w)


def hand_ceiling(w):
    """hourly_cap_ceiling：标记恰好滑出 60 分钟窗口（<= HOUR）的边界。"""
    env = Env("ceiling_hand", 4343, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0, "account_hourly_cap": 160})
    t0 = env.t
    env.step("hourly_cap", lambda: env.g.hourly_cap(t0))
    env.set_value("account_hourly_cap", 100)
    env.at(t0 + 1.0)
    env.step("hourly_cap", lambda: env.g.hourly_cap(t0 + 1.0))
    for dt in (1.0, 2.0, 3599.0, 3600.0, 3600.75, 3601.0, 3601.25, 3602.0, 3602.5, 7200.0):
        env.at(t0 + dt)
        env.step("hourly_cap_ceiling", lambda: env.g.hourly_cap_ceiling(env.t))
    env.finish(w)


def hand_marks_64(w):
    """_cap_marks 是 deque(maxlen=64)：上限反复变化超过 64 次，最旧的被挤掉。"""
    env = Env("marks_64", 4444, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0})
    for i in range(150):
        env.set_value("account_hourly_cap", 100 + (i % 7) * 10)
        env.at(env.t + 7.25)
        env.step("hourly_cap", lambda: env.g.hourly_cap(env.t))
        if i % 5 == 0:
            env.step("hourly_cap_ceiling", lambda: env.g.hourly_cap_ceiling(env.t))
    env.finish(w)


def hand_breaker(w):
    """熔断恰好到期（breaker_until > now 才拦）。"""
    env = Env("breaker_edges", 4545, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0})
    t0 = env.t
    for secs, reason in ((300, ""), (60.5, "手动"), (0, ""), (-3, "")):
        env.at(t0)
        env.step("trip_breaker", lambda: env.g.trip_breaker(secs, reason, t0), seconds=secs, reason=reason)
        for dt in (0.0, 59.75, 60.0, 60.5, 60.75, 299.0, 299.75, 300.0, 300.25):
            env.at(t0 + dt)
            env.db.counters["tok"] = {"images": 0, "v5": 0, "units": 0.0}
            env.step("token_block_reason", lambda: pair(run(env.g.token_block_reason(env.db, "tok", "d", env.t))), token="tok", used=0.0)
        t0 += 1000.0
    env.finish(w)


def hand_adapt_edges(w):
    """adapt_daily 的 86400 秒门槛：恰好满 24 小时 / 差一点；first 刚好覆盖 20 小时。"""
    for i, (d_step, d_429, cover) in enumerate([(86400.0, 90000.0, 79200.0), (86399.75, 90000.0, 79200.0), (90000.0, 86400.0, 79200.0),
                                                (90000.0, 86399.75, 79200.0), (90000.0, 90000.0, 71999.75), (90000.0, 90000.0, 72000.0),
                                                (90000.0, 90000.0, 72000.25), (90000.0, 90000.0, 86400.0), (90000.0, 90000.0, 200000.0)]):
        env = Env(f"adapt_edges{i}", 4600 + i, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0, "account_hourly_cap": 120},
                  kv={})
        t = env.t
        env.db.kv["guard_hourly_adapted_at"] = str(t - d_step)
        env.db.kv["guard_last_upstream_429"] = str(t - d_429)
        env.db.hit = True
        env.db.first = t - cover
        env.steps.append({"op": "set_kv", "now": t, "key": "guard_hourly_adapted_at", "value": env.db.kv["guard_hourly_adapted_at"]})
        env.steps.append({"op": "set_kv", "now": t, "key": "guard_last_upstream_429", "value": env.db.kv["guard_last_upstream_429"]})
        first = env.db.first
        env.step("adapt_daily", lambda: run(env.g.adapt_daily(t)), hit=True, first=first)
        env.finish(w)


def hand_idle(w):
    """site_idle 的阈值恰好相等（used < share * cap * n）。"""
    for i, (cap, share, toks) in enumerate([(100, None, ["a"]), (100, None, ["a", "b"]), (150, None, ["a"]), (40, 0.25, ["a"]), (80, 0.5, ["a", "b"])]):
        env = Env(f"idle_edges{i}", 4700 + i, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0, "account_hourly_cap": cap},
                  pdata={} if share is None else {"capacity.idle_share": share})
        limit = (G.IDLE_SHARE if share is None else share) * cap * len(toks)
        n_stamps = int(limit)
        for k in range(max(0, n_stamps - 2), n_stamps + 3):
            have = len(env.g._starts.get(toks[0], ()))       # 全部记在第一个账号上，总数 = k
            if have < k:
                h_record(env, toks[0], [[env.t, 1]] * (k - have))
            env.step("site_idle", lambda: env.g.site_idle(0, toks, env.t), key=0, tokens=toks)
            env.step("site_idle", lambda: env.g.site_idle(3, toks, env.t), key=3, tokens=toks)
        env.finish(w)


def hand_quiet(w):
    """安静时段边界：3 点开始、7 点结束（也是服务日重置），上限 0（硬停）和 20。"""
    for qcap in (0, 20):
        env = Env(f"quiet_edges_{qcap}", 888 + qcap, STARTS["pre_quiet"],
                  values={"quiet_start": 3, "quiet_end": 7, "quiet_hourly_cap": qcap, "account_hourly_cap": 120})
        for ts in (local_ts(2026, 10, 14, 2, 59, 58), local_ts(2026, 10, 14, 2, 59, 59), local_ts(2026, 10, 14, 3, 0, 0),
                   local_ts(2026, 10, 14, 3, 0, 1), local_ts(2026, 10, 14, 5, 0, 0), local_ts(2026, 10, 14, 6, 59, 59),
                   local_ts(2026, 10, 14, 7, 0, 0), local_ts(2026, 10, 14, 7, 0, 1), local_ts(2026, 10, 14, 7, 30, 0),
                   local_ts(2026, 10, 14, 3, 20, 0), local_ts(2026, 10, 14, 3, 40, 0)):
            env.t = ts
            CLK.t = ts
            tok = TOKENS[0]
            record_entries = [[ts - i * 20.0, 1] for i in range(30)]
            env.step("record_starts", lambda: [env.g.record_start(tok, now=s, images=im) for s, im in record_entries] and None,
                     token=tok, entries=record_entries)
            env.step("in_quiet", lambda: env.g.in_quiet(ts))
            env.step("hourly_cap", lambda: env.g.hourly_cap(ts))
            env.step("hourly_cap_ceiling", lambda: env.g.hourly_cap_ceiling(ts))
            env.step("burst_cap", lambda: env.g.burst_cap(ts))
            env.db.counters[tok] = {"images": 0, "v5": 0, "units": 0.0}
            env.step("token_block_reason", lambda: pair(run(env.g.token_block_reason(env.db, tok, "d", ts))), token=tok, used=0.0)
            env.step("paced_allowance", lambda: env.g.paced_allowance(ts))
            env.step("describe", lambda: env.g.describe())
        env.finish(w)


with Writer("guard/traces") as w:
    hand_burst(w)
    hand_burst_tiny(w)
    hand_aimd(w)
    hand_ceiling(w)
    hand_marks_64(w)
    hand_breaker(w)
    hand_adapt_edges(w)
    hand_idle(w)
    hand_quiet(w)
    # 白天平稳：默认设置，不启用安静时段
    gen(w, "noon_default", 1, MIX["general"], 200, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0}, low_used=True)
    # 夜间安静时段 3-7 点停用（站长 2026-10-13）：从 02:40 跨过 3 点和 7 点（服务日重置）
    gen(w, "quiet_3_7_stop", 2, MIX["general"], 220, STARTS["pre_quiet"], values={"quiet_start": 3, "quiet_end": 7, "quiet_hourly_cap": 0})
    gen(w, "quiet_3_7_slow", 3, MIX["general"], 220, STARTS["pre_quiet"], values={"quiet_start": 3, "quiet_end": 7, "quiet_hourly_cap": 20}, low_used=True)
    # 跨午夜的安静时段（start > end）
    gen(w, "quiet_wrap", 4, MIX["general"], 200, STARTS["late"], values={"quiet_start": 22, "quiet_end": 6, "quiet_hourly_cap": 30}, low_used=True)
    # 7 点服务日重置前后的每日放行
    gen(w, "reset_7_paced", 5, MIX["paced"], 150, STARTS["dawn"], values={"quiet_start": 0, "quiet_end": 0})
    gen(w, "reset_0_paced", 6, MIX["paced"], 150, STARTS["late"], reset_hour=0, values={"account_daily_cap": 2400, "daily_burst": 100})
    gen(w, "reset_5_paced", 7, MIX["paced"], 150, STARTS["morning_reset"], reset_hour=5)
    # 小额上限里的突发窗口
    gen(w, "burst_small_cap", 8, MIX["burst"], 200, STARTS["noon"], values={"account_hourly_cap": 40, "quiet_start": 0, "quiet_end": 0}, low_used=True, time_scale=0.12, tokens=TOKENS[:2])
    gen(w, "burst_default_cap", 9, MIX["burst"], 200, STARTS["evening"], values={"quiet_start": 0, "quiet_end": 0}, low_used=True, time_scale=0.12, tokens=TOKENS[:2])
    gen(w, "burst_tiny_cap", 10, MIX["burst"], 160, STARTS["noon"], values={"account_hourly_cap": 8, "quiet_start": 0, "quiet_end": 0}, low_used=True, time_scale=0.12, tokens=TOKENS[:2])
    # AIMD：429 减半、每日 +10、挨过 429 的天花板；不同的 guard_429_cap_at 初值和私密参数
    gen(w, "aimd_default", 11, MIX["aimd"], 160, STARTS["noon"], values={"quiet_start": 0, "quiet_end": 0})
    gen(w, "aimd_low_cap", 12, MIX["aimd"], 160, STARTS["noon"], values={"account_hourly_cap": 110, "quiet_start": 0, "quiet_end": 0},
        kv={"guard_429_cap_at": "125.0"})
    gen(w, "aimd_params", 13, MIX["aimd"], 160, STARTS["evening"], values={"account_hourly_cap": 130, "quiet_start": 0, "quiet_end": 0},
        pdata={"capacity.hourly_min": 90, "capacity.hourly_decrease": 0.6, "capacity.hourly_max": 180, "capacity.hourly_step": 15},
        kv={"guard_429_cap_at": "150"})
    gen(w, "aimd_params_str", 14, MIX["aimd"], 160, STARTS["noon"], values={"account_hourly_cap": 160, "quiet_start": 0, "quiet_end": 0},
        pdata={"capacity.hourly_min": "100", "capacity.hourly_decrease": "0.25", "capacity.hourly_max": 170.9, "capacity.hourly_step": True},
        kv={"guard_hourly_adapted_at": "0"})
    gen(w, "aimd_manual_below_min", 15, MIX["aimd"], 160, STARTS["noon"], values={"account_hourly_cap": 60, "quiet_start": 0, "quiet_end": 0})
    # 熔断与抖动
    for jit in (0, 5, 30):
        gen(w, f"breaker_jitter_{jit}", 20 + jit, MIX["breaker"], 120, STARTS["noon"], values={"interval_jitter": jit, "quiet_start": 0, "quiet_end": 0})
    # 排队
    gen(w, "queue_default", 30, MIX["queue"], 150, STARTS["noon"])
    gen(w, "queue_tight", 31, MIX["queue"], 150, STARTS["noon"], values={"key_image_queue": 0, "queue_per_account": 2})
    gen(w, "queue_loose", 32, MIX["queue"], 150, STARTS["noon"], values={"key_image_queue": 3, "queue_per_account": 100})
    gen(w, "queue_unlimited", 33, MIX["queue"], 150, STARTS["noon"], values={"queue_per_account": 0})
    # 站点空闲借用的私密参数
    gen(w, "idle_share_param", 34, MIX["queue"], 150, STARTS["noon"], pdata={"capacity.idle_share": 0.25})
    # 其它时区 / 服务日起点
    gen(w, "tz_utc", 35, MIX["general"], 150, STARTS["noon"], reset_hour=3, tz="UTC")
    # 随机混合
    for seed in range(100, 112):
        rng = random.Random(seed)
        values = {"quiet_start": rng.choice([0, 3, 22]), "quiet_end": rng.choice([0, 7, 6]),
                  "account_hourly_cap": rng.choice([20, 100, 150, 240]), "quiet_hourly_cap": rng.choice([0, 20, 60]),
                  "account_daily_cap": rng.choice([0, 300, 1000]), "daily_burst": rng.choice([0, 150]),
                  "interval_jitter": rng.choice([0, 5]), "key_image_queue": rng.randint(0, 3), "queue_per_account": rng.choice([0, 2, 5])}
        gen(w, f"mix{seed}", seed, MIX["general"], 200, rng.choice(list(STARTS.values())), reset_hour=rng.choice([0, 7, 7, 5, 23]), values=values,
            low_used=rng.random() < 0.5)
