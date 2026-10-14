"""自动驾驶（app/autopilot.py）的 golden 夹具。

输出（crates/gate-core/tests/golden/autopilot/）：
- pure.jsonl   slots_rule / breaker_rule / key_guard_rule（阈值两侧都有用例）
- consts.jsonl 常量核对（单 Key 限流统计排除的原因码清单、SLOTS_*）
- run.jsonl    整条 run()：假 db / 假 state / 假注册服务回答查询并记录每一次写入与动作；
               Rust 的 decide() 必须产出逐字节相同的写入序列与返回值

运行（仓库根目录）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/autopilot.py
"""
from __future__ import annotations

import json
import math
import random
from datetime import datetime, timedelta
from types import SimpleNamespace

from _fakes import FakeDb, TZ, fake_log, make_day, run
from _golden import Writer

from app import autopilot, ops, quota_algo, reasons, site_flags

rng = random.Random(20261014 + 7)


# ---------------------------------------------------------------------------
# 纯规则
# ---------------------------------------------------------------------------
def gen_pure() -> None:
    with Writer("autopilot/pure") as w:
        # slots_rule：每个门槛两侧
        caps = [0, 1, 30, 50, 94, 95, 96, 97, 98, 99, 100, 101, 150]
        utils = [0.0, 0.1, 0.125, 0.3, 0.5, 0.595, 0.599999, 0.6, 0.600001, 0.605, 0.625, 0.7, 1.0, 1.5, 0.005, 0.015, 0.025]
        for _ in range(2500):
            cap = rng.choice(caps)
            active = rng.choice([cap, cap - 1, cap - 2, cap - 3, cap + 1, 0, rng.randint(0, 150)])
            util = rng.choice(utils) if rng.random() < 0.7 else rng.uniform(0, 1.5)
            blocked = rng.choice([0, 1, 2, 3, 4])
            queue = rng.choice([0, 1, 2, 3, 4])
            v5 = rng.choice([None, None, 49, 49.4, 49.5, 50, 50.0, 50.5, 12, 100, 0, 49.99, 0.5, 1.5, 2.5])
            if rng.random() < 0.5:
                w.call(autopilot.slots_rule, cap, active, util, blocked)
            else:
                w.call(autopilot.slots_rule, cap, active, util, blocked, queue, v5)
        for fails in range(0, 14):
            for total in [0, 1, 3, 5, 10, 16, 17, 18, 20, 33, 40]:
                w.call(autopilot.breaker_rule, fails, total)
        for _ in range(500):
            w.call(autopilot.breaker_rule, rng.randint(0, 60), rng.randint(0, 200))
        for alt in range(0, 4):
            for cl in range(0, 3):
                for ad in range(0, 3):
                    for rej in (0, 1, 59, 60, 61, 500):
                        w.call(autopilot.key_guard_rule, alt, cl, ad, rej)

    with Writer("autopilot/consts") as w:
        w.record(fn="consts", skip_reasons=[reasons.KEY_PAUSED, *reasons.SITE_LEVEL],
                 slots_max=autopilot.SLOTS_MAX, slots_step=autopilot.SLOTS_STEP,
                 slots_cooldown=autopilot.SLOTS_COOLDOWN, queue_busy=autopilot.QUEUE_BUSY_REJECTS,
                 slots_v5_min=autopilot.SLOTS_V5_MIN, state_key=autopilot.STATE_KEY,
                 history_key=autopilot.HISTORY_KEY)


# ---------------------------------------------------------------------------
# 场景
# ---------------------------------------------------------------------------
def gen_now(rh: int = 7) -> float:
    day0 = datetime(2026, 9, 1, tzinfo=TZ) + timedelta(days=rng.randint(0, 150))
    kind = rng.random()
    if kind < 0.1:
        t = day0.replace(hour=rh) + timedelta(seconds=rng.choice([-1, 0, 1]))
    elif kind < 0.2:      # 整点附近（每小时快照键）
        t = day0 + timedelta(hours=rng.randint(0, 23), seconds=rng.choice([-1, 0, 1, 3599, 3600]))
    else:
        t = day0 + timedelta(seconds=rng.randint(0, 86399))
    return t.timestamp() + rng.choice([0.0, 0.0, 0.5, round(rng.random(), 3), rng.random()])


