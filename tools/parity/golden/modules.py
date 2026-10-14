"""模块登记（app/modules.py）里纯校验与文案的 golden 夹具。

做法：用假 state / 假 db 构造真正的 Module（modules.capacity(state) 等），调它们的 checks / tick / params，
把「假数据 + 旧代码真实输出」记下来；Rust 侧对同样的数字调 gate_core::modules 里的纯函数，结果必须逐字节相同。

输出（crates/gate-core/tests/golden/modules/）：
- meta.jsonl     各模块的静态元数据（名称 / 标题 / 问题 / 说明 / 面板参数名与说明）
- checks.jsonl   capacity / allocation / registration / registration_ready / observation / risk 的校验结果
- ticks.jsonl    capacity / allocation / autopilot 的 tick 摘要与副作用
- params.jsonl   面板参数的文案

运行（仓库根目录）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/modules.py
"""
from __future__ import annotations

import json
import random
import time
from datetime import datetime, timedelta
from types import SimpleNamespace
from unittest import mock

from _fakes import FakeDb, TZ, fake_log, run
from _golden import Writer

from app import action_log, autopilot, modules, quota_algo, risk_check
from app.kernel import Kernel

rng = random.Random(20261014 + 13)
NOOP = lambda *a, **k: None  # noqa: E731


def check_dict(c) -> dict:
    return dict(c.__dict__)


class Pool:
    """上游账号池：token_id / usable。`usable=None` 表示对象上没有这个属性（旧代码 getattr 默认当可用）。"""

    @staticmethod
    def make(specs):
        out = []
        for i, usable in enumerate(specs):
            ns = SimpleNamespace(token_id=f"tok{i}")
            if usable is not None:
                ns.usable = usable
            out.append(ns)
        return out


def make_state(settings, fetch, effects, *, guard=None, pool=None, day="2026-10-14", execute=None, nai_extra=None,
               st_settings=None):
    db = FakeDb(settings, effects, fetch, execute)
    nai = SimpleNamespace(pool=pool if pool is not None else [], **(nai_extra or {}))
    return SimpleNamespace(db=db, guard=guard, nai=nai, day=lambda ts=None: day,
                           settings=SimpleNamespace(image_min_interval=15.0, default_expires_days=30,
                                                    key_inactivity_delete_days=3, **(st_settings or {})))


def kernel_for(state, registrar=None) -> Kernel:
    k = Kernel(state, NOOP)
    if registrar is not None:
        k.extra["registrar"] = registrar
    return k


def usable_count(pool_specs) -> int:
    return sum(1 for u in pool_specs if u is None or u)


# ---------------------------------------------------------------------------
# 元数据
# ---------------------------------------------------------------------------
def gen_meta() -> None:
    state = make_state({}, lambda s, p: ("x", []), [], guard=SimpleNamespace(values={"account_daily_cap": 1000}))
    k = modules.build(state, NOOP)
    with Writer("modules/meta") as w:
        for name, m in k.modules.items():
            w.record(fn="meta", name=name, title=m.title, question=m.question, hard_note=m.hard_note,
                     default_enabled=m.default_enabled, report_bugs=m.report_bugs, periodic=m.tick is not None,
                     has_checks=m.checks is not None, custom_enabled=m.get_enabled is not None,
                     params=[[p.name, p.note] for p in m.params])
        w.record(fn="order", names=list(k.modules))


# ---------------------------------------------------------------------------
# capacity
# ---------------------------------------------------------------------------
def gen_snaps(now: float) -> list[dict]:
    r = rng.random()
    if r < 0.15:
        return []
    n = rng.choice([1, 2, 2, 3, 6, 12, 25])
    span = rng.choice([6 * 3600, 6 * 3600 - 1, 6 * 3600 + 1, 3600, 20 * 3600, 5.9 * 3600, 6.1 * 3600, 12 * 3600, 0])
    t1 = now - rng.uniform(0, 600)
    t0 = t1 - span
    ts = sorted([t0] + [rng.uniform(t0, t1) for _ in range(max(0, n - 2))] + ([t1] if n > 1 else []))
    if rng.random() < 0.5:
        ts = [float(int(t)) for t in ts]
    p = rng.choice([94.99, 95.0, 95.01, 94.0, 80.0, 60.5, 99.0, 30.0, 12.3])
    first = p + rng.choice([0.0, 0.5, 1.0, 3.0, 8.0, -2.0, 0.2])
    rows = []
    for i, t in enumerate(ts):
        pct = first if i == 0 else p
        if 0 < i < len(ts) - 1:
            pct = round(rng.uniform(min(p, first), max(p, first)), 1)
        rate = rng.choice([None, None, 0.0, 5.0, 11.0, 7.3, 14.0, 2.2])
        rows.append({"ts": t, "percent": round(pct, 2) if rng.random() < 0.5 else pct, "rate": rate})
    return rows


