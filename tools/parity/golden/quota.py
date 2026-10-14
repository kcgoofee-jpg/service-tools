"""动态额度（app/quota_algo.py + app/quota_policies.py 的 simple / water_fill）的 golden 夹具。

输出（crates/gate-core/tests/golden/quota/）：
- pure.jsonl     纯函数：water_fill / simple / choose_rate / v5_factor / v5_plan / quota_notice / free_paced_cap
- sqlmath.jsonl  SQL 派生量的纯数学：day_coverage / _yesterday / _v5_used_equiv / quiet_recovery / _allowance / _active_v5 的日期参数
- run.jsonl      整条 run()：用假 db / 假 state 回答查询并记录每一次写入；Rust 的 decide() 必须产出逐字节相同的写入序列与返回值

运行（仓库根目录）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/quota.py
"""
from __future__ import annotations

import json
import math
import random
import sqlite3
from datetime import datetime, timedelta
from types import SimpleNamespace

from _fakes import FakeDb, TZ, fake_log, make_day, run
from _golden import Writer

from app import params as params_mod
from app import quota_algo, quota_policies, site_flags
from app.guard import Guard
from app.params import P
from app.policy import free_paced_cap

rng = random.Random(20261014)

# ---------------------------------------------------------------------------
# 私密参数（P）打桩
# ---------------------------------------------------------------------------
OVERRIDES: dict = {}
params_mod._load = lambda: OVERRIDES

PARAM_DEFAULTS = {          # Rust 字段 -> (私密文件键, 起点值)
    "v5_k_table": ("allocation.v5_k_table", [[90, 1.3], [70, 1.1], [40, 0.9], [20, 0.6]]),
    "v5_k_floor": ("allocation.v5_k_floor", 0.3),
    "min_people": ("allocation.min_people", 10),
    "min_coverage_hours": ("allocation.min_coverage_hours", 20),
    "v5_flood_pct": ("allocation.v5_flood_pct", 85),
    "v5_flood_min_each": ("allocation.v5_flood_min_each", 10),
    "v5_flood_global": ("allocation.v5_flood_global", 260),
    "v5_tighten_below": ("allocation.v5_tighten_below", 40),
}


def set_params(overrides: dict | None) -> None:
    OVERRIDES.clear()
    for field, v in (overrides or {}).items():
        OVERRIDES[PARAM_DEFAULTS[field][0]] = v


def resolved_params() -> dict:
    return {field: P(key, default) for field, (key, default) in PARAM_DEFAULTS.items()}


def rand_overrides() -> dict:
    """随机挑几项覆盖（值的类型贴近真实私密文件：可能是 int 也可能是 float，P 会按起点值类型转换）。"""
    out: dict = {}
    if rng.random() < 0.4:
        out["v5_k_table"] = rng.choice([
            [[95, 1.5], [75, 1.2], [45, 1.0], [25, 0.7]],
            [[90, 1], [50, 1]],
            [],
            [[0, 2.0]],
            [[80.5, 1.25], [60, 1.0]],
        ])
    if rng.random() < 0.3:
        out["v5_k_floor"] = rng.choice([0.2, 0.5, 1, 0.0, 0.35])
    if rng.random() < 0.3:
        out["min_people"] = rng.choice([1, 5, 8, 12, 20, "7", 6.9])
    if rng.random() < 0.3:
        out["min_coverage_hours"] = rng.choice([0, 12, 18, 22, 24, 19.5, "21"])
    if rng.random() < 0.3:
        out["v5_flood_pct"] = rng.choice([80, 88, 90, 95, 100, 85.5])
    if rng.random() < 0.3:
        out["v5_flood_min_each"] = rng.choice([5, 8, 12, 15])
    if rng.random() < 0.3:
        out["v5_flood_global"] = rng.choice([100, 200, 300, 400])
    if rng.random() < 0.3:
        out["v5_tighten_below"] = rng.choice([30, 35, 45, 50, 60])
    return out


# ---------------------------------------------------------------------------
# 纯函数
# ---------------------------------------------------------------------------
def rand_cfg() -> dict:
    cfg = dict(quota_algo.DEFAULTS)
    for k in cfg:
        if rng.random() < 0.15:
            cfg[k] = rng.choice([0, 1, 5, 10, 25, 50, 100, 150, 200, 300, 500])
    return cfg


def rand_usage(budget: float | None = None) -> list[int]:
    n = rng.choice([0, 0, 1, 2, 3, 5, 8, 12, 20, 30])
    out = []
    for _ in range(n):
        r = rng.random()
        if r < 0.05:
            out.append(rng.choice([0, -1, -5]))
        elif r < 0.5:
            out.append(rng.randint(1, 60))
        else:
            out.append(rng.randint(1, 400))
    return out