def gen_last(now: float, clean: bool, perfect: bool = False):
    """上一轮额度复盘（quota_algo_history 的最后一条）；围绕各门槛取值。clean=False 时只加一个故障。"""
    day_fn = make_day(7)
    yday = day_fn(now - 86400)
    budget = rng.choice([870, 1000, 740, 500, 333, 1000])
    b = budget
    last: dict = {
        "day": yday, "budget": budget, "cap": rng.choice([1000, 1000, 2000]),
        "used": rng.choice([0, 1, int(b * 0.6) - 1, int(b * 0.6), math.ceil(b * 0.6), int(b * 0.125), int(b * 0.625),
                            int(b * 0.5), int(b * 0.2), int(b * 0.995), b, int(b * 1.4)]),
        "hourly_blocks": rng.choice([0, 0, 1, 2, 3, 4]),
        "coverage_hours": rng.choice([24.0, 24.0, 24.0, 24.0, 20.0, 19.9, 19.99, 20.01, 0.0, 12.5, 21, 18.3, 7.7]),
    }
    if perfect:       # 几乎一定会触发 +5 的上一轮复盘（用量 < 60%、没被每小时上限拦、数据完整）
        last["used"] = rng.choice([0, 1, int(b * 0.6) - 1, int(b * 0.125), int(b * 0.5), int(b * 0.2)])
        last["hourly_blocks"] = rng.choice([0, 1, 2])
        last["coverage_hours"] = rng.choice([24.0, 20.0, 20.01, 22.5])
    if clean:
        return last
    fault = rng.choice(["no_day", "empty_day", "bad_day", "none_day", "int_day", "x_day", "no_budget", "zero_budget",
                        "str_budget", "float_budget", "no_cap", "zero_cap", "none_used", "no_used", "float_used",
                        "float_blocks", "str_blocks", "none_blocks", "bad_blocks", "no_blocks", "none_cov", "str_cov",
                        "no_cov", "bool_cov", "int_cov", "empty"])
    if fault == "no_day":
        del last["day"]
    elif fault == "empty_day":
        last["day"] = ""
    elif fault == "bad_day":
        last["day"] = "2026-13-45"
    elif fault == "none_day":
        last["day"] = None
    elif fault == "int_day":
        last["day"] = 5
    elif fault == "x_day":
        last["day"] = "x-day"
    elif fault == "no_budget":
        del last["budget"]
    elif fault == "zero_budget":
        last["budget"] = 0
    elif fault == "str_budget":
        last["budget"] = "abc"
    elif fault == "float_budget":
        last["budget"] = 1.0
    elif fault == "no_cap":
        del last["budget"]
        del last["cap"]
    elif fault == "zero_cap":
        del last["budget"]
        last["cap"] = rng.choice([0, None])
    elif fault == "none_used":
        last["used"] = None
    elif fault == "no_used":
        del last["used"]
    elif fault == "float_used":
        last["used"] = 1234.5
    elif fault == "float_blocks":
        last["hourly_blocks"] = rng.choice([2.9, 3.0])
    elif fault == "str_blocks":
        last["hourly_blocks"] = "3"
    elif fault == "none_blocks":
        last["hourly_blocks"] = None
    elif fault == "bad_blocks":
        last["hourly_blocks"] = "x"
    elif fault == "no_blocks":
        del last["hourly_blocks"]
    elif fault == "none_cov":
        last["coverage_hours"] = None
    elif fault == "str_cov":
        last["coverage_hours"] = "abc"
    elif fault == "no_cov":
        del last["coverage_hours"]
    elif fault == "bool_cov":
        last["coverage_hours"] = True
    elif fault == "int_cov":
        last["coverage_hours"] = rng.choice([19, 20, 21])
    elif fault == "empty":
        last = {}
    return last