def gen_capacity_checks(count: int) -> None:
    with Writer("modules/checks_capacity") as w:
        for _ in range(count):
            now = 1_790_000_000.0 + rng.uniform(0, 1e6)
            specs = rng.choice([[], [True], [True], [None], [True, True], [True, False], [False], [None, True, False],
                                [True, True, True]])
            logged = rng.choice([0, 3, 10, 50, 100, 149, 150, 160, 300, rng.randint(0, 400)])
            cap_ceiling = rng.choice([100, 150, 160, 120, 70, 0, 20])
            n_tok = usable_count(specs)
            cap = cap_ceiling * n_tok
            kind = rng.random()
            if kind < 0.3:
                mem = max(0, logged - 2 + rng.choice([-1, 0, 1, 2]))
            else:
                mem = rng.choice([0, logged, logged + 5, rng.randint(0, 300)])
            if kind > 0.8:     # 上限边界：logged 恰好在 cap*1.05+2 两侧
                lim = cap * 1.05 + 2
                logged = int(lim) + rng.choice([-1, 0, 1])
            snaps = gen_snaps(now)
            counted = rng.choice([0, 20, 100, 200, 400, rng.randint(0, 800)])
            if len(snaps) >= 2:        # 把「每 1% ≈ 张」调到 7 / 28 的边界附近
                hours = (snaps[-1]["ts"] - snaps[0]["ts"]) / 3600
                rate = snaps[-1]["rate"] or quota_algo.V5_FALLBACK_RATE
                used = snaps[0]["percent"] - snaps[-1]["percent"] + rate * hours / 24
                if used > 0.5 and rng.random() < 0.6:
                    counted = int(used * rng.choice([6.9, 7.0, 7.1, 27.9, 28.0, 28.1, 14.2]))
            usable_ids = [f"tok{i}" for i, u in enumerate(specs) if u is None or u]
            hour_counts = {t: (mem if i == 0 else rng.randint(0, mem)) for i, t in enumerate(usable_ids)}

            def fetch(sql, p, snaps=snaps, logged=logged, counted=counted):
                if sql.startswith("SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND status='ok' AND kind LIKE 'image%'"):
                    return "logged", [(logged,)]
                if sql.startswith("SELECT ts, v5_percent, v5_rate FROM upstream_snapshots"):
                    return "snaps", [(s["ts"], s["percent"], s["rate"]) for s in snaps]
                if sql.startswith("SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND ts<=?"):
                    return "counted", [(counted,)]
                raise AssertionError(sql)
            guard = SimpleNamespace(values={"account_daily_cap": 1000},
                                    hour_count=lambda t, n, hc=hour_counts: hc[t],
                                    hourly_cap_ceiling=lambda n, c=cap_ceiling: c)
            effects: list = []
            state = make_state({}, fetch, effects, guard=guard, pool=Pool.make(specs))
            m = modules.capacity(state)
            with mock.patch.object(time, "time", return_value=now):
                out = run(m.checks(kernel_for(state)))
            inputs = {"tokens": n_tok, "logged": logged, "mem": max(hour_counts.values()) if hour_counts else 0,
                      "cap_ceiling": cap_ceiling, "snaps": snaps, "counted": counted}
            w.record(fn="capacity_checks", inputs=inputs, out=[check_dict(c) for c in out], queries=state.db._db.queries)


