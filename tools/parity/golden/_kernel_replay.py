"""内核整轮回放的公共部分：种子数据构造、场景、假运行环境、回放与落盘（用法见 kernel_tick.py）。"""

from __future__ import annotations

import asyncio
import gzip
import hashlib
import json
import os
import random
import sqlite3
import tempfile
import time as _time
from collections import Counter
from datetime import datetime
from pathlib import Path
from types import SimpleNamespace
from zoneinfo import ZoneInfo

from _golden import ROOT, enc

OUT_DIR = ROOT / "crates" / "gate-server" / "tests" / "fixtures"
SH = ZoneInfo("Asia/Shanghai")

# 风险检查会列出代理环境变量：夹具必须与生成机器无关（也不能把本机代理地址带进公开仓库）
for _name in ("UPSTREAM_PROXY", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
              "http_proxy", "https_proxy", "all_proxy", "no_proxy"):
    os.environ.pop(_name, None)

# ---------------------------------------------------------------- 冻结时钟
_NOW = [0.0]
_REAL_TIME = _time.time
_time.time = lambda: _NOW[0]          # 旧代码全是 `time.time()` 动态取，替换模块属性即可

from app import action_log, alerts, autopilot, errors, modules, ops, quota_algo, risk_check, status_page  # noqa: E402
from app.database import Database  # noqa: E402
from app.guard import Guard  # noqa: E402
from app.nai import TokenState  # noqa: E402
from app.share_guard import ShareGuard  # noqa: E402


def T(y: int, mo: int, d: int, h: int = 0, mi: int = 0, s: int = 0) -> float:
    """本地（上海）时间 → Unix 秒。选在 2030 年：远大于真实时钟。"""
    return datetime(y, mo, d, h, mi, s, tzinfo=SH).timestamp()


HOUR, DAYS = 3600.0, 86400.0
RESET_HOUR = 7
NOW = T(2030, 3, 15, 12, 0, 0)                 # 服务日 2030-03-15（7 点切换）
TODAY, YDAY = "2030-03-15", "2030-03-14"
Y0 = T(2030, 3, 14, RESET_HOUR)                # 昨天（服务日）的起点
TOKENS = ["synthetic-token-alpha", "synthetic-token-bravo", "synthetic-token-charlie"]


def token_id(token: str) -> str:
    return "token-" + hashlib.sha256(token.encode()).hexdigest()[:16]


def service_day(ts: float) -> str:
    return datetime.fromtimestamp(ts - RESET_HOUR * 3600, SH).strftime("%Y-%m-%d")


# ---------------------------------------------------------------- 种子数据（带列名的 INSERT，Python 与 Rust 执行同一批语句）
def lit(v) -> str:
    if v is None:
        return "NULL"
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return repr(v)
    return "'" + str(v).replace("'", "''") + "'"


class Seed:
    def __init__(self) -> None:
        self.sql: list[str] = []

    def insert(self, table: str, **cols) -> None:
        names = ", ".join(cols)
        vals = ", ".join(lit(v) for v in cols.values())
        self.sql.append(f"INSERT INTO {table} ({names}) VALUES ({vals})")

    def setting(self, key: str, value) -> None:
        self.sql.append(f"INSERT INTO site_settings (key, value) VALUES ({lit(key)}, {lit(str(value))}) "
                        f"ON CONFLICT(key) DO UPDATE SET value=excluded.value")

    def setting_json(self, key: str, value) -> None:
        self.setting(key, json.dumps(value, ensure_ascii=False))

    def key(self, kid: int, name: str, *, daily_images=150, daily_v5=10, scope="all", quota_auto=1, enabled=1,
            is_admin=0, is_test=0, created_at=None, last_image_at=None) -> int:
        self.insert("api_keys", id=kid, name=name, token=f"nai-synthetic-{kid:04d}", enabled=enabled,
                    daily_images=daily_images, daily_v5=daily_v5, image_model_scope=scope, quota_auto=quota_auto,
                    is_admin=is_admin, is_test=is_test, created_at=created_at if created_at is not None else NOW - 10 * DAYS,
                    last_image_at=last_image_at)
        return kid

    def counter(self, key_id: int, day: str, images: int, legacy: int, v5: int = 0) -> None:
        self.insert("counters", key_id=key_id, day=day, images=images, legacy_free_images=legacy, v5=v5)

    def log(self, ts: float, key_id, *, kind="image", model="nai-diffusion-4-5-full", status="ok", images=1,
            detail="", reason="", wait_ms=0, dur_ms=8000, up_status=200, name="") -> None:
        self.insert("usage_log", ts=ts, key_id=key_id, key_name=name, kind=kind, model=model, status=status,
                    images=images, detail=detail, reason=reason, wait_ms=wait_ms, dur_ms=dur_ms, up_status=up_status)

    def upstream_counter(self, tok: str, day: str, images: int, v5: int = 0, units=None) -> None:
        self.insert("upstream_token_counters", token_id=token_id(tok), day=day, images=images, v5=v5,
                    units=float(images if units is None else units))

    def snapshot(self, ts: float, pct, rate=None, fresh=1) -> None:
        self.insert("upstream_snapshots", ts=ts, v5_percent=pct, v5_rate=rate, anlas=0, fresh=fresh)

    def audit(self, ts: float, key_id=None, status="ok") -> None:
        self.insert("generation_audit", ts=ts, key_id=key_id, key_name="", status=status)

    def pause(self, key_id: int, until: float, reason="synthetic pause") -> None:
        self.insert("share_state", key_id=key_id, paused_until=until, pause_reason=reason)