def gen_pure() -> None:
    with Writer("quota/pure") as w:
        # water_fill：预算既有整数也有小数
        for _ in range(700):
            usage = rand_usage()
            budget = rng.choice([rng.randint(0, 3000), rng.randint(0, 3000), rng.uniform(0, 3000), 0, 1, sum(usage)])
            w.call(quota_policies.water_fill, usage, budget)
        for usage in ([], [0], [5], [5, 5], [10, 20, 30], [1, 1, 1, 100]):
            for budget in (0, 1, 10, 30, 36, 37, 100):
                w.call(quota_policies.water_fill, usage, budget)

        # simple：prev 的 a 为 0 / 缺失时退回 quota_target_avg
        for _ in range(800):
            cfg = rand_cfg()
            usage = rand_usage()
            budget = rng.choice([rng.randint(0, 3000), rng.randint(0, 600), 0])
            if usage and rng.random() < 0.3:      # 在 95% 门槛附近
                tot = sum(usage)
                budget = rng.choice([int(tot / 0.95), int(tot / 0.95) + 1, int(tot / 0.95) - 1, tot])
            prev = rng.choice([{}, {"a": rng.randint(1, 320)}, {"a": rng.randint(1, 320)}, {"a": 0}, {"a": None}])
            if usage and rng.random() < 0.3 and prev.get("a"):
                usage[rng.randrange(len(usage))] = prev["a"] + rng.choice([-1, 0, 1])
            w.call(quota_policies.simple, prev, usage=usage, budget=budget, cfg=cfg)

        # choose_rate
        points = [None, 0.0, 0.5, 0.99, 1.0, 1.0001, 2.5, 5.0, 5.0001, 7.5, 10.0, 14.9, 15.0, 15.0001, 20.0, -1.0]
        for _ in range(1000):
            def pick():
                return rng.choice(points) if rng.random() < 0.6 else (None if rng.random() < 0.1 else round(rng.uniform(0, 22), rng.randint(0, 3)))
            api, measured = pick(), pick()
            if api is not None and measured is not None and rng.random() < 0.3:     # 50% 对不上的边界
                api = round(measured * rng.choice([1.5, 0.5, 1.4999, 1.5001, 0.50001, 2.0, 1.0]), rng.randint(0, 4))
            w.call(quota_algo.choose_rate, api, measured)

        # free_paced_cap
        for a in list(range(-3, 140)) + [200, 300, 1000]:
            w.call(free_paced_cap, a)

        # v5_factor / v5_plan（带私密参数覆盖）
        def v5_factor_p(percent, params):
            set_params({k: v for k, v in params.items()})
            return quota_algo.v5_factor(percent)

        def v5_plan_p(percent, rate, active, lo, hi, params):
            set_params({k: v for k, v in params.items()})
            return quota_algo.v5_plan(percent, rate, active, lo=lo, hi=hi)
        v5_factor_p.__name__, v5_plan_p.__name__ = "v5_factor", "v5_plan"

        pcts = [None, 0.0, 19.99, 20.0, 39.999, 40.0, 69.9, 70.0, 89.99, 90.0, 100.0, 85.0, 84.9, 97.3, 55.5]
        for _ in range(400):
            pct = rng.choice(pcts) if rng.random() < 0.6 else round(rng.uniform(0, 100), rng.randint(0, 3))
            ov = rand_overrides() if rng.random() < 0.4 else {}
            set_params(ov)
            params = resolved_params()
            w.call(v5_factor_p, pct, params)
        for _ in range(1000):
            pct = rng.choice(pcts) if rng.random() < 0.6 else round(rng.uniform(0, 100), rng.randint(0, 3))
            rate = rng.choice([None, 0, 0.0, -2.0, 1.0, 5.0, 11.0, 15.0, 4.37]) if rng.random() < 0.5 else round(rng.uniform(0, 20), rng.randint(0, 3))
            active = rng.choice([0, 1, 3, 9, 10, 11, 14, 25, 80])
            lo = rng.choice([3, 3, 3, 1, 10, 0])
            hi = rng.choice([30, 30, 30, 5, 100])
            ov = rand_overrides() if rng.random() < 0.4 else {}
            set_params(ov)
            params = resolved_params()
            w.call(v5_plan_p, pct, rate, active, lo, hi, params)
        set_params({})

        # quota_notice：文案逐字节对拍
        for _ in range(1500):
            a = rng.choice([150, 100, 126, 127, 125, 0, -3, 300, 30, rng.randint(1, 400)])
            kind = rng.random()
            if kind < 0.05:
                v5 = {}
            elif kind < 0.1:
                v5 = {"each": None, "global": None}
            else:
                v5 = {"each": rng.choice([0, 3, 9, 10, 11, 18, 30, rng.randint(0, 40)]),
                      "global": rng.choice([0, 100, 130, 260, 5, rng.randint(0, 600)])}
                if rng.random() < 0.1:
                    v5["each"] = float(v5["each"]) + rng.choice([0.0, 0.5])
            members = rng.choice([0, 1, 5, 14, 30, 40, rng.randint(0, 100)])
            today = rng.choice(["2026-10-14", "2026-01-02", "2026-12-31", "2027-03-09"])
            pct = rng.choice([None, 0.0, 84.9, 85.0, 85.5, 86.5, 90.5, 97.3, 99.5, 100.0, 12.5, 0.5, 1.5, 2.5]) if rng.random() < 0.7 else round(rng.uniform(0, 100), rng.randint(0, 3))
            rk = rng.random()
            if rk < 0.3:
                review = None
            elif rk < 0.35:
                review = {}
            elif rk < 0.4:
                review = {"reasons": ["x"]}                       # 没有 from/to：None == None
            else:
                f = [rng.choice([100, 150, 175]), rng.choice([100, 150, 175])]
                t = list(f) if rng.random() < 0.3 else [rng.choice([100, 150, 175, 200]), rng.choice([100, 150, 175, 200])]
                review = {"from": f, "to": t, "reasons": rng.choice([["a"], ["每人每天 150 张。还有余量"], ["甲", "乙"], []])}
            reset_hour = rng.choice([7, 7, 7, 0, 5, 23, rng.randint(0, 23)])
            anlas = rng.choice([0, 0, 50, 25, -5, 1])
            w.call(quota_algo.quota_notice, a, v5, members, today=today, pct=pct, review=review,
                   reset_hour=reset_hour, anlas=anlas)