def gen_capacity_ticks(count: int) -> None:
    with Writer("modules/ticks_capacity") as w:
        for _ in range(count):
            now = 1_790_000_000.0 + rng.uniform(0, 1e6)
            last = rng.choice([None, None, 0.0, now - 3599.9, now - 3600.0, now - 3600.1, now - 100, now - 86400, now - 7200.5])
            changed = rng.choice([None, None, (150, 100), (100, 110), (150, 75)])
            hourly = rng.choice([100, 110, 150, 160, 75])
            accounts = [{"percent": rng.choice([None, 97, 88, 40]), "recharge_per_day": rng.choice([None, 11.0, 5.5])}
                        for _ in range(rng.choice([0, 1, 2]))]
            specs = rng.choice([[True], [True, True], [False], [], [None]])
            client = object() if rng.random() < 0.5 else None
            effects: list = []

            def fetch(sql, p, last=last):
                if sql == "SELECT MAX(ts) FROM upstream_snapshots":
                    return "max_ts", [(last,)]
                raise AssertionError(sql)

            def execute(sql, p, effects=effects):
                assert sql.startswith("INSERT INTO upstream_snapshots(ts, v5_percent, v5_rate, anlas, fresh) VALUES (?,?,?,?,?)"), sql
                effects.append({"op": "insert_snapshot", "row": list(p)})
                return "insert_snapshot"

            class Allow:
                async def snapshot(self, pool):
                    return {"accounts": [dict(a) for a in accounts]}

                async def resolve(self, *a):
                    return None

            async def adapt_daily(changed=changed):
                return changed
            guard = SimpleNamespace(values={"account_daily_cap": 1000, "account_hourly_cap": hourly}, adapt_daily=adapt_daily)
            state = make_state({}, fetch, effects, guard=guard, pool=Pool.make(specs), execute=execute,
                               nai_extra={"allowance": Allow(), "_client": client, "image_host": "h"})
            for t in state.nai.pool:
                t.token = "synthetic"
            m = modules.capacity(state)
            with mock.patch.object(time, "time", return_value=now), mock.patch.object(action_log, "log_action", fake_log(effects)):
                out = run(m.tick(kernel_for(state)))
            w.record(fn="capacity_tick", inputs={"now": now, "last": last, "changed": list(changed) if changed else None,
                                                  "hourly": hourly, "accounts": accounts, "fresh_possible": client is not None and any(
                                                      u is True for u in specs)},   # refresh_allowance 里 usable 缺省为 False
                     out=out, effects=effects)


# ---------------------------------------------------------------------------
# allocation
# ---------------------------------------------------------------------------
def gen_state_json(allow_bad: bool = True) -> str | None:
    r = rng.random()
    if r < 0.1:
        return None
    if r < 0.14:
        return rng.choice(["", "{}", "garbage", "[]", "null"])
    if allow_bad and r < 0.16:
        return rng.choice(["[1]", '"s"', "5"])
    v5 = rng.choice([
        {"each": 9, "global": 130}, {"each": 18, "global": 130}, {"each": 10, "global": 100}, {"each": 3, "global": 0},
        {"each": 0, "global": 50}, {}, None, {"each": 9}, {"global": 130}, {"each": 9.5, "global": 100},
        {"each": 10, "global": 100, "rate_check": rate_check()}, {"each": 9, "global": 130, "rate_check": rate_check()},
    ])
    d = {"enabled": rng.choice([True, True, True, False, None]), "ceiling": rng.choice([150, 100, 126, 0, None, 300, 175]), "v5": v5}
    if rng.random() < 0.1:
        del d["enabled"]
    if rng.random() < 0.05:
        del d["ceiling"]
    return json.dumps(d, ensure_ascii=False)


def rate_check():
    def v():
        return rng.choice([None, 5.0, 11.0, 7.34, 15.0, 4.55, 0.0, 12])
    d = {"api": v(), "measured": v(), "used": v(), "status": rng.choice(["已核对", "对不上", "未核对", None])}
    if rng.random() < 0.2:
        del d["status"]
    return d