# ---------------------------------------------------------------- 场景
class Sc:
    def __init__(self, name: str, desc: str, *, tokens: int = 2, env=None, params=None, allowance=None,
                 registrar=None, token_state=None, guard_starts=None, seed: int = 0) -> None:
        self.name, self.desc = name, desc
        self.tokens = TOKENS[:tokens]
        self.env = {"image_min_interval": 15.0, "key_image_min_interval": 15.0, "default_expires_days": 30,
                    "key_inactivity_delete_days": 0, "global_daily_v5": 150, "post_jitter_min": 0.3,
                    "post_jitter_max": 1.2, **(env or {})}
        self.params = params
        self.allowance = allowance or {"accounts": [], "cached": False, "client": False, "resolve_fails": False}
        self.registrar = registrar
        self.token_state = token_state or {}
        self.guard_starts = guard_starts or []
        self.loose_errors = False             # 场景里有 Python 异常：Bug 的特征码 / 堆栈与异常消息不逐位比
        self.seed = Seed()
        self.steps: list[dict] = []
        self.rng = random.Random(f"kernel-tick:{name}:{seed}")

    def step(self, op: str, at: float, **kw) -> "Sc":
        self.steps.append({"op": op, "at": float(at), **kw})
        return self

    def spec(self) -> dict:
        return {"name": self.name, "desc": self.desc, "tokens": self.tokens, "env": self.env,
                "private_params": self.params, "allowance": self.allowance, "registrar": self.registrar,
                "token_state": self.token_state, "guard_starts": self.guard_starts,
                "loose_errors": self.loose_errors, "seed_sql": self.seed.sql, "steps": self.steps}


# ---------------------------------------------------------------- 假的运行环境
class RecordingAlerter(alerts.Alerter):
    """真实的冷却 / 静音逻辑，只是把「会发出去的文本」记下来而不是发 HTTP。"""

    def __init__(self) -> None:
        super().__init__(webhook_url="http://alert.invalid/hook")
        self.texts: list[str] = []

    async def _send(self, text: str) -> None:
        self.texts.append(text[:1900])
        self.sent += 1


class FakeAllowance:
    """`nai.allowance`：cached=True 时快照里直接有读数；否则要等 resolve 成功才有（刚重启的情形）。"""

    def __init__(self, spec: dict, n_tokens: int) -> None:
        self.accounts = list(spec["accounts"]) + [None] * max(0, n_tokens - len(spec["accounts"]))
        self.accounts = self.accounts[:n_tokens]
        self.cached, self.fails = spec["cached"], spec["resolve_fails"]
        self.resolved = False

    async def snapshot(self, pool) -> dict:
        shown = self.cached or self.resolved
        rows = []
        for a in self.accounts:
            rows.append({"percent": a["percent"] if (a and shown) else None,
                         "recharge_per_day": a.get("recharge_per_day") if (a and shown) else None})
        return {"threshold": 20, "accounts": rows, "mode": "on_demand"}

    async def resolve(self, client, host, token_id_, token) -> None:
        if self.fails:
            raise RuntimeError("upstream unavailable")
        self.resolved = True