# ---------------------------------------------------------------------------
# SQL 派生量的纯数学（用假 db 调旧的 async 函数）
# ---------------------------------------------------------------------------
def rand_day() -> str:
    return (datetime(2026, 9, 1) + timedelta(days=rng.randint(0, 150))).strftime("%Y-%m-%d")


def day_start(day: str, rh: int) -> float:
    import time
    return time.mktime(time.strptime(day, "%Y-%m-%d")) + rh * 3600


def gen_sqlmath() -> None:
    with Writer("quota/sqlmath") as w:
        # day_coverage
        def day_coverage(first, day, reset_hour):
            db = FakeDb({}, [], lambda sql, p: ("min_ts", [(first,)]))
            return run(quota_algo.day_coverage(db, day, reset_hour))
        for _ in range(700):
            day = rand_day()
            rh = rng.choice([7, 7, 0, 5, 23, rng.randint(0, 23)])
            start = day_start(day, rh)
            kind = rng.random()
            if kind < 0.07:
                first = None
            elif kind < 0.3:
                first = start - rng.uniform(0, 5 * 86400)
            elif kind < 0.4:
                first = start + 86400 - rng.choice([20 * 3600, 20 * 3600 - 1, 20 * 3600 + 1, 18 * 3600, 0, 3600])
            elif kind < 0.5:
                first = start + 86400 + rng.uniform(0, 3600)           # 一天结束之后才开始记录 → 0
            else:
                first = start + rng.uniform(0, 86400)
            if first is not None and rng.random() < 0.3:
                first = float(int(first))
            w.call(day_coverage, first, day, rh)

        # _cfg：DEFAULTS 各键的解析（非法值回到起点值，inf 抛 OverflowError）
        def cfg_fn(settings):
            db = FakeDb(settings, [], lambda sql, p: ("x", []))
            return run(quota_algo._cfg(db))
        cfg_fn.__name__ = "_cfg"
        for _ in range(500):
            settings = {}
            for name in quota_algo.DEFAULTS:
                if rng.random() < 0.5:
                    settings[name] = rng.choice(["0", "1", "10", "150", " 7 ", "7.9", "-3", "1e2", "abc", "", "nan", "inf", "-inf",
                                                 "1_0", "٣", "99999999999", "0.0", "-0.5"])
            w.call(cfg_fn, settings)

        # _yesterday
        def gen_yesterday() -> dict:
            n = rng.choice([0, 1, 3, 6, 12])
            counters = []
            for _ in range(n):
                images = rng.choice([0, 1, 20, 100, 150, rng.randint(0, 400)])
                daily = rng.choice([0, 100, 150, 150, 175, rng.randint(1, 300)])
                legacy = rng.choice([0, images, daily, max(0, daily - 1), daily + 1, rng.randint(0, images or 1)])
                counters.append({"images": images, "daily_images": daily, "legacy_free_images": legacy,
                                 "quota_auto": rng.choice([1, 1, 1, 1, -1, 0])})
            imgs = rng.choice([0, 0, 50, 1000, sum(c["images"] for c in counters), rng.randint(1, 3000)])
            units = rng.choice([0.0, float(imgs), imgs * 0.5, imgs * 0.5 + 0.25, imgs * 0.75, imgs * 1.0001,
                                round(imgs * rng.uniform(0.3, 1.2), rng.randint(0, 3))])
            return {"counters": counters, "upstream_images": imgs, "upstream_units": float(units),
                    "hourly_block_hours": [rng.randint(470000, 470100) for _ in range(rng.choice([0, 0, 1, 2, 3, 5, 9]))],
                    "base_blocks": rng.choice([0, 0, 1, 7, 30]), "paced_blocks": rng.choice([0, 0, 2, 11, 250])}

        def make_yesterday_fetch(y):
            def fetch(sql, p):
                if "FROM counters c JOIN api_keys k ON k.id=c.key_id WHERE c.day=?" in sql and sql.startswith("SELECT c.key_id"):
                    return "counters", [(i, c["images"], c["daily_images"], c["legacy_free_images"], c["quota_auto"])
                                        for i, c in enumerate(y["counters"])]
                if "FROM upstream_token_counters WHERE day=?" in sql:
                    return "upstream", [(y["upstream_images"], y["upstream_units"])]
                if sql.startswith("SELECT DISTINCT CAST(u.ts/3600 AS INT)"):
                    return "hourly_hours", [(h,) for h in y["hourly_block_hours"]]
                if sql.startswith("SELECT COUNT(*) FROM usage_log u JOIN api_keys k") and sql.endswith("u.detail LIKE ?"):
                    return "base_blocks", [(y["base_blocks"],)]
                if sql.startswith("SELECT COUNT(*) FROM usage_log u JOIN api_keys k") and sql.endswith("u.reason=?"):
                    return "paced_blocks", [(y["paced_blocks"],)]
                raise AssertionError(sql)
            return fetch

        def yesterday(y, day, reset_hour):
            db = FakeDb({}, [], make_yesterday_fetch(y))
            out = run(quota_algo._yesterday(db, day, [], reset_hour))
            yesterday.queries = db._db.queries
            return out
        yesterday.__name__ = "_yesterday"
        for _ in range(700):
            y = gen_yesterday()
            day = rand_day()
            rh = rng.choice([7, 7, 0, 5, 23])
            out = w.call(yesterday, y, day, rh)
            w.lines[-1] = w.lines[-1][:-1] + ',"queries":' + json.dumps(yesterday.queries, ensure_ascii=False) + "}"

        # _v5_used_equiv
        details = ["", "1216x832/28step", "832x1216/14step/x", "/0step", "/1step", "/27step", "/29step", "/100step",
                   "x/１４step", "no steps here", "/12345678901234567890123step", "a/28stepb/14step", "/step",
                   "native /20step 2 images", "猫 /7step 头"]
        def v5_used_equiv(rows, day, reset_hour):
            db = FakeDb({}, [], lambda sql, p: ("v5_rows", [(r["detail"], r["images"]) for r in rows]))
            out = run(quota_algo._v5_used_equiv(db, day, reset_hour))
            v5_used_equiv.queries = db._db.queries
            return out
        v5_used_equiv.__name__ = "_v5_used_equiv"
        for _ in range(600):
            rows = [{"detail": rng.choice(details) if rng.random() < 0.7 else f"{rng.choice(['1024x1024', '832x1216'])}/{rng.randint(1, 40)}step",
                     "images": rng.choice([0, 1, 1, 1, 2, 4, 6])} for _ in range(rng.choice([0, 1, 2, 5, 20, 60]))]
            day = rand_day()
            rh = rng.choice([7, 0, 5, 23])
            w.call(v5_used_equiv, rows, day, rh)
            w.lines[-1] = w.lines[-1][:-1] + ',"queries":' + json.dumps(v5_used_equiv.queries, ensure_ascii=False) + "}"

        # quiet_recovery
        def quiet_recovery(now, snaps, counts, window):
            def fetch(sql, p):
                if sql.startswith("SELECT ts, v5_percent FROM upstream_snapshots"):
                    return "snaps", [(s["ts"], s["percent"]) for s in snaps]
                if sql.startswith("SELECT COUNT(*) FROM usage_log WHERE ts>? AND ts<=?"):
                    for i in range(len(snaps) - 1):
                        if snaps[i]["ts"] == p[0] and snaps[i + 1]["ts"] == p[1]:
                            return "v5_between", [(counts[i],)]
                    raise AssertionError(p)
                raise AssertionError(sql)
            db = FakeDb({}, [], fetch)
            out = run(quota_algo.quiet_recovery(db, now, window))
            quiet_recovery.queries = db._db.queries
            return out
        for _ in range(500):
            now = 1_790_000_000.0 + rng.uniform(0, 4e6)
            snaps, counts = gen_snaps(now)
            out = w.call(quiet_recovery, now, snaps, counts, 72 * 3600)
            w.lines[-1] = w.lines[-1][:-1] + ',"queries":' + json.dumps(quiet_recovery.queries, ensure_ascii=False) + "}"

        # _allowance
        def allowance(accounts):
            class Allow:
                async def snapshot(self, pool):
                    return {"accounts": accounts}
            state = SimpleNamespace(nai=SimpleNamespace(pool=[SimpleNamespace(usable=True)], allowance=Allow()))
            return list(run(quota_algo._allowance(state)))
        allowance.__name__ = "_allowance"
        for _ in range(400):
            w.call(allowance, gen_accounts(True))

        # _active_v5 的日期参数（计数本身是假 db 给的）
        def active_v5(now, days):
            db = FakeDb({}, [], lambda sql, p: ("active_v5", [(7,)]))
            run(quota_algo._active_v5(db, days, now))
            return db._db.queries[-1][1]
        active_v5.__name__ = "_active_v5_days"
        for _ in range(400):
            now = gen_now(rng.choice([7, 0]))
            w.call(active_v5, now, rng.choice([1, 3, 3, 3, 5, 7]))