def gen_allocation(count: int) -> None:
    with Writer("modules/checks_allocation") as w:
        for _ in range(count):
            raw = gen_state_json()
            settings = {} if raw is None else {"quota_algo_last": raw}
            ack = rng.choice(["", "", "1", "0", "yes"])
            if ack:
                settings["ack_v5_oversell"] = ack
            keys = rng.choice([0, 1, 5, 14, 23, 40, 60])
            drift = rng.choice([0, 0, 0, 1, 7])
            base_mode = rng.random()
            guard = None
            if base_mode < 0.85:
                vals = {"account_daily_cap": 1000}
                if rng.random() < 0.9:
                    vals["base_daily_images"] = rng.choice([100, 150, 126, 0, 175, 300, 99999])
                guard = SimpleNamespace(values=vals)
            params_seen: list = []

            def fetch(sql, p, keys=keys, drift=drift):
                if sql == "SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 AND enabled=1":
                    return "keys", [(keys,)]
                if sql == ("SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 "
                           "AND enabled=1 AND (daily_images<>? OR daily_v5<>?)"):
                    return "drift", [(0 if p[0] is None else drift,)]
                raise AssertionError(sql)
            effects: list = []
            state = make_state(settings, fetch, effects, guard=guard)
            m = modules.allocation(state)
            try:
                out = [check_dict(c) for c in run(m.checks(kernel_for(state)))]
                err = None
            except Exception as e:  # noqa: BLE001
                out, err = None, type(e).__name__
            queries = state.db._db.queries
            # drift 查询的参数是 (a, each)；a 为 NULL 时 SQL 恒不成立，所以喂给 Rust 的 drift 要还原成「服务层会查到的数」
            drift_in = drift
            for tag, ps in queries:
                if tag == "drift" and ps[0] is None:
                    drift_in = 0
            base = None if guard is None else guard.values.get("base_daily_images")
            base = 0 if guard is None else base
            w.record(fn="allocation_checks",
                     inputs={"last_raw": raw, "keys": keys, "drift": drift_in, "ack_oversell": bool(ack), "base": base},
                     out=out, err=err, queries=queries)


def gen_allocation_ticks(count: int) -> None:
    with Writer("modules/ticks_allocation") as w:
        for _ in range(count):
            kind = rng.random()
            if kind < 0.1:
                r = None
            elif kind < 0.2:
                r = {"enabled": False}
            elif kind < 0.25:
                r = {}
            else:
                r = {"enabled": True, "ceiling": rng.choice([150, 100, 175, 126]),
                     "v5": rng.choice([{"each": 9, "global": 130}, {"each": 18, "global": 0}, {}, None, {"each": 3}]),
                     "members": 5}
            state = make_state({}, lambda s, p: ("x", []), [], guard=SimpleNamespace(values={"account_daily_cap": 1000}))
            m = modules.allocation(state)

            async def fake_run(st, now=None, r=r):
                return r
            with mock.patch.object(quota_algo, "run", fake_run):
                out = run(m.tick(kernel_for(state)))
            w.record(fn="allocation_tick", inputs={"r": r}, out=out)


def gen_autopilot_ticks(count: int) -> None:
    with Writer("modules/ticks_autopilot") as w:
        for _ in range(count):
            kind = rng.random()
            if kind < 0.1:
                r = {"at": 1.0, "rules": {}}
            elif kind < 0.15:
                r = {"at": 1.0}
            elif kind < 0.2:
                r = {"at": 1.0, "rules": None}
            else:
                n = rng.choice([0, 0, 1, 2, 5])
                r = {"at": 1.0, "rules": {
                    "slots": {"mode": "observe", "value": 50, "why": "x"},
                    "breaker": rng.choice([{"mode": "observe", "value": rng.choice([True, False]), "why": "y"}, {}, None]),
                    "key_guard": rng.choice([{"mode": "enforce", "value": [{"key": i} for i in range(n)], "why": "z"}, {}, None,
                                             {"value": None}])}}
            state = make_state({}, lambda s, p: ("x", []), [])
            m = modules.autopilot_module(state)

            async def fake_run(st, registrar=None, now=None, r=r):
                return r
            with mock.patch.object(autopilot, "run", fake_run):
                out = run(m.tick(kernel_for(state)))
            w.record(fn="autopilot_tick", inputs={"r": r}, out=out)


# ---------------------------------------------------------------------------
# registration / registration_ready
# ---------------------------------------------------------------------------
def make_registrar(info: dict):
    class Reg:
        key_daily_images = info["env_daily_images"]
        key_daily_v5 = info["env_daily_v5"]
        max_users = info["env_max_users"]

        async def settings(self):
            return {"max_users": info["max_users"], "daily_images": info["daily_images"], "daily_v5": info["daily_v5"],
                    "open": True}

        async def count_active(self):
            return info["active"]
    return Reg()