def gen_hist_raw(last) -> str | None:
    if last is None:
        return rng.choice([None, "", "[]", "{}", "0", "null", "false"])
    r = rng.random()
    if r < 0.02:
        return "garbage"
    if r < 0.03:
        return '{"a": 1}'
    if r < 0.04:
        return '"str"'
    if r < 0.05:
        return "5"
    if r < 0.06:
        return "true"
    items = [{"day": f"2026-09-{i + 1:02d}", "used": 1, "budget": 1000} for i in range(rng.choice([0, 1, 3]))]
    if rng.random() < 0.02:
        last = 7
    return json.dumps(items + [last], ensure_ascii=False)


def gen_inputs() -> dict:
    now = gen_now()
    s: dict[str, str] = {}
    for rule in ("slots", "breaker", "key_guard"):
        r = rng.random()
        if r < 0.45:
            s[f"autopilot_{rule}"] = "enforce"
        elif r < 0.7:
            s[f"autopilot_{rule}"] = "observe"
        elif r < 0.78:
            s[f"autopilot_{rule}"] = "off"
        elif r < 0.84:
            s[f"autopilot_{rule}"] = rng.choice(["bogus", "", " enforce", "ENFORCE"])
    targeted = rng.random() < 0.4          # 名额 +5 的条件很苛刻，专门造一批「几乎满足」的场景
    perfect = targeted and rng.random() < 0.65
    last = gen_last(now, clean=targeted or rng.random() < 0.7, perfect=perfect)
    if rng.random() < 0.08 and not perfect:
        last = None
    h = gen_hist_raw(last)
    if h is not None:
        s[quota_algo.HISTORY_KEY] = h
    r = rng.random()
    if r < 0.3:
        pass
    elif r < 0.32:
        s[quota_algo.STATE_KEY] = "bad json"
    elif r < 0.33:
        s[quota_algo.STATE_KEY] = "[]"
    elif r < 0.34:
        s[quota_algo.STATE_KEY] = json.dumps({"v5": [1]})
    elif r < 0.35:
        s[quota_algo.STATE_KEY] = json.dumps({"v5": {"percent": "50"}})
    else:
        pct = rng.choice([None, 49, 49.4, 49.5, 50, 50.0, 50.5, 12, 100, 0, 97.3, 49.99, True])
        v5 = rng.choice([{"percent": pct}, {"percent": pct, "each": 9}, {}, None, {"each": 3}])
        s[quota_algo.STATE_KEY] = json.dumps({"enabled": True, "v5": v5})
    r = rng.random()
    if r < 0.5:
        pass
    elif r < 0.55:
        s["autopilot_slots_at"] = rng.choice(["0", "", "abc", "nan", "inf"])
    else:
        s["autopilot_slots_at"] = repr(now - rng.choice([86400, 86400.0001, 86399.9999, 3600, 90000, 0, 1e6]))
    r = rng.random()
    if r < 0.5:
        pass
    elif r < 0.52:
        s["autopilot_breaker_until"] = rng.choice(["", "x", "0"])
    else:
        s["autopilot_breaker_until"] = repr(now + rng.choice([-1000, -100, 100, 300, -900, 0]))
    r = rng.random()
    if r < 0.5:
        pass
    elif r < 0.6:
        s["global_daily_v5"] = rng.choice(["130", "150", "0", "99999999", "abc", ""])
    hour = datetime.fromtimestamp(now, TZ).strftime("%Y-%m-%d %H:00")
    r = rng.random()
    if r < 0.4:
        pass
    elif r < 0.55:
        s[autopilot.HISTORY_KEY] = json.dumps([{"hour": hour, "slots": 50, "breaker": False, "key_guard": 0}])   # 本小时已有
    elif r < 0.7:
        s[autopilot.HISTORY_KEY] = json.dumps([{"hour": "2026-01-01 00:00", "slots": 50, "breaker": False, "key_guard": 0}])
    elif r < 0.72:
        n = rng.choice([167, 168, 169])
        s[autopilot.HISTORY_KEY] = json.dumps([{"hour": f"h{i}", "slots": 50, "breaker": False, "key_guard": 0} for i in range(n)])
    elif r < 0.78:
        s[autopilot.HISTORY_KEY] = rng.choice(["", "{}", "bad", '{"a": 1}', '"s"', "5", "null", "[5]", "[{}]", '[{"hour": null}]'])

    registrar = None
    if rng.random() > 0.12:
        cap = rng.choice([0, 0, 30, 50, 95, 96, 97, 98, 99, 100, 101, 150])
        active = rng.choice([cap, cap - 1, cap - 2, cap - 3, cap + 1, 0, rng.randint(0, 150)])
        registrar = {"max_users": cap, "active": active}

    if perfect:
        s[quota_algo.STATE_KEY] = json.dumps({"enabled": True, "v5": rng.choice([{"percent": 97}, {"percent": 50}, {}, None])})
    if targeted:
        s["autopilot_slots"] = "enforce"
        cap = rng.choice([30, 50, 94, 95, 96, 97, 98, 99])
        registrar = {"max_users": cap, "active": cap - rng.choice([0, 1, 2, 2, 3])}
        if rng.random() < 0.8:
            s.pop("autopilot_slots_at", None)
        if rng.random() < 0.7:
            s.pop(quota_algo.STATE_KEY, None)
        if rng.random() < 0.9:
            s.pop(autopilot.HISTORY_KEY, None)

    nkeys = rng.choice([0, 1, 2, 3, 5, 8])
    kids = rng.sample(range(1, 300), nkeys)
    keys = [[k, f"成员{k:03d}"] for k in kids]
    events = []
    rejects = []
    for k in kids:
        for kind, cnt in (("alternate", rng.choice([0, 0, 1, 2, 3])), ("clients", rng.choice([0, 0, 0, 1, 2])),
                          ("allday", rng.choice([0, 0, 0, 1, 2]))):
            for _ in range(cnt):
                t = now - rng.choice([0, 10, 3600, 80000, 86399.9, 86400.0, 86400.001, 90000, 200000])
                events.append([t, k, kind])
        n = rng.choice([0, 0, 3, 59, 60, 61, 200])
        if n:
            rejects.append([k, n])
    for _ in range(rng.choice([0, 0, 2])):     # 已删除 / 测试 Key 的杂项
        events.append([now - 10, rng.randint(300, 400), rng.choice(["alternate", "clients", "allday"])])
        rejects.append([rng.randint(300, 400), 99])
    rng.shuffle(events)
    rng.shuffle(rejects)
    if rng.random() < 0.04:       # 重复 id（数据库主键不会有，旧代码按 dict 保留第一次出现的位置）
        if keys:
            keys.append([keys[0][0], "重复"])

    r = rng.random()
    fails, total = rng.choice([(None, 0), (0, 0), (5, 16), (5, 17), (4, 10), (5, 5), (3, 10), (10, 33), (6, 20), (9, 30),
                               (1, 3), (30, 40), (5, 40), (4, 4)] + [(rng.randint(0, 20), rng.randint(0, 60))])
    inp = {"now": now, "settings": s, "registrar": registrar,
           "global_daily_v5_default": rng.choice([150, 0, 130]),
           "queue_hours": rng.choice([0, 1, 2]) if perfect else rng.choice([0, 0, 1, 2, 2, 3, 4, 10]),
           "breaker": {"fails": fails, "total": total},
           "guard": None if rng.random() < 0.15 else {"breaker_until": rng.choice([0.0, now - 10, now, now + 10, now + 299])},
           "share": None, "events": events, "rejects": rejects, "keys": keys}
    if rng.random() > 0.15:
        paused = {}
        for k in kids:
            if rng.random() < 0.3:
                paused[str(k)] = rng.choice([now + 5, now - 5, now, now + 3600, 0.0])
        inp["share"] = {"paused": paused}
    return inp