# ---------------------------------------------------------------------------
# 场景生成
# ---------------------------------------------------------------------------
def gen_now(rh: int) -> float:
    day0 = datetime(2026, 9, 1, tzinfo=TZ) + timedelta(days=rng.randint(0, 150))
    kind = rng.random()
    if kind < 0.12:      # 服务日重置点附近
        t = day0.replace(hour=rh) + timedelta(seconds=rng.choice([-2, -1, 0, 1, 2, 30, -30, 600, -600]))
    elif kind < 0.2:     # 本地午夜附近（_active_v5 用自然日）
        t = day0 + timedelta(seconds=rng.choice([-2, -1, 0, 1, 2, 30]))
    else:
        t = day0 + timedelta(seconds=rng.randint(0, 86399))
    return t.timestamp() + rng.choice([0.0, 0.0, 0.5, round(rng.random(), 3), rng.random()])


def rand_pct() -> float | None:
    r = rng.random()
    if r < 0.08:
        return None
    if r < 0.5:
        return rng.choice([100.0, 99.5, 97.5, 95.0, 91.0, 90.0, 89.9, 86.5, 85.5, 85.0, 84.99, 70.0, 69.9,
                           55.5, 40.0, 39.9, 20.0, 19.99, 0.0, 5.5, 62.0, 93, 88, 41])
    return round(rng.uniform(0, 100), rng.randint(0, 2))


def gen_accounts(nai_present: bool) -> list[dict]:
    if not nai_present:
        return []
    out = []
    for _ in range(rng.choice([0, 1, 1, 1, 2, 2, 3])):
        rate = rng.choice([None, 0, 0.0, 5.0, 11.0, 7.3, 14.9, 15.1, 0.5, 20.0, 4.4, 8.8]) if rng.random() < 0.7 else round(rng.uniform(0.5, 16), 1)
        out.append({"percent": rand_pct(), "recharge_per_day": rate})
    return out