def gen_registrar_info() -> dict:
    per = rng.choice([0, 100, 150, 125, 80, 300])
    return {"max_users": rng.choice([0, 30, 50, 100, 150]), "daily_images": per, "daily_v5": rng.choice([0, 5, 9, 18]),
            "active": rng.choice([0, 1, 7, 14, 30, 50, 51, 100, 101, 140]),
            "env_daily_images": rng.choice([0, 0, per, 100, 150]),
            "env_daily_v5": rng.choice([0, 0, 9, 5]),
            "env_max_users": rng.choice([0, 0, 50, 100, 30])}


def gen_registration(count: int) -> None:
    with Writer("modules/checks_registration") as w:
        for _ in range(count):
            info = gen_registrar_info() if rng.random() > 0.15 else None
            state = make_state({}, lambda s, p: ("x", []), [])
            m = modules.registration(state)
            out = run(m.checks(kernel_for(state, make_registrar(info) if info else None)))
            w.record(fn="registration_checks", inputs={"registrar": info}, out=[check_dict(c) for c in out])


def gen_registration_ready(count: int) -> None:
    with Writer("modules/checks_ready") as w:
        for _ in range(count):
            info = gen_registrar_info() if rng.random() > 0.12 else None
            specs = rng.choice([[], [True], [True, True], [None, True, True], [False], [True, False, True]])
            cap = rng.choice([1000, 1000, 0, 500, 2000, 300, 1])
            if info and rng.random() < 0.4:      # 在容量边界上
                n = max(1, usable_count(specs))
                site = cap * n
                info["daily_images"] = rng.choice([site // max(1, info["active"]) if info["active"] else 100, 100, 150])
                info["active"] = rng.choice([site // max(1, info["daily_images"] or 1), site // max(1, info["daily_images"] or 1) + 1, info["active"]])
            open_raw = rng.choice([None, "1", "0", "true", "off", "", "bogus", "yes"])
            ack = rng.choice(["", "", "1", "0"])
            hourly_raw = rng.choice([None, "12", "0", "500", "9999", "-3", "abc", "", "7.9", " 20 "])
            settings = {}
            if open_raw is not None:
                settings["register_open"] = open_raw
            if ack:
                settings["ack_registration_ready"] = ack
            if hourly_raw is not None:
                settings["issue_hourly_cap"] = hourly_raw
            guard = SimpleNamespace(values={"account_daily_cap": cap})
            state = make_state(settings, lambda s, p: ("x", []), [], guard=guard, pool=Pool.make(specs))
            m = modules.registration_ready(state)
            out = run(m.checks(kernel_for(state, make_registrar(info) if info else None)))
            # 面板参数
            params = {}
            for p in m.params:
                v = p.value()
                params[p.name] = run(v) if hasattr(v, "__await__") else v
            from app import site_flags
            open_now = run(site_flags.get(state.db, site_flags.REGISTER_OPEN))
            hourly = run(site_flags.get(state.db, site_flags.ISSUE_HOURLY_CAP, state.settings))
            w.record(fn="registration_ready_checks",
                     inputs={"open_now": bool(open_now), "registrar": info, "account_daily_cap": cap,
                             "tokens": usable_count(specs), "hourly": hourly, "ack": bool(ack)},
                     raw={"hourly_raw": hourly_raw},
                     out=[check_dict(c) for c in out], params=params)


# ---------------------------------------------------------------------------
# observation / risk
# ---------------------------------------------------------------------------
def gen_observation(count: int) -> None:
    with Writer("modules/checks_observation") as w:
        for _ in range(count):
            day = (datetime(2026, 9, 1) + timedelta(days=rng.randint(0, 150))).strftime("%Y-%m-%d")
            logged = rng.choice([0, 5, 100, 400, 1000, 1001, rng.randint(0, 2000)])
            kind = rng.random()
            if kind < 0.15:
                audited = 0
            else:
                tol = max(2, logged * 0.005)
                audited = max(1, int(logged + rng.choice([-1, 0, 1]) * tol + rng.choice([-1, 0, 1])))
            raw = gen_rate_check_state()
            settings = {} if raw is None else {"quota_algo_last": raw}

            def fetch(sql, p, logged=logged, audited=audited):
                if sql.startswith("SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>=? AND status='ok' AND kind LIKE 'image%'"):
                    return "logged", [(logged,)]
                if sql == "SELECT COUNT(*) FROM generation_audit WHERE ts>=? AND status='ok'":
                    return "audited", [(audited,)]
                raise AssertionError(sql)
            state = make_state(settings, fetch, [], day=day)
            m = modules.observation(state)
            try:
                out = [check_dict(c) for c in run(m.checks(kernel_for(state)))]
                err = None
            except Exception as e:  # noqa: BLE001
                out, err = None, type(e).__name__
            w.record(fn="observation_checks", inputs={"day": day, "logged": logged, "audited": audited, "quota_state": raw},
                     out=out, err=err, queries=state.db._db.queries)


def gen_rate_check_state() -> str | None:
    r = rng.random()
    if r < 0.1:
        return None
    if r < 0.18:
        return rng.choice(["", "{}", "garbage", '{"v5": null}', '{"v5": {}}', '{"v5": {"rate_check": {}}}', "[1]", '{"v5": [1]}',
                           '{"v5": {"rate_check": [1]}}'])
    return json.dumps({"enabled": True, "v5": {"each": 9, "rate_check": rate_check()}}, ensure_ascii=False)


def gen_risk(count: int) -> None:
    with Writer("modules/checks_risk") as w:
        for _ in range(count):
            items = []
            for _i in range(rng.choice([0, 1, 3, 6])):
                items.append({"title": rng.choice(["出站指纹", "共用迹象", "重复提示词"]), "detail": rng.choice(["正常", "有 3 个 Key 在同一网段", "x" * 5]),
                              "status": rng.choice(["ok", "warn", "bad", "na"]),
                              "evidence": [rng.choice(["a", "b", "猫头鹰"]) for _ in range(rng.choice([0, 1, 3]))]})
            state = make_state({}, lambda s, p: ("x", []), [])
            m = modules.risk(state)

            async def collect(st, now, items=items):
                return {"items": items}
            with mock.patch.object(risk_check, "collect", collect):
                out = run(m.checks(kernel_for(state)))
            w.record(fn="risk_checks", inputs={"items": items}, out=[check_dict(c) for c in out])


# ---------------------------------------------------------------------------
# 面板参数（带格式化的那几个）
# ---------------------------------------------------------------------------
def gen_params() -> None:
    with Writer("modules/params") as w:
        for burst in (0, 150, 300, None):
            for cap in (0, 1, 12, 36, 1000, 1500, 1020, 2500, 20000):
                vals = {"account_daily_cap": cap, "account_hourly_cap": 150}
                if burst is not None:
                    vals["daily_burst"] = burst
                state = make_state({}, lambda s, p: ("x", []), [], guard=SimpleNamespace(values=vals))
                m = modules.capacity(state)
                params = {p.name: p.value() for p in m.params}
                w.record(fn="capacity_params", inputs={"daily_burst": burst, "account_daily_cap": cap}, out=params)
        state = make_state({}, lambda s, p: ("x", []), [], guard=SimpleNamespace(values={"account_daily_cap": 1000}))
        m = modules.allocation(state)
        w.record(fn="allocation_params", out={p.name: p.value() for p in m.params if p.name != "今天每人 V4.5"})
        m = modules.autopilot_module(state)
        w.record(fn="autopilot_params", out={p.name: p.value() for p in m.params})
        for days in (0, 1, 3, 30):
            state.settings.key_inactivity_delete_days = days
            m = modules.registration(state)
            w.record(fn="registration_params", inputs={"days": days}, out={p.name: p.value() for p in m.params})
        for per in (0, 1, 3, 100, 150, 125, 300, -5):
            for daily in (0, 1000, 3000, 2999, 77):
                w.record(fn="seats_text", inputs={"per": per, "daily": daily},
                         out=(f"约 {daily // per} 人（{daily} 张/天 ÷ 每人 {per} 张）" if per else "—"))
        for cap in (0, 1, 12, 500):
            w.record(fn="hourly_text", inputs={"cap": cap}, out=(f"{cap} 个名额/小时" if cap else "不限"))


if __name__ == "__main__":
    gen_meta()
    gen_capacity_checks(900)
    gen_capacity_ticks(300)
    gen_allocation(900)
    gen_allocation_ticks(200)
    gen_autopilot_ticks(200)
    gen_registration(200)
    gen_registration_ready(900)
    gen_observation(600)
    gen_risk(100)
    gen_params()