class FakeRegistrar:
    """注册服务：设置沿用真实的 `ops.registration_settings`（库里的优先，其次环境默认值），名额占用数是定值。"""

    def __init__(self, db, spec: dict) -> None:
        self.db = db
        self.max_users = spec["max_users_env"]
        self.key_daily_images, self.key_daily_v5 = spec["daily_images_env"], spec["daily_v5_env"]
        self.key_features, self.key_image_scope, self.key_expires_days = None, "all", 30
        self.active = spec["active"]

    async def settings(self) -> dict:
        return await ops.registration_settings(self.db, self)

    async def count_active(self) -> int:
        return self.active


class FakeNai:
    def __init__(self, sc: Sc) -> None:
        self.pool = [TokenState(tok, i, 0, True) for i, tok in enumerate(sc.tokens)]
        for i, tok in enumerate(self.pool):
            spec = sc.token_state.get(str(i)) or {}
            if spec.get("admin_disabled"):
                tok.admin_enabled = False
            if spec.get("blocked_for"):
                tok.blocked_until = NOW + spec["blocked_for"]
            if spec.get("v5_limit"):
                tok.v5_daily_limit = spec["v5_limit"]
        self.allowance = FakeAllowance(sc.allowance, len(sc.tokens))
        self._client = object() if sc.allowance["client"] else None
        self.image_host = "http://upstream.invalid"


def json_roundtrip(v):
    return json.loads(json.dumps(enc(v), ensure_ascii=False))


def dump_tables(path: Path) -> dict:
    con = sqlite3.connect(path)
    con.row_factory = sqlite3.Row
    out = {}
    for (name,) in con.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").fetchall():
        rows = [{k: r[k] for k in r.keys()} for r in con.execute(f'SELECT * FROM "{name}" ORDER BY rowid')]
        if rows:
            out[name] = rows
    con.close()
    return out


def _canon(row: dict) -> str:
    return json.dumps(row, sort_keys=True, ensure_ascii=False)


def diff_tables(before: dict, after: dict) -> dict:
    out = {}
    for t in sorted(set(before) | set(after)):
        b, a = before.get(t, []), after.get(t, [])
        cb, ca = Counter(map(_canon, b)), Counter(map(_canon, a))
        added, removed = [], []
        left = Counter(cb)
        for row in a:
            k = _canon(row)
            if left.get(k, 0) > 0:
                left[k] -= 1
            else:
                added.append(row)
        left = Counter(ca)
        for row in b:
            k = _canon(row)
            if left.get(k, 0) > 0:
                left[k] -= 1
            else:
                removed.append(row)
        if added or removed:
            out[t] = {"added": added, "removed": removed}
    return out