def gen_snaps(now: float) -> tuple[list[dict], list[int]]:
    r = rng.random()
    if r < 0.3:
        return [], []
    n = rng.choice([1, 2, 3, 5, 6, 9, 12, 20])
    integer_ts = rng.random() < 0.5
    t = now - rng.uniform(0, 3600)
    ts = []
    for _ in range(n):
        ts.append(float(int(t)) if integer_ts else t)
        gap = rng.choice([3600, 3600, 3600, 1800, 7200, 10800, 10801, 14400, 3600.5]) + (0 if integer_ts else rng.uniform(-30, 30))
        t -= gap
    ts.reverse()
    pct = rng.uniform(30, 95)
    snaps = []
    for x in ts:
        pct += rng.choice([0.2, 0.2, 0.2, 0.0, -0.5, 1.0, -1.2])
        snaps.append({"ts": x, "percent": round(pct, rng.choice([1, 1, 2]))})
    counts = [rng.choice([0, 0, 0, 0, 1, 3, 12]) for _ in range(max(0, n - 1))]
    return snaps, counts


def make_review(day: str, i: int) -> dict:
    a = rng.choice([150, 100, 175, 125])
    n = rng.choice([a, a, a + 25, a - 10 if a > 20 else a])
    r = {"day": day, "used": rng.randint(0, 900), "ceiling_hits": rng.randint(0, 20), "hourly_blocks": rng.randint(0, 5),
         "base_blocks": rng.randint(0, 3), "paced_blocks": rng.randint(0, 40),
         "coverage_hours": round(rng.choice([24.0, 24.0, 18.3, 20.0, 12.5, 0.0, 7.7]), 1),
         "cap": 1000, "budget": rng.choice([870, 1000, 740]), "from": [a, a], "to": [n, n],
         "reasons": [f"每人每天 {n} 张。还有余量"]}
    if rng.random() < 0.7:
        r["v5_pct"] = round(rng.uniform(20, 99), 1)
        r["v5_rate"] = rng.choice([None, 5.0, 11.0, round(rng.uniform(2, 14), 1)])
        if rng.random() < 0.6:
            r["v5_used_pct"] = round(rng.uniform(0, 12), 2)
            r["v5_rate_measured"] = rng.choice([round(rng.uniform(-3, 20), 2), 4.5, 16.2, 0.4, 15.0, 1.0])
    return r


def gen_history_raw(yday: str, rh: int) -> str | None:
    r = rng.random()
    if r < 0.1:
        return None
    if r < 0.15:
        return ""
    if r < 0.17:
        return "garbage"
    if r < 0.18:
        return "{}"
    if r < 0.19:
        return '"str"'
    if r < 0.2:
        return '{"a": 1}'
    n = rng.choice([0, 0, 0, 1, 1, 1, 2, 2, 3, 3, 4, 5, 6, 8] + ([29, 30, 31] if rng.random() < 0.12 else []))
    base = datetime.strptime(yday, "%Y-%m-%d")
    items = [make_review((base - timedelta(days=n - k)).strftime("%Y-%m-%d"), k) for k in range(n)]
    if items and rng.random() < 0.5:
        items[-1]["day"] = yday if rng.random() < 0.7 else items[-1]["day"]
    if items and rng.random() < 0.03:
        items[-1] = 5
    if items and rng.random() < 0.05:
        items[0] = {"day": "x"}
    return json.dumps(items, ensure_ascii=False)


def gen_v5_plan_value(today: str) -> dict:
    pct = rand_pct()
    rate, info = quota_algo.choose_rate(rng.choice([None, 5.0, 11.0, 9.2]), rng.choice([None, 4.5, 6.0]))
    set_params({})
    plan = quota_algo.v5_plan(pct, rate, rng.choice([3, 10, 14, 30]), lo=3, hi=30)
    plan["each"] = rng.choice([3, 5, 9, 10, 12, 18, 25, 30])
    plan["global"] = rng.choice([100, 130, 260, 90])
    plan["rate_info"] = info
    return plan