def execute_case(inp: dict):
    effects: list[dict] = []
    queue_hours, breaker = inp["queue_hours"], inp["breaker"]
    skip = "','".join((reasons.KEY_PAUSED,) + reasons.SITE_LEVEL)

    def fetch(sql, p):
        if sql.startswith("SELECT CAST(ts/3600 AS INT) h FROM usage_log"):
            return "queue_hours", [(i,) for i in range(queue_hours)]
        if sql.startswith("SELECT SUM(CASE WHEN status='error'"):
            return "breaker", [(breaker["fails"], breaker["total"])]
        if sql.startswith("SELECT key_id, COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected'"):
            assert f"reason NOT IN ('{skip}')" in sql, sql
            return "rejects", [tuple(r) for r in inp["rejects"]]
        if sql == "SELECT id, name FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0":
            return "keys", [tuple(k) for k in inp["keys"]]
        raise AssertionError(sql)

    db = FakeDb(inp["settings"], effects, fetch)
    guard = None
    if inp["guard"] is not None:
        def trip_breaker(seconds, reason="", now=None):
            effects.append({"op": "trip_breaker", "seconds": seconds, "reason": reason, "now": now})
        guard = SimpleNamespace(breaker_until=inp["guard"]["breaker_until"], trip_breaker=trip_breaker)
    share = None
    if inp["share"] is not None:
        paused = {int(k): v for k, v in inp["share"]["paused"].items()}

        class Share:
            async def pause_key(self, key_id, seconds, reason, now=None):
                if paused.get(int(key_id), 0.0) > now:
                    return False
                effects.append({"op": "pause_key", "key_id": key_id, "seconds": seconds, "reason": reason, "now": now})
                paused[int(key_id)] = now + seconds
                return True
        share = Share()
    state = SimpleNamespace(db=db, guard=guard, share=share, sources=SimpleNamespace(events=[tuple(e) for e in inp["events"]]),
                            settings=SimpleNamespace(global_daily_v5=inp["global_daily_v5_default"]))
    registrar = None
    if inp["registrar"] is not None:
        reg = inp["registrar"]

        class Registrar:
            async def settings(self):
                return {"max_users": reg["max_users"], "open": True}

            async def count_active(self):
                return reg["active"]
        registrar = Registrar()

    async def set_registration(db_, body, state_=None, service=None):
        assert set(body) == {"max_users", "notify"} and body["notify"] is False, body
        effects.append({"op": "set_registration_max_users", "max_users": body["max_users"]})
    ops.set_registration = set_registration
    autopilot.log_action = fake_log(effects)
    try:
        out, err = run(autopilot.run(state, registrar, now=inp["now"])), None
    except Exception as e:  # noqa: BLE001
        out, err = None, type(e).__name__
    return out, err, effects, db._db.queries


def gen_run(count: int) -> None:
    stats = {"err": 0, "slots_applied": 0, "breaker_applied": 0, "pause_applied": 0, "observe_log": 0}
    with Writer("autopilot/run") as w:
        for _ in range(count):
            inp = gen_inputs()
            out, err, effects, queries = execute_case(inp)
            stats["err"] += err is not None
            if out:
                stats["slots_applied"] += bool(out["rules"]["slots"].get("applied"))
                stats["breaker_applied"] += bool(out["rules"]["breaker"].get("applied"))
                stats["pause_applied"] += sum(1 for d in out["rules"]["key_guard"]["value"] if d.get("applied"))
                stats["observe_log"] += any(e["op"] == "log" and e["action"] == "自动驾驶（观察）" for e in effects)
            w.record(fn="run", inputs=inp, out=out, err=err, effects=effects, queries=queries)
    print("run 场景统计:", stats)


if __name__ == "__main__":
    gen_pure()
    gen_run(800)