async def run_scenario(sc: Sc) -> dict:
    tmp = Path(tempfile.mkdtemp(prefix="kernel-tick-"))
    path = tmp / "gate.db"
    _NOW[0] = NOW
    db = Database(str(path), "Asia/Shanghai")
    await db.connect()
    await status_page.ensure_schema(db)
    for stmt in sc.seed.sql:
        await db._db.execute(stmt)
    await db._db.commit()
    before = dump_tables(path)

    writes: list = []
    orig_set, orig_bulk = db.set_setting, db.set_settings_bulk

    async def rec_set(key, value):
        await orig_set(key, value)
        writes.append(["set", key, str(value)])

    async def rec_bulk(values):
        await orig_bulk(values)
        writes.append(["bulk", [[k, str(v)] for k, v in values.items()]])

    db.set_setting, db.set_settings_bulk = rec_set, rec_bulk

    alerter = RecordingAlerter()
    state = SimpleNamespace()
    state.db = db
    state.settings = SimpleNamespace(day_reset_hour=RESET_HOUR, image_min_interval=sc.env["image_min_interval"],
                                     key_image_min_interval=sc.env["key_image_min_interval"],
                                     default_expires_days=sc.env["default_expires_days"],
                                     key_inactivity_delete_days=sc.env["key_inactivity_delete_days"],
                                     global_daily_v5=sc.env["global_daily_v5"])
    state.tz = SH
    state.day = lambda ts=None: service_day(ts or _time.time())
    state.guard = Guard(db, "Asia/Shanghai", reset_hour=RESET_HOUR)
    state.nai = FakeNai(sc)
    state.share = ShareGuard(db)
    state.alerter = alerter
    state.bugs = errors.Tracker(db, notify=lambda kind, msg, cooldown: alerter.notify(kind, msg, cooldown=cooldown))
    # 私密参数（P(...)）：旧版读 data/private_params.json；这里直接换掉加载函数
    from app import params as _params
    _params._load = lambda: dict(sc.params or {})
    registrar = FakeRegistrar(db, sc.registrar) if sc.registrar else None

    def bug(source, exc=None, **kw):
        kw.setdefault("rid", "")
        kw.setdefault("key_id", None)
        return state.bugs.capture(source, exc, **kw)

    # risk 模块里「出站请求头」「TLS 指纹」两条在 3.0 换了文案（docs/v3-deviations.md #7）：两边都用固定占位，其余逐项对拍
    stub = lambda id_, title: risk_check._item(id_, title, "ok", "stub", [])  # noqa: E731
    risk_check._outbound_headers_item = lambda state_: stub("outbound_headers", "出站请求头")
    risk_check._tls_fingerprint_item = lambda: stub("tls_fingerprint", "TLS 指纹（JA3/JA4）")

    # GateState.load_runtime_limits：后台改过的出图间隔（库里存的整数）优先于环境变量，之后 settings 上就是整数
    for name_, lo, hi in (("image_min_interval", 15, 120), ("key_image_min_interval", 15, 120)):
        raw = await db.get_setting("runtime_" + name_, None)
        try:
            val = int(raw)
        except (TypeError, ValueError):
            continue
        if lo <= val <= hi:
            setattr(state.settings, name_, val)
    await state.guard.load()
    await state.share.load()
    for idx, ts, images in sc.guard_starts:
        state.guard.record_start(token_id(sc.tokens[idx]), ts, images)
    kernel = modules.build(state, bug)
    kernel.extra["registrar"] = registrar
    await kernel.load()                                              # 启动时读回上次的运行记录

    steps_out = []
    sent_before = 0
    for step in sc.steps:
        _NOW[0] = step["at"]
        writes.clear()
        rec = {"op": step["op"], "at": step["at"]}
        try:
            op = step["op"]
            if op == "kernel_load":
                await kernel.load()
            elif op == "tick_all":
                rec["result"] = json_roundtrip(await kernel.tick_all())
            elif op == "quota_run":
                rec["result"] = json_roundtrip(await quota_algo.run(state))
            elif op == "autopilot_run":
                rec["result"] = json_roundtrip(await autopilot.run(state, registrar))
            elif op == "snapshot":
                rec["result"] = json_roundtrip(await kernel.snapshot())
            elif op == "set_enabled":
                await kernel.set_enabled(step["name"], step["on"])
            elif op == "sql":
                for stmt in step["sql"]:
                    await db._db.execute(stmt)
                await db._db.commit()
            elif op == "guard_load":
                await state.guard.load()
            elif op == "trip_breaker":
                state.guard.trip_breaker(step["seconds"], "synthetic", _NOW[0])
            elif op == "probe":
                rec["result"] = json_roundtrip({
                    "breaker_until": state.guard.breaker_until,
                    "paused": {str(k): v for k, v in state.share.paused.items()},
                    "guard_values": state.guard.values,
                })
            else:
                raise AssertionError(f"未知步骤 {op}")
        except Exception as exc:  # noqa: BLE001 — 异常类型本身就是要对拍的行为
            rec["error"] = type(exc).__name__
        await state.bugs.drain()
        while alerter._tasks:
            await asyncio.gather(*list(alerter._tasks), return_exceptions=True)
        rec["settings_writes"] = [list(w) for w in writes]
        rec["alerts"] = alerter.texts[sent_before:]
        sent_before = len(alerter.texts)
        steps_out.append(rec)

    await db.close()
    after = dump_tables(path)
    return {"fn": "scenario", **sc.spec(), "expect": {"steps": steps_out, "tables_diff": diff_tables(before, after)}}


def save(name: str, lines: list[str]) -> None:
    """超过 256 KB 的夹具存成 .jsonl.gz（mtime 固定为 0，重复生成逐字节一致）。"""
    path = OUT_DIR / f"{name}.jsonl"
    gz = path.with_name(path.name + ".gz")
    for stale in (path, gz):
        stale.unlink(missing_ok=True)
    data = ("\n".join(lines) + "\n").encode("utf-8")
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    if len(data) > 256 * 1024:
        gz.write_bytes(gzip.compress(data, compresslevel=9, mtime=0))
        path = gz
    else:
        path.write_bytes(data)
    print(f"{path.relative_to(ROOT)}: {len(lines)} 个场景")