def gen_settings(rng_: random.Random, sc: dict) -> dict:
    s: dict[str, str] = {}
    a_ok = True
    today, yday, rh = sc["today"], sc["yday"], sc["rh"]
    for name, default in quota_algo.DEFAULTS.items():
        if rng_.random() < 0.15:
            if name == "quota_auto_enabled":
                s[name] = rng_.choice(["1", "1", "1.0", " 1 "])
                continue
            r = rng_.random()
            if r < 0.04:
                s[name] = rng_.choice(["abc", "", "nan", "1_0"])
            elif r < 0.05:
                s[name] = "inf"
            else:
                val = {"quota_target_avg": rng_.randint(60, 220), "quota_base": rng_.randint(40, 150),
                       "quota_ceiling_max": rng_.choice([126, 200, 300, 400]), "quota_base_min": rng_.randint(20, 80),
                       "quota_base_min_fair": rng_.choice([5, 10, 20, 40]), "quota_step": rng_.choice([5, 10, 25, 50]),
                       "quota_base_step": rng_.choice([5, 10, 20]), "quota_v5_min": rng_.choice([1, 3, 5, 8]),
                       "quota_v5_max": rng_.choice([10, 20, 30, 40])}[name]
                s[name] = rng_.choice([str(val), str(val), f"{val}.0", f"{val}.9", f" {val} ", f"{val}e0"])
    r = rng_.random()
    if r < 0.04:
        s["quota_auto_enabled"] = rng_.choice(["0", "0.5", " 0 "])
    if rng_.random() < 0.85:
        v = rng_.choice([150, 150, 150, 100, 120, 175, 200, 300, 50, 10, 25, 0, 126, 127, 125])
        s["quota_ceiling"] = rng_.choice([str(v), str(v), str(v), f"{v}.0", f"{v}.9", f" {v} "])
        if rng_.random() < 0.012:
            s["quota_ceiling"] = rng_.choice(["abc", "", "inf", "nan"])
    if rng_.random() < 0.85:
        s["quota_base_now"] = str(rng_.choice([150, 150, 100, 120, 80, 0, 175]))
        if rng_.random() < 0.02:
            s["quota_base_now"] = rng_.choice(["xyz", "-5", "100001"])
    r = rng_.random()
    if r < 0.4:
        s[quota_algo.DAY_KEY] = today
    elif r < 0.8:
        s[quota_algo.DAY_KEY] = rng_.choice([yday, "2026-01-01"])
    h = gen_history_raw(yday, rh)
    if h is not None:
        s[quota_algo.HISTORY_KEY] = h
    r = rng_.random()
    if r < 0.35:
        pass
    elif r < 0.37:
        s[quota_algo.V5_DAY_KEY] = ""
    elif r < 0.38:
        s[quota_algo.V5_DAY_KEY] = "{oops"
    elif r < 0.39:
        s[quota_algo.V5_DAY_KEY] = "[]"
    elif r < 0.72:
        s[quota_algo.V5_DAY_KEY] = json.dumps({"day": today, "plan": gen_v5_plan_value(today)}, ensure_ascii=False)
    elif r < 0.87:
        s[quota_algo.V5_DAY_KEY] = json.dumps({"day": yday, "plan": gen_v5_plan_value(yday)}, ensure_ascii=False)
    elif r < 0.9:
        s[quota_algo.V5_DAY_KEY] = json.dumps({"day": today, "plan": {}})
    elif r < 0.92:
        s[quota_algo.V5_DAY_KEY] = json.dumps({"day": today})
    elif r < 0.95:
        p = gen_v5_plan_value(today)
        del p["each"]
        s[quota_algo.V5_DAY_KEY] = json.dumps({"day": today, "plan": p})
    else:
        s[quota_algo.V5_DAY_KEY] = json.dumps({"day": today, "plan": [1, 2]})
    r = rng_.random()
    if r < 0.5:
        s["global_daily_v5"] = rng_.choice(["130", "150", "0", "260", "99999999", "-4", "12.7", " 140 "])
    elif r < 0.55:
        s["global_daily_v5"] = rng_.choice(["", "abc", "nan"])
    r = rng_.random()
    if r < 0.4:
        s["member_daily_anlas"] = rng_.choice(["0", "50", "25", "-3", "100000", "999999", "7.9", "", "abc"])
    return s


def compute_budget(inp: dict) -> int:
    pool_len = inp["pool_len"]
    accounts = max(1, pool_len or 0) if pool_len is not None else 1
    cap = (inp["guard"]["account_daily_cap"] if inp["guard"] else 1000) * accounts
    v5_global = int(site_flags.parse(site_flags.GLOBAL_DAILY_V5, inp["settings"].get("global_daily_v5"),
                                     fallback=inp["global_daily_v5_default"]) or 0)
    return max(0, cap - v5_global)


def gen_daily(inp: dict, a_guess: int) -> dict:
    y = {}
    n = rng.choice([0, 1, 3, 6, 10, 20])
    counters = []
    for _ in range(n):
        images = rng.choice([0, 1, 20, 100, 150, rng.randint(0, 400)])
        daily = rng.choice([0, 100, 150, 150, 175, rng.randint(1, 300)])
        legacy = rng.choice([0, images, daily, max(0, daily - 1), daily + 1, rng.randint(0, images or 1)])
        counters.append({"images": images, "daily_images": daily, "legacy_free_images": legacy,
                         "quota_auto": rng.choice([1, 1, 1, 1, -1, 0])})
    imgs = rng.choice([0, 0, 50, 1000, sum(c["images"] for c in counters), rng.randint(1, 3000)])
    units = rng.choice([0.0, float(imgs), imgs * 0.5, imgs * 0.5 + 0.25, imgs * 0.75, imgs * 1.0001,
                        round(imgs * rng.uniform(0.3, 1.2), rng.randint(0, 3))])
    y = {"counters": counters, "upstream_images": imgs, "upstream_units": float(units),
         "hourly_block_hours": [rng.randint(470000, 470100) for _ in range(rng.choice([0, 0, 1, 2, 3, 5, 9]))],
         "base_blocks": rng.choice([0, 0, 1, 7, 30]), "paced_blocks": rng.choice([0, 0, 2, 11, 250])}
    # 覆盖小时数：偶尔恰好卡在 20 小时门槛上
    yday, rh = inp["_yday"], inp["_rh"]
    start = day_start(yday, rh)
    kind = rng.random()
    if kind < 0.05:
        first = None
    elif kind < 0.5:
        first = start - rng.uniform(0, 3 * 86400)
    elif kind < 0.6:
        first = start + 86400 - rng.choice([20 * 3600, 20 * 3600 - 1, 20 * 3600 + 1, 19.99 * 3600, 20.01 * 3600])
    elif kind < 0.65:
        first = start + 86400 + 10
    else:
        first = start + rng.uniform(0, 86400)
    if first is not None and rng.random() < 0.3:
        first = float(int(first))
    # 成员用量：围绕 95% 门槛
    budget = compute_budget(inp)
    usage: list[int] = []
    kind = rng.random()
    if kind < 0.08:
        usage = []
    else:
        f = rng.choice([0.3, 0.6, 0.9, 0.94, 0.95, 0.96, 1.0, 1.2, 1.5, 0.0])
        total = int(budget * f)
        if kind < 0.3:
            thr = budget * 0.95
            total = rng.choice([math.ceil(thr), math.ceil(thr) - 1, int(thr), int(thr) + 1])
        k = rng.randint(1, 20)
        if total >= k:
            cuts = sorted(rng.sample(range(1, total), k - 1)) if k > 1 else []
            parts = [b - a for a, b in zip([0] + cuts, cuts + [total])]
            usage = parts
        elif total > 0:
            usage = [total]
        else:
            usage = [rng.randint(1, 20) for _ in range(rng.choice([0, 1, 3]))]
        if usage and rng.random() < 0.4:
            usage[rng.randrange(len(usage))] = max(1, a_guess + rng.choice([-1, 0, 1, 40]))
        rng.shuffle(usage)
    details = ["1216x832/28step", "832x1216/14step", "1024x1024/20step", "", "1216x832"]
    rows = [{"detail": rng.choice(details), "images": rng.choice([0, 1, 1, 2, 4])} for _ in range(rng.choice([0, 0, 3, 6, 15]))]
    return {"yesterday": y, "coverage_first_ts": first, "member_usage": usage, "v5_rows": rows}


def gen_run_inputs() -> dict:
    rh = rng.choice([7] * 8 + [0, 5, 23, 12])
    now = gen_now(rh)
    day_fn = make_day(rh)
    today, yday = day_fn(now), day_fn(now - 86400)
    sc = {"today": today, "yday": yday, "rh": rh}
    pool_len = rng.choice([None, 0, 1, 1, 1, 2, 3])
    guard = None
    if rng.random() < 0.9:
        guard = {"account_daily_cap": rng.choice([1000, 1000, 1000, 500, 2000, 300, 0, 1500]), "base_daily_images": None}
    inp = {"now": now, "settings": gen_settings(rng, sc), "guard": guard, "pool_len": pool_len}
    env_gdv5 = rng.choice([150, 150, 0, 130, 260, 200000, -5])
    inp["_env_gdv5"] = env_gdv5
    inp["global_daily_v5_default"] = site_flags.default_of(site_flags.GLOBAL_DAILY_V5, SimpleNamespace(global_daily_v5=env_gdv5))
    inp["members"] = rng.choice([0, 1, 5, 14, 23, 40, rng.randint(0, 90)])
    inp["_yday"], inp["_rh"], inp["_today"] = yday, rh, today
    try:
        a_guess = int(float(inp["settings"].get("quota_ceiling", "150")))
    except ValueError:
        a_guess = 150
    except OverflowError:
        a_guess = 150
    inp["daily"] = gen_daily(inp, a_guess)
    inp["accounts"] = gen_accounts(pool_len is not None)
    snaps, counts = gen_snaps(now)
    inp["quiet"] = {"snaps": snaps, "counts": counts}
    inp["active_v5"] = rng.choice([0, 1, 3, 9, 10, 14, 25, rng.randint(0, 60)])
    inp["managed_keys"] = []
    inp["_private"] = rand_overrides() if rng.random() < 0.3 else {}
    if guard is not None:
        r = rng.random()
        base = a_guess if r < 0.5 else rng.choice([None, 100, 150, 0, 99999, 100001, -1])
        guard["base_daily_images"] = base
    return inp


def gen_managed(rng_: random.Random, a: int, each: int, mode: str) -> list[dict]:
    n = rng_.choice([0, 1, 3, 6, 10] + ([30] if rng_.random() < 0.1 else []))
    keys = []
    for _ in range(n):
        if mode == "synced":
            keys.append({"daily_images": a, "daily_v5": each, "image_model_scope": "all"})
        else:
            keys.append({"daily_images": rng_.choice([a, a, 150, 100, a + 1]), "daily_v5": rng_.choice([each, each, 0, 5, each + 1]),
                         "image_model_scope": rng_.choice(["all", "all", "legacy"])})
    return keys


def execute_case(inp: dict) -> tuple:
    """用假 db / 假 state 跑一遍真正的 quota_algo.run。返回 (out, err, effects, queries, 解析后的参数)。"""
    effects: list[dict] = []
    set_params(inp["_private"])
    managed = inp["managed_keys"]
    mem = sqlite3.connect(":memory:")
    mem.execute("CREATE TABLE api_keys (id INTEGER PRIMARY KEY, quota_auto INTEGER, is_admin INTEGER, is_test INTEGER, "
                "daily_images INTEGER NOT NULL, daily_v5 INTEGER NOT NULL, image_model_scope TEXT NOT NULL)")
    for k in managed:
        mem.execute("INSERT INTO api_keys(quota_auto,is_admin,is_test,daily_images,daily_v5,image_model_scope) VALUES (1,0,0,?,?,?)",
                    (k["daily_images"], k["daily_v5"], k["image_model_scope"]))
    # 干扰行：手动 / 站长 / 测试 Key 不应被数到
    for q, ad, te in ((-1, 0, 0), (1, 1, 0), (1, 0, 1), (0, 0, 0)):
        mem.execute("INSERT INTO api_keys(quota_auto,is_admin,is_test,daily_images,daily_v5,image_model_scope) VALUES (?,?,?,?,?,?)",
                    (q, ad, te, 999, 999, "legacy"))
    d = inp["daily"]
    snaps = inp["quiet"]["snaps"]
    counts = inp["quiet"]["counts"]

    def fetch(sql, p):
        if sql == "SELECT id FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0 AND quota_auto=1":
            return "members", [(i,) for i in range(inp["members"])]
        if sql.startswith("SELECT c.key_id, c.images, k.daily_images, c.legacy_free_images, k.quota_auto FROM counters c"):
            return "counters", [(i, c["images"], c["daily_images"], c["legacy_free_images"], c["quota_auto"])
                                for i, c in enumerate(d["yesterday"]["counters"])]
        if "FROM upstream_token_counters WHERE day=?" in sql:
            return "upstream", [(d["yesterday"]["upstream_images"], d["yesterday"]["upstream_units"])]
        if sql.startswith("SELECT DISTINCT CAST(u.ts/3600 AS INT)"):
            return "hourly_hours", [(h,) for h in d["yesterday"]["hourly_block_hours"]]
        if sql.startswith("SELECT COUNT(*) FROM usage_log u JOIN api_keys k") and sql.endswith("u.detail LIKE ?"):
            return "base_blocks", [(d["yesterday"]["base_blocks"],)]
        if sql.startswith("SELECT COUNT(*) FROM usage_log u JOIN api_keys k") and sql.endswith("u.reason=?"):
            return "paced_blocks", [(d["yesterday"]["paced_blocks"],)]
        if sql == "SELECT MIN(ts) FROM usage_log":
            return "min_ts", [(d["coverage_first_ts"],)]
        if sql.startswith("SELECT c.legacy_free_images FROM counters c"):
            return "member_usage", [(u,) for u in d["member_usage"]]
        if sql.startswith("SELECT detail, images FROM usage_log WHERE ts>=? AND ts<?"):
            return "v5_rows", [(r["detail"], r["images"]) for r in d["v5_rows"]]
        if sql.startswith("SELECT ts, v5_percent FROM upstream_snapshots"):
            return "snaps", [(s["ts"], s["percent"]) for s in snaps]
        if sql.startswith("SELECT COUNT(*) FROM usage_log WHERE ts>? AND ts<=?"):
            for i in range(len(snaps) - 1):
                if snaps[i]["ts"] == p[0] and snaps[i + 1]["ts"] == p[1]:
                    return "v5_between", [(counts[i],)]
            raise AssertionError(p)
        if sql.startswith("SELECT COUNT(DISTINCT c.key_id) FROM counters c"):
            return "active_v5", [(inp["active_v5"],)]
        if sql.startswith("SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 AND (daily_images<>?"):
            return "count_managed", mem.execute(sql, p).fetchall()
        raise AssertionError(sql)

    def execute(sql, p):
        assert sql.startswith("UPDATE api_keys SET daily_images=?, daily_v5=?, image_model_scope='all' WHERE quota_auto=1 AND is_admin=0 AND is_test=0"), sql
        effects.append({"op": "update_keys", "daily_images": p[0], "daily_v5": p[1]})
        return "update_keys"

    db = FakeDb(inp["settings"], effects, fetch, execute)
    guard = None
    if inp["guard"] is not None:
        g = Guard(db=None)
        g.values["account_daily_cap"] = inp["guard"]["account_daily_cap"]
        if inp["guard"]["base_daily_images"] is None:
            g.values.pop("base_daily_images", None)
        else:
            g.values["base_daily_images"] = inp["guard"]["base_daily_images"]

        async def save(body, _g=g):
            res = await Guard.save(_g, body)          # 先校验（越界抛 ValueError），成功才记录
            effects.append({"op": "guard_save_base", "base_daily_images": body["base_daily_images"]})
            return res
        g.save = save
        guard = g
    nai = None
    if inp["pool_len"] is not None:
        accounts = inp["accounts"]

        class Allow:
            async def snapshot(self, pool):
                return {"accounts": [dict(a) for a in accounts]}
        nai = SimpleNamespace(pool=[SimpleNamespace(usable=True) for _ in range(inp["pool_len"])], allowance=Allow())
    state = SimpleNamespace(db=db, guard=guard, nai=nai, day=make_day(inp["_rh"]),
                            settings=SimpleNamespace(day_reset_hour=inp["_rh"], global_daily_v5=inp["_env_gdv5"]))
    quota_algo.log_action = fake_log(effects)
    try:
        out, err = run(quota_algo.run(state, now=inp["now"])), None
    except Exception as e:  # noqa: BLE001 — 异常类型本身就是要对拍的行为
        out, err = None, type(e).__name__
    params = resolved_params()
    return out, err, effects, db._db.queries, params


def rust_inputs(inp: dict, params: dict) -> dict:
    keep = ("now", "settings", "guard", "pool_len", "global_daily_v5_default", "members", "daily", "accounts",
            "quiet", "active_v5", "managed_keys")
    out = {k: inp[k] for k in keep}
    out["params"] = params
    return out


def gen_run(count: int) -> None:
    stats = {"disabled": 0, "adjusted": 0, "err": 0, "changed": 0}
    with Writer("quota/run") as w:
        for i in range(count):
            inp = gen_run_inputs()
            out, err, effects, queries, params = execute_case(inp)
            # 让一部分场景的 Key 已经同步到算法结果（changed == 0 分支），另一部分随机失配
            if err is None and out.get("enabled") and rng.random() < 0.6:
                a, each = out["ceiling"], out["v5"]["each"]
                inp["managed_keys"] = gen_managed(rng, a, each, "synced" if rng.random() < 0.55 else "mixed")
                out, err, effects, queries, params = execute_case(inp)
            elif rng.random() < 0.5:
                inp["managed_keys"] = gen_managed(rng, 150, 9, "mixed")
                out, err, effects, queries, params = execute_case(inp)
            stats["err"] += err is not None
            if err is None and not out.get("enabled"):
                stats["disabled"] += 1
            if err is None and out.get("review"):
                stats["adjusted"] += 1
            if err is None and out.get("changed_keys"):
                stats["changed"] += 1
            w.record(fn="run", reset_hour=inp["_rh"], inputs=rust_inputs(inp, params), out=out, err=err, effects=effects,
                     queries=queries)
    print("run 场景统计:", stats)


if __name__ == "__main__":
    gen_pure()
    gen_sqlmath()
    gen_run(700)
