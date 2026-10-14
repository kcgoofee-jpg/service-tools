"""gate-db 数据访问层 golden 夹具：对旧 `app/database.py::Database` 的每个方法造场景并记录结果。

每个场景：
  1. 在临时目录里用真实的 `Database.connect()` 建一个全新库；
  2. 用 Python API（create_key / add_log / record_success / bump_counters ...）按固定时间造合成数据
     （`app.database.time` 被替换成可控的假时钟，`uuid4` 换成计数器）；
  3. 记录调用前的库快照 `seed_sql`（`iterdump()` 里的 INSERT 语句 + sqlite_sequence）和原图目录快照；
  4. 调用被测方法，记录 `out` / 异常，并把调用后所有非空表的整表内容（按 rowid）与原图目录一并记下。

Rust 侧（crates/gate-db/tests/repo_golden.rs）在空库里执行 seed_sql，调用同名 Rust 方法，逐项比较。

用法（仓库根目录，Python 3.12）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/db_methods.py

所有数据都是合成的（假 Token / 假 Discord ID / 假提示词），仓库是公开的。
"""
from __future__ import annotations

import asyncio
import base64
import itertools
import os
import sqlite3
import sys
import tempfile
import time as real_time
import types
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

from _golden import ROOT, _dump, enc  # 导入时会把 TZ 设成 Asia/Shanghai 并加入 sys.path

import app.database as D  # noqa: E402
from app.database import Database  # noqa: E402

OUT_DIR = ROOT / "crates" / "gate-db" / "tests" / "golden"
SH = ZoneInfo("Asia/Shanghai")


def T(y: int, mo: int, d: int, h: int = 0, mi: int = 0, s: int = 0) -> float:
    """本地（上海）时间 → Unix 秒。选在 2030 年：远大于真实时钟，原图目录 mtime 对拍时不受真实时间影响。"""
    return datetime(y, mo, d, h, mi, s, tzinfo=SH).timestamp()


NOW = T(2030, 3, 15, 12, 0, 0)
DAY = "2030-03-15"
HOUR, DAYS = 3600.0, 86400.0


# ---------------------------------------------------------------- 假时钟 / 假 uuid
class FakeTime:
    now = 0.0

    def time(self) -> float:
        return FakeTime.now

    def strftime(self, fmt, t=None):
        return real_time.strftime(fmt, real_time.localtime(FakeTime.now) if t is None else t)

    def __getattr__(self, name):
        return getattr(real_time, name)


D.time = FakeTime()
_uuid_counter = itertools.count(1)
_uuid_used: list[str] = []


def _fake_uuid4():
    h = f"{next(_uuid_counter):032x}"
    _uuid_used.append(h)
    return types.SimpleNamespace(hex=h)


D.uuid4 = _fake_uuid4


def at(t: float) -> None:
    FakeTime.now = float(t)


# ---------------------------------------------------------------- 快照 / 序列化
def norm(v):
    """sqlite3.Row / 元组 → 普通 dict / list，便于 enc。"""
    if isinstance(v, sqlite3.Row):
        return {k: norm(v[k]) for k in v.keys()}
    if isinstance(v, dict):
        return {k: norm(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [norm(x) for x in v]
    return v


def dump_seed(path: Path) -> list[str]:
    """调用前的库内容：`iterdump()` 里的 INSERT 语句；sqlite_sequence 放最后（先清再写，AUTOINCREMENT 计数要原样）。"""
    con = sqlite3.connect(path)
    rows, seq = [], []
    for line in con.iterdump():
        if line.startswith('INSERT INTO "sqlite_sequence"'):
            seq.append(line)
        elif line.startswith('INSERT INTO "'):
            rows.append(line)
    con.close()
    if seq:
        rows.append('DELETE FROM "sqlite_sequence";')
        rows.extend(seq)
    return rows


def dump_tables(path: Path) -> dict:
    con = sqlite3.connect(path)
    con.row_factory = sqlite3.Row
    out = {}
    for (name,) in con.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").fetchall():
        rows = [dict(r) for r in con.execute(f'SELECT * FROM "{name}" ORDER BY rowid')]
        if rows:
            out[name] = rows
    con.close()
    return out


def snap_files(root: Path, with_mtime: bool = False, extra=()):
    """原图目录快照：({相对路径: base64}, [子目录相对路径])。root 本身是文件时记为路径 ""。
    extra：目录之外、但相对 root 可寻址的文件（如 "../outside.png"，测越界保护）。"""
    files, dirs = {}, []
    if root.is_file():
        files[""] = base64.b64encode(root.read_bytes()).decode()
    elif root.is_dir():
        for p in sorted(root.rglob("*")):
            rel = p.relative_to(root).as_posix()
            if p.is_dir():
                dirs.append(rel)
            elif p.is_file():
                files[rel] = base64.b64encode(p.read_bytes()).decode()
    for rel in extra:
        p = root / rel
        if p.is_file():
            files[rel] = base64.b64encode(p.read_bytes()).decode()
    if not with_mtime:
        return files, dirs
    def canon(m: float) -> float:
        # 脚本显式 utime 过的都在 2030 年；真实时钟写出来的 mtime 每次运行都不同，统一换成固定值（仍早于所有 older_than）
        return m if m >= 1.85e9 else 1_767_225_600.0

    seed_files = [{"path": rel, "b64": b, "mtime": canon((root / rel if rel else root).stat().st_mtime)}
                  for rel, b in files.items()]
    seed_dirs = [{"path": rel, "mtime": canon((root / rel).stat().st_mtime)} for rel in dirs]
    return seed_files, seed_dirs


class Rec:
    """一个夹具文件（一个领域）。"""

    def __init__(self, area: str):
        self.area = area
        self.lines: list[str] = []
        self.counts: dict[str, int] = {}

    async def sc(self, method, args=(), kwargs=None, *, setup=None, now=NOW, rule_version="",
                 extra_ddl=(), note=""):
        kwargs = kwargs or {}
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "gate.db"
            D.RULE_VERSION = rule_version
            at(NOW - 5 * DAYS)
            db = Database(str(path))
            await db.connect()
            for ddl in extra_ddl:
                await db._db.execute(ddl)
                await db._db.commit()
            if setup is not None:
                await setup(db)
            seed_sql = dump_seed(path)
            extra_seed = list(getattr(db, "_extra_seed", []))
            seed_files, seed_dirs = snap_files(db.image_dir, with_mtime=True, extra=extra_seed)
            rec_args = enc(norm(list(args)))
            rec_kwargs = enc(norm(dict(kwargs)))
            _uuid_used.clear()
            at(now)
            err, out = None, None
            try:
                out = await getattr(db, method)(*args, **kwargs)
            except Exception as e:  # noqa: BLE001 —— 异常本身就是要对拍的行为
                err = {"type": type(e).__name__, "msg": str(e)}
            tables_after = dump_tables(path)
            files_after, dirs_after = snap_files(db.image_dir, extra=extra_seed)
            await db.close()
        rec = {
            "area": self.area, "method": method, "args": rec_args, "kwargs": rec_kwargs, "now": now,
            "rule_version": rule_version, "uuids": list(_uuid_used), "extra_ddl": list(extra_ddl),
            "seed_sql": seed_sql, "seed_files": seed_files, "seed_dirs": seed_dirs,
            "out": enc(norm(out)), "err": err, "tables_after": enc(tables_after),
            "files_after": files_after, "dirs_after": dirs_after,
        }
        if note:
            rec["note"] = note
        self.lines.append(_dump(rec))
        self.counts[method] = self.counts.get(method, 0) + 1

    def save(self) -> None:
        OUT_DIR.mkdir(parents=True, exist_ok=True)
        path = OUT_DIR / f"{self.area}.jsonl"
        path.write_text("\n".join(self.lines) + "\n", encoding="utf-8")
        print(f"{path.relative_to(ROOT)}: {len(self.lines)} 条  " +
              ", ".join(f"{m}×{n}" for m, n in sorted(self.counts.items())))


# ---------------------------------------------------------------- 造数据的小工具（都走 Python 原 API）
async def sql(db, q, args=()):
    await db._db.execute(q, args)
    await db._db.commit()


async def mkkey(db, name="m1", t=None, **over):
    """建 Key，返回行（dict）。t 为 None 时用当前假时钟。"""
    if t is not None:
        at(t)
    fields = dict(name=name, token=f"nai-{name}-0123456789abcdef", daily_images=100, daily_anlas=0,
                  daily_v5=0, monthly_anlas=500.0, daily_text_tokens=150000, rpm=10)
    fields.update(over)
    return dict(await db.create_key(fields))


async def mklog(db, k, t, kind="image", status="ok", model="nai-diffusion-4-5-full", **kw):
    """写一条日志（add_log）。k 为 None 表示没有 Key（key_id=NULL）。"""
    at(t)
    await db.add_log(k["id"] if k else None, k["name"] if k else "", kind, model, status, **kw)


async def mkok(db, k, t, day=DAY, kind="image", model="nai-diffusion-4-5-full", **kw):
    """记一次成功（record_success）。"""
    at(t)
    await db.record_success(k["id"], k["name"], kind, model, day, **kw)


def setups(*fns):
    """把多个 setup 串起来。"""
    async def run(db):
        for f in fns:
            await f(db)
    return run


PNG = b"\x89PNG\r\n\x1a\n" + bytes(range(16))
WEBP = b"RIFF\x00\x00\x00\x00WEBPVP8 " + bytes(range(8))
JPG = b"\xff\xd8\xff\xe0" + bytes(range(12))
THUMB = b"\xff\xd8thumb" + bytes(range(6))


# ================================================================ reconciliation
async def area_reconciliation():
    r = Rec("reconciliation")

    # ---- reconciliation_totals
    await r.sc("reconciliation_totals", note="空库：SUM 为 NULL → COALESCE 给整数 0")

    async def rich(db):
        a, b = await mkkey(db, "a"), await mkkey(db, "b")
        await mkok(db, a, NOW - HOUR, images=2, anlas=3.5, unconfirmed_anlas=1.5)
        await mkok(db, b, NOW - HOUR + 1, images=1, anlas=0.25)
        await mklog(db, a, NOW - HOUR + 5, status="error", unconfirmed_anlas=0.75)
        await mklog(db, a, NOW - HOUR + 10, status="rejected")
    await r.sc("reconciliation_totals", setup=rich)

    async def deleted_and_purged(db):
        a = await mkkey(db, "a")
        await mkok(db, a, NOW - 2 * HOUR, images=1, anlas=2.0, unconfirmed_anlas=0.5)
        await db.delete_key(a["id"])                      # 已删除 Key 的计数仍计入累计
        at(NOW)
        await db.purge_usage_log(NOW)                     # 日志清空后 MAX(id) 回到 0
    await r.sc("reconciliation_totals", setup=deleted_and_purged, note="日志清空但计数器还在")

    async def logs_only(db):
        a = await mkkey(db, "a")
        await mklog(db, a, NOW - HOUR, status="error", unconfirmed_anlas=2.25)
        await mklog(db, a, NOW - HOUR + 1, status="error", unconfirmed_anlas=0.0)
    await r.sc("reconciliation_totals", setup=logs_only, note="只有日志没有计数器")

    # ---- save_reconciliation
    await r.sc("save_reconciliation", [{"ts": NOW, "account_anlas": 1234.5, "ledger": {"anlas": 20.0, "n": 3},
                                         "items": [1, 2.5, None, True, "x"], "说明": "对账正常\n第二行 \"引号\" \\ 反斜杠"}])
    await r.sc("save_reconciliation", [{}])
    await r.sc("save_reconciliation", [{"a": 1e16, "b": 1e-7, "c": -0.0, "d": 123456789012345678, "e": 0.1 + 0.2,
                                         "f": 5e-324, "g": 1.7976931348623157e308}])
    await r.sc("save_reconciliation", [[1, "二", {"三": 3.0}]], note="非 dict 的快照也能存")

    async def pre_saved(db):
        at(NOW - HOUR)
        await db.save_reconciliation({"first": 1})
    await r.sc("save_reconciliation", [{"second": 2}], setup=pre_saved)

    # ---- reconciliation_history
    await r.sc("reconciliation_history", note="空表")

    async def three(db):
        for i in range(3):
            await db.save_reconciliation({"ts": NOW - (3 - i) * HOUR, "anlas": 10.5 * (i + 1), "ok": i % 2 == 0})
    await r.sc("reconciliation_history", setup=three)
    await r.sc("reconciliation_history", [2], setup=three)
    await r.sc("reconciliation_history", [0], setup=three, note="limit=0 → 夹到 1")
    await r.sc("reconciliation_history", [-5], setup=three, note="负数 → 夹到 1")

    async def many(db):
        for i in range(25):
            await db.save_reconciliation({"seq": i})
    await r.sc("reconciliation_history", [100], setup=many, note="上限 20")
    await r.sc("reconciliation_history", [], setup=many, note="默认 20")

    async def with_id_key(db):
        await db.save_reconciliation({"id": 999, "x": "y", "z": 1.0})
        await db.save_reconciliation({"中": "文", "id": "s"})
    await r.sc("reconciliation_history", setup=with_id_key, note="快照里已有 id 键：值被覆盖、位置不变")

    async def bad_json(db):
        await sql(db, "INSERT INTO anlas_reconciliations(snapshot) VALUES (?)", ("{not json",))
    await r.sc("reconciliation_history", setup=bad_json, note="库里存了坏 JSON → 旧版抛 JSONDecodeError")
    r.save()


# ================================================================ upstream token
async def area_upstream():
    r = Rec("upstream")
    TOK = "token-abc123"

    # ---- get_upstream_counter
    await r.sc("get_upstream_counter", [TOK, DAY], note="没有记录")

    async def v5_only(db):
        await db.bump_upstream_v5_counter(TOK, DAY)       # 新行 units 为 NULL
        await db.bump_upstream_v5_counter(TOK, DAY)
    await r.sc("get_upstream_counter", [TOK, DAY], setup=v5_only, note="units 为 NULL 且 images=0")

    async def with_units(db):
        await db.bump_upstream_image_counter(TOK, DAY, 3, 0.5)
        await db.bump_upstream_v5_counter(TOK, DAY)
    await r.sc("get_upstream_counter", [TOK, DAY], setup=with_units)
    await r.sc("get_upstream_counter", [TOK, "2030-03-14"], setup=with_units, note="别的日期")

    async def null_units_images(db):
        await sql(db, "INSERT INTO upstream_token_counters(token_id, day, images, v5) VALUES (?,?,?,?)",
                  (TOK, DAY, 7, 2))
    await r.sc("get_upstream_counter", [TOK, DAY], setup=null_units_images, note="老数据：units NULL → 按 images")

    # ---- migrate_upstream_token_ids
    await r.sc("migrate_upstream_token_ids", [[]])

    async def old_ids(db):
        for tid, day, imgs, v5 in (("token-1-abc123", DAY, 4, 1), ("token-2-abc123", DAY, 3, 0),
                                   ("token-1-abc123", "2030-03-14", 5, 2), ("token-9-zzzzzz", DAY, 8, 8)):
            await sql(db, "INSERT INTO upstream_token_counters(token_id, day, images, v5, units) VALUES (?,?,?,?,?)",
                      (tid, day, imgs, v5, imgs * 0.5))
    await r.sc("migrate_upstream_token_ids", [[TOK]], setup=old_ids, note="旧位置编号合并到哈希标识；不带 units")

    async def old_ids_with_target(db):
        await old_ids(db)
        await db.bump_upstream_image_counter(TOK, DAY, 10, 1.0)    # 目标已有同日行：累加
        await db.bump_upstream_v5_counter(TOK, DAY)
    await r.sc("migrate_upstream_token_ids", [[TOK, TOK]], setup=old_ids_with_target, note="重复的 token_id 去重")
    await r.sc("migrate_upstream_token_ids", [["abc123"]], setup=old_ids, note="没有 token- 前缀：后缀就是整个串")
    await r.sc("migrate_upstream_token_ids", [["token-nomatch"]], setup=old_ids, note="没有匹配的旧行")

    async def like_wild(db):
        await sql(db, "INSERT INTO upstream_token_counters(token_id, day, images, v5) VALUES (?,?,?,?)",
                  ("token-1-axb", DAY, 6, 1))
        await sql(db, "INSERT INTO upstream_token_counters(token_id, day, images, v5) VALUES (?,?,?,?)",
                  ("TOKEN-2-AXB", DAY, 1, 1))
    await r.sc("migrate_upstream_token_ids", [["token-a_b"]], setup=like_wild,
               note="LIKE 里的 _ 是通配符、且不分大小写：旧版的怪癖照抄")

    async def two_targets(db):
        for tid, imgs in (("token-1-aaaa", 1), ("token-1-bbbb", 2), ("token-aaaa", 10), ("token-bbbb", 20)):
            await sql(db, "INSERT INTO upstream_token_counters(token_id, day, images, v5) VALUES (?,?,?,?)",
                      (tid, DAY, imgs, 0))
    await r.sc("migrate_upstream_token_ids", [["token-aaaa", "token-bbbb"]], setup=two_targets,
               note="两个目标都已有同日行，只发生累加，结果与遍历顺序无关")

    # ---- move_upstream_token
    async def full_old(db):
        for tid in ("token-old", "token-new"):
            await db.set_upstream_token_limit(tid, 50 if tid == "token-old" else 5)
            await db.set_upstream_token_enabled(tid, tid == "token-old")
            await db.set_upstream_token_image_concurrency(tid, 3 if tid == "token-old" else 1)
            await db.bump_upstream_image_counter(tid, DAY, 2 if tid == "token-old" else 9, 1.0)
        await db.bump_upstream_image_counter("token-old", "2030-03-14", 4, 0.5)
    await r.sc("move_upstream_token", ["token-old", "token-new"], setup=full_old, note="新标识原有数据被覆盖")
    await r.sc("move_upstream_token", ["token-old", "token-old"], setup=full_old, note="相同标识：什么都不做")

    async def only_new(db):
        await db.set_upstream_token_limit("token-new", 5)
        await db.bump_upstream_v5_counter("token-new", DAY)
    await r.sc("move_upstream_token", ["token-missing", "token-new"], setup=only_new, note="旧标识没有数据：只清掉新标识的")

    async def only_old_counters(db):
        await db.bump_upstream_image_counter("token-old", DAY, 1, 1.0)
    await r.sc("move_upstream_token", ["token-old", "token-fresh"], setup=only_old_counters)

    # ---- limits / enabled / concurrency getters + setters
    await r.sc("get_upstream_token_limits", note="空")
    await r.sc("get_upstream_token_enabled")
    await r.sc("get_upstream_token_image_concurrency")

    async def limits(db):
        await db.set_upstream_token_limit("token-b", 80)
        await db.set_upstream_token_limit("token-a", 40)
        await db.set_upstream_token_limit("token-b", 90)
    await r.sc("get_upstream_token_limits", setup=limits)

    async def moved(db):
        await full_old(db)
        await db.move_upstream_token("token-old", "token-moved")
        await db.set_upstream_token_limit("token-zzz", 7)
    await r.sc("get_upstream_token_limits", setup=moved, note="move 之后：新标识带着旧设置")
    await r.sc("get_upstream_token_limits", setup=full_old, note="两个标识各一条")

    async def enabled(db):
        await db.set_upstream_token_enabled("token-b", False)
        await db.set_upstream_token_enabled("token-a", True)
        await db.set_upstream_token_enabled("token-c", 0)
        await db.set_upstream_token_enabled("token-b", True)
    await r.sc("get_upstream_token_enabled", setup=enabled)
    await r.sc("get_upstream_token_enabled", setup=moved, note="enabled 为 0 / 1 → False / True")
    await r.sc("get_upstream_token_enabled", setup=full_old)

    async def conc(db):
        await db.set_upstream_token_image_concurrency("token-a", 2)
        await db.set_upstream_token_image_concurrency("token-b", 1)
        await db.set_upstream_token_image_concurrency("token-a", 4)
    await r.sc("get_upstream_token_image_concurrency", setup=conc)
    await r.sc("get_upstream_token_image_concurrency", setup=moved)
    await r.sc("get_upstream_token_image_concurrency", setup=full_old)

    await r.sc("set_upstream_token_limit", ["token-a", 60])
    await r.sc("set_upstream_token_limit", ["token-a", 0], setup=limits)
    await r.sc("set_upstream_token_limit", ["token-z", -1], setup=limits)
    await r.sc("set_upstream_token_enabled", ["token-a", True])
    await r.sc("set_upstream_token_enabled", ["token-b", False], setup=enabled)
    await r.sc("set_upstream_token_enabled", ["token-a", False], setup=enabled)
    await r.sc("set_upstream_token_image_concurrency", ["token-a", 3])
    await r.sc("set_upstream_token_image_concurrency", ["token-a", 1], setup=conc)
    await r.sc("set_upstream_token_image_concurrency", ["token-q", 0], setup=conc)

    # ---- get_upstream_v5_counter
    await r.sc("get_upstream_v5_counter", [TOK, DAY], note="没有记录 → 0")
    await r.sc("get_upstream_v5_counter", [TOK, DAY], setup=with_units)
    await r.sc("get_upstream_v5_counter", [TOK, "2030-03-14"], setup=with_units)

    # ---- bump_upstream_image_counter
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2])
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2, 0.5])
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 0, 1.0], note="images<1：什么都不做")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, -3, 1.0])
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2, -1.5], note="负权重按 0")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2, float("nan")], note="NaN 权重：max(0.0, nan) = 0.0")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 3, 2], note="整数权重")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2, 0.5], setup=with_units, note="累加：units=COALESCE(units,images)+新")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2, 1.0], setup=v5_only, note="已有行 units 为 NULL")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 2, 0.25], setup=null_units_images, note="老行 units NULL, images=7")
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 1, 0.1], setup=with_units)
    await r.sc("bump_upstream_image_counter", [TOK, DAY, 1, float("inf")], note="无穷权重照存")

    # ---- bump_upstream_v5_counter
    await r.sc("bump_upstream_v5_counter", [TOK, DAY])
    await r.sc("bump_upstream_v5_counter", [TOK, DAY], setup=v5_only)
    await r.sc("bump_upstream_v5_counter", [TOK, DAY], setup=with_units, note="已有出图计数的行")
    await r.sc("bump_upstream_v5_counter", [TOK, "2030-03-16"], setup=with_units, note="新的一天")
    r.save()


# ================================================================ site_settings
async def area_settings():
    r = Rec("settings")

    async def seeded(db):
        await db.set_setting("alpha", "1")
        await db.set_setting("empty", "")
        await db.set_setting("中文键", "值 ✓")
        await db.set_setting("num", 15)
    await r.sc("get_setting", ["alpha"], setup=seeded)
    await r.sc("get_setting", ["empty"], setup=seeded, note="空串不是缺失")
    await r.sc("get_setting", ["中文键"], setup=seeded)
    await r.sc("get_setting", ["missing"], setup=seeded, note="缺失 → None")
    await r.sc("get_setting", ["missing", "dflt"], setup=seeded, note="缺失 → 调用方的默认值")
    await r.sc("get_setting", ["missing", 0], setup=seeded, note="默认值可以是任意类型")
    await r.sc("get_setting", ["num", 0], setup=seeded, note="数字设置读出来是字符串")

    await r.sc("set_setting", ["k", "v"])
    await r.sc("set_setting", ["alpha", "2"], setup=seeded, note="覆盖")
    await r.sc("set_setting", ["k", 5])
    await r.sc("set_setting", ["k", 1.5])
    await r.sc("set_setting", ["k", 0.1 + 0.2])
    await r.sc("set_setting", ["k", 1e16], note="str(1e16) = '1e+16'")
    await r.sc("set_setting", ["k", 1e-5])
    await r.sc("set_setting", ["k", 100.0])
    await r.sc("set_setting", ["k", True])
    await r.sc("set_setting", ["k", False])
    await r.sc("set_setting", ["k", None], note="str(None) = 'None'")
    await r.sc("set_setting", ["k", "含中文 ✓ 和 'quote'"])
    await r.sc("set_setting", ["k", 12345678901234567890])
    await r.sc("set_setting", ["", ""], note="空键空值")

    await r.sc("set_settings_bulk", [{}])
    await r.sc("set_settings_bulk", [{"a": 1, "b": 2.5, "c": True, "d": None, "e": "文本"}])
    await r.sc("set_settings_bulk", [{"alpha": 9, "new": "n", "num": 0.5}], setup=seeded, note="部分覆盖 + 部分新增")
    r.save()


# ================================================================ coupons
async def area_coupons():
    r = Rec("coupons")
    await r.sc("add_coupon", [1, "reset", 3])
    await r.sc("add_coupon", [1, "reset", 0.5, "唱得真好听", "站长"])
    await r.sc("add_coupon", [2, "reset", 1.5, "n" * 250, "b" * 50], note="note 截 200，by 截 40")
    await r.sc("add_coupon", [2, "reset", 1, "中" * 201, "站" * 41], note="按码点截断")
    await r.sc("add_coupon", [3, "reset", -1, "已过期的券"], note="天数为负：发出就已过期")
    await r.sc("add_coupon", [3, "reset", 0.1 + 0.2, "", ""], note="浮点天数")
    await r.sc("add_coupon", [1, "gift", 7, "x"], kwargs={"by": "kw"})

    async def coupons(db):
        at(NOW - 2 * DAYS)
        await db.add_coupon(1, "reset", 5, "活跃-晚过期")      # 过期 = NOW+3d
        at(NOW - 2 * DAYS)
        await db.add_coupon(1, "reset", 2.0, "到期整点")       # 过期 = NOW 整
        at(NOW - 2 * DAYS + 2)
        await db.add_coupon(1, "reset", 3, "活跃-早过期")      # 过期 = NOW+1d
        at(NOW - 5 * DAYS)
        await db.add_coupon(1, "reset", 1, "早已过期")
        at(NOW - 3 * DAYS)
        c = await db.add_coupon(1, "reset", 10, "已使用")
        at(NOW - 2 * DAYS)
        await db.use_coupon(c["id"], 1)
        at(NOW - DAYS)
        await db.add_coupon(2, "reset", 9, "别人的券")
    await r.sc("list_coupons", [1], setup=coupons, note="active_only 默认")
    await r.sc("list_coupons", [1, False], setup=coupons, note="全部，按 expires_at 升序")
    await r.sc("list_coupons", [2], setup=coupons)
    await r.sc("list_coupons", [99], setup=coupons, note="没有券")
    await r.sc("list_coupons", [1], setup=coupons, now=NOW - 3 * DAYS, note="换个当前时间：之前过期的现在仍活跃")
    await r.sc("list_coupons", [1], kwargs={"active_only": True}, setup=coupons, now=NOW + 10 * DAYS, note="全部过期")

    async def one(db):
        at(NOW - DAYS)
        await db.add_coupon(1, "reset", 5, "有效")                # id 1
        at(NOW - 3 * DAYS)
        await db.add_coupon(1, "reset", 3, "刚好到期")            # id 2：expires = NOW
        at(NOW - 4 * DAYS)
        await db.add_coupon(1, "reset", 1, "早过期")              # id 3
        at(NOW - DAYS)
        await db.add_coupon(2, "reset", 5, "别人")               # id 4
        c = await db.add_coupon(1, "reset", 5, "已用")           # id 5
        await db.use_coupon(c["id"], 1)
    await r.sc("use_coupon", [1, 1], setup=one, note="成功")
    await r.sc("use_coupon", [1, 2], setup=one, note="不是自己的券")
    await r.sc("use_coupon", [2, 1], setup=one, note="expires_at == now：算过期")
    await r.sc("use_coupon", [3, 1], setup=one, note="已过期")
    await r.sc("use_coupon", [5, 1], setup=one, note="已经用过")
    await r.sc("use_coupon", [99, 1], setup=one, note="不存在")
    await r.sc("use_coupon", [4, 2], setup=one, note="另一个 Key 自己用")
    r.save()


# ================================================================ admin_actions
async def area_admin_actions():
    r = Rec("admin_actions")
    await r.sc("add_admin_action", ["后台(1.2.*.*)", "新建 Key", "member-1", "额度 100", True])
    await r.sc("add_admin_action", ["Discord:123", "封禁", "x", "", False])
    await r.sc("add_admin_action", ["系统", "a" * 500, "t" * 300, "详情 " * 300, True], note="不截断")
    await r.sc("add_admin_action", ["", "", "", "", True])

    async def many(db):
        for i in range(40):
            at(NOW - (40 - i) * HOUR)
            await db.add_admin_action(f"actor{i % 3}", f"act{i % 5}", f"t{i}", f"detail {i}", i % 4 != 0)
    await r.sc("list_admin_actions", setup=many, note="默认 30 条，新→旧")
    await r.sc("list_admin_actions", [5, 0], setup=many)
    await r.sc("list_admin_actions", [5, 37], setup=many, note="翻页到末尾")
    await r.sc("list_admin_actions", [10, 100], setup=many, note="offset 超出")
    await r.sc("list_admin_actions", [0], setup=many, note="limit=0 → 夹到 1")
    await r.sc("list_admin_actions", [-4, 2], setup=many, note="负 limit → 1")
    await r.sc("list_admin_actions", [1000], setup=many, note="上限 200（只有 40 条）")
    await r.sc("list_admin_actions", [3, -9], setup=many, note="负 offset → 0")
    await r.sc("list_admin_actions", note="空表")

    async def many_210(db):
        for i in range(210):
            at(NOW - (210 - i) * 60)
            await db.add_admin_action("a", "x", str(i), "", True)
    await r.sc("list_admin_actions", [500], setup=many_210, note="limit 夹到 200")

    await r.sc("count_admin_actions")
    await r.sc("count_admin_actions", setup=many)
    await r.sc("count_admin_actions", setup=many_210, note="超过 200 也如实计数")

    async def purged(db):
        await many(db)
        await db.purge_admin_actions(NOW - 20 * HOUR)
    await r.sc("count_admin_actions", setup=purged, note="清理之后")

    await r.sc("purge_admin_actions", [NOW - 30 * HOUR], setup=many, note="清掉最老的 10 条")
    await r.sc("purge_admin_actions", [NOW - 100 * HOUR], setup=many, note="没有可清")
    await r.sc("purge_admin_actions", [NOW], setup=many, note="全清")
    await r.sc("purge_admin_actions", [NOW])
    r.save()


# ================================================================ discord 黑名单 / 领取记录
async def area_discord():
    r = Rec("discord")

    async def regs(db):
        a, b = await mkkey(db, "a"), await mkkey(db, "b")
        await sql(db, "INSERT INTO discord_registrations(discord_id, key_id, created_at, role_granted, username) "
                      "VALUES (?,?,?,?,?)", ("100000000000000001", a["id"], NOW - DAYS, 1, "alice"))
        await sql(db, "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
                  ("100000000000000002", b["id"], NOW - DAYS))
    await r.sc("forget_registration_for_key", [1], setup=regs, note="返回被删的 Discord ID")
    await r.sc("forget_registration_for_key", [2], setup=regs)
    await r.sc("forget_registration_for_key", [99], setup=regs, note="没有记录 → []")
    await r.sc("forget_registration_for_key", [1], note="空库")

    # ---- ban_discord
    await r.sc("ban_discord", ["100000000000000009"], note="只给 ID：banned_by 默认 '系统'")
    await r.sc("ban_discord", ["100000000000000009"], {"reason": "滥用", "username": "bob", "display_name": "Bob 🦉",
                                                       "avatar": "a1b2c3", "banned_by": "站长"})
    await r.sc("ban_discord", [123456789012345678], {"reason": "int 型 ID 会被 str()"})

    async def banned(db):
        at(NOW - 3 * DAYS)
        await db.ban_discord("100000000000000009", reason="旧理由", username="bob", display_name="Bob",
                             avatar="av1", banned_by="系统")
    await r.sc("ban_discord", ["100000000000000009"], {"reason": "新理由"}, setup=banned,
               note="再次封禁：时间 / 理由 / 操作者刷新，资料为空时保留旧值")
    await r.sc("ban_discord", ["100000000000000009"], {"reason": "x", "username": "bobby", "avatar": "av2",
                                                       "banned_by": "站长"}, setup=banned,
               note="资料非空时覆盖")
    await r.sc("ban_discord", ["100000000000000010"], {"reason": "另一个"}, setup=banned)

    # ---- unban_discord
    await r.sc("unban_discord", ["100000000000000009"], setup=banned)
    await r.sc("unban_discord", ["100000000000000099"], setup=banned, note="不在名单里 → False")
    await r.sc("unban_discord", [100000000000000009], setup=banned, note="int 型 ID 也会 str()")
    await r.sc("unban_discord", ["100000000000000009"])

    # ---- list_bans
    async def many_bans(db):
        for i, (did, reason, uname, dname) in enumerate((
                ("100000000000000001", "刷图", "alice", "Alice"),
                ("100000000000000002", "转卖账号", "bob_the_builder", "Bob 🦉"),
                ("100000000000000003", "分享 Key", "carol", "卡罗尔"),
                ("200000000000000004", "100% 恶意", "dave", "Dave_D"),
                ("200000000000000005", "", "", ""))):
            at(NOW - (5 - i) * DAYS)
            await db.ban_discord(did, reason=reason, username=uname, display_name=dname, avatar=f"av{i}",
                                 banned_by="站长" if i % 2 else "系统")
    await r.sc("list_bans", setup=many_bans, note="全部，新→旧")
    await r.sc("list_bans", [""], setup=many_bans)
    await r.sc("list_bans", ["   "], setup=many_bans, note="纯空白 strip 后为空")
    await r.sc("list_bans", ["ALICE"], setup=many_bans, note="LIKE 对 ASCII 不分大小写")
    await r.sc("list_bans", ["bob"], setup=many_bans, note="命中 username")
    await r.sc("list_bans", [" 卡罗 "], setup=many_bans, note="命中 display_name，且 strip")
    await r.sc("list_bans", ["转卖"], setup=many_bans, note="命中 reason")
    await r.sc("list_bans", ["2000000"], setup=many_bans, note="命中 discord_id 前缀")
    await r.sc("list_bans", ["%"], setup=many_bans, note="通配符不转义：% 匹配全部")
    await r.sc("list_bans", ["_"], setup=many_bans, note="_ 匹配任意单字符")
    await r.sc("list_bans", ["100%"], setup=many_bans, note="'100%' 同时命中 reason 里的字面量与 ID 前缀")
    await r.sc("list_bans", ["不存在的"], setup=many_bans, note="无匹配 → []")
    await r.sc("list_bans", [], note="空库")
    await r.sc("list_bans", [None], setup=many_bans, note="None → (q or '')")
    r.save()


# ================================================================ 问卷 / 捐赠
async def area_feedback():
    r = Rec("feedback")
    kw = dict(client="Cherry Studio", understand="明白", improve="更快", improve_other="无", donate="愿意")
    await r.sc("save_survey", [1], kw)
    await r.sc("save_survey", [1], dict(client="c" * 100, understand="u" * 50, improve="i" * 300,
                                       improve_other="o" * 500, donate="d" * 50), note="各字段按旧版长度截断")
    await r.sc("save_survey", [1], dict(client="客" * 61, understand="懂" * 21, improve="改" * 201,
                                       improve_other="他" * 301, donate="捐" * 21), note="按码点截断")
    await r.sc("save_survey", [1], dict(client="", understand="", improve="", improve_other="", donate=""))

    async def first_survey(db):
        a = await mkkey(db, "a")
        at(NOW - DAYS)
        await db.save_survey(a["id"], **kw)
    await r.sc("save_survey", [1], dict(client="新客户端", understand="一般", improve="稳定", improve_other="",
                                       donate="不愿"), setup=first_survey, now=NOW, note="一人一份：重复提交覆盖")
    await r.sc("save_survey", [2], kw, setup=first_survey, note="另一个 Key 单独一份")

    await r.sc("list_survey", note="空")

    async def surveys(db):
        a, b, c = await mkkey(db, "a"), await mkkey(db, "b"), await mkkey(db, "c")
        await sql(db, "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
                  ("300000000000000001", a["id"], NOW - 9 * DAYS))
        for k, t in ((a, NOW - 3 * DAYS), (b, NOW - DAYS), (c, NOW - 2 * DAYS)):
            at(t)
            await db.save_survey(k["id"], client=f"客户端-{k['name']}", understand="明白", improve="更快",
                                 improve_other="", donate="愿意")
        await db.delete_key(c["id"])           # 已删 Key：name 为 NULL
    await r.sc("list_survey", setup=surveys, note="LEFT JOIN：已删 Key 的 name 为 NULL，discord_id 来自领取记录")

    async def survey_overwritten(db):
        await surveys(db)
        at(NOW - 10)
        await db.save_survey(2, client="覆盖后", understand="x", improve="y", improve_other="z", donate="d")
    await r.sc("list_survey", setup=survey_overwritten, note="覆盖提交后排到最前")

    async def survey_orphan(db):
        at(NOW - HOUR)
        await db.save_survey(77, client="孤儿", understand="a", improve="b", improve_other="c", donate="d")
    await r.sc("list_survey", setup=survey_orphan, note="Key 与领取记录都不存在：name / discord_id 都是 NULL")

    # ---- donations
    await r.sc("save_donation", ["pst-FAKE-TOKEN-1", "想捐一个号", "qq 12345"])
    await r.sc("save_donation", ["pst-FAKE-TOKEN-2", "m" * 600, "c" * 150], note="message 截 500、contact 截 100")
    await r.sc("save_donation", ["t", "留" * 501, "联" * 101], note="按码点截断")
    await r.sc("save_donation", ["", "", ""])

    async def donations(db):
        for i in range(5):
            at(NOW - (5 - i) * HOUR)
            await db.save_donation(f"pst-FAKE-{i}", f"留言 {i}", f"联系 {i}")
        at(NOW - HOUR / 2)
        await db.handle_donation(2, "已接入")
        at(NOW - HOUR / 4)
        await db.handle_donation(4, "重复号")
    await r.sc("save_donation", ["pst-FAKE-NEW", "新的", ""], setup=donations, note="id 递增")
    await r.sc("list_donations", note="空")
    await r.sc("list_donations", setup=donations, note="未处理在前、各自按 ts 倒序")

    async def donations_202(db):
        for i in range(202):
            at(NOW - (202 - i) * 60)
            await db.save_donation(f"pst-FAKE-{i}", f"m{i}", "")
        await sql(db, "UPDATE key_donations SET handled=1, handled_at=?, handled_note='x' WHERE id IN (3,5,7)", (NOW,))
    await r.sc("list_donations", setup=donations_202, note="LIMIT 200：已处理的排在后面被挤出去")

    await r.sc("count_unhandled_donations")
    await r.sc("count_unhandled_donations", setup=donations)
    await r.sc("count_unhandled_donations", setup=donations_202)

    await r.sc("handle_donation", [1, "接入完成"], setup=donations)
    await r.sc("handle_donation", [2, "再来一次"], setup=donations, note="已处理 → False")
    await r.sc("handle_donation", [99, "x"], setup=donations, note="不存在 → False")
    await r.sc("handle_donation", [3, "备" * 250], setup=donations, note="note 截 200")
    await r.sc("handle_donation", [1, ""])
    r.save()


# ================================================================ execute_fetchall_compat
async def area_misc():
    r = Rec("misc")

    async def data(db):
        a = await mkkey(db, "a", expires_at=NOW + DAYS)
        b = await mkkey(db, "b", expires_at=NOW - DAYS)
        c = await mkkey(db, "c")
        for k, did in ((a, "1"), (b, "2"), (c, "3")):
            await sql(db, "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
                      (did, k["id"], NOW - HOUR))
    q = ("SELECT COUNT(*) AS n FROM discord_registrations r JOIN api_keys k ON k.id=r.key_id "
         "WHERE k.expires_at IS NULL OR k.expires_at > ?")
    await r.sc("execute_fetchall_compat", [q, (NOW,)], setup=data, note="registration.count_registered 的真实用法")
    await r.sc("execute_fetchall_compat", [q, (NOW + 2 * DAYS,)], setup=data)
    await r.sc("execute_fetchall_compat", [q, (NOW,)], note="空库")
    await r.sc("execute_fetchall_compat", ["SELECT id, name, enabled, expires_at, last_used_at FROM api_keys ORDER BY id", ()],
               setup=data, note="多列 + NULL + REAL")
    await r.sc("execute_fetchall_compat", ["SELECT id FROM api_keys WHERE name=? AND enabled=? ORDER BY id", ("b", True)],
               setup=data, note="参数：str / bool")
    await r.sc("execute_fetchall_compat", ["SELECT name FROM api_keys WHERE expires_at IS ? ORDER BY id", (None,)],
               setup=data, note="参数：None")
    await r.sc("execute_fetchall_compat", ["SELECT ? AS i, ? AS f, ? AS s, ? AS n", (7, 1.5, "文", None)])
    await r.sc("execute_fetchall_compat", ["SELECT id FROM api_keys WHERE id>?", (99,)], setup=data, note="无结果")
    r.save()


# ================================================================ keys
async def tag(db, key_id, tag="邀请", t=NOW - DAYS):
    await sql(db, "INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)", (key_id, tag, "备注", "站长", t))


async def area_keys():
    r = Rec("keys")
    base = dict(name="m1", token="nai-fixture-token-0001", daily_images=100, monthly_anlas=500.0,
                daily_text_tokens=150000, rpm=10)

    # ---- create_key
    await r.sc("create_key", [base], note="只给必填项：其余取默认")
    await r.sc("create_key", [dict(base, name="全字段", token="nai-fixture-token-0002", enabled=False, daily_anlas=12.5,
                                   daily_v5=7, allow_anlas=True, allow_img2img=True, exclude_global_v5=True,
                                   image_model_scope="all", is_admin=True, expires_at=NOW + 30 * DAYS,
                                   features='["tags","vibe"]', is_test=True, discord_id="400000000000000001")])
    await r.sc("create_key", [dict(base, expires_at=1900000000, daily_anlas=3, monthly_anlas=0)], note="整数 expires_at / daily_anlas")

    async def one_key(db):
        await mkkey(db, "dup", token="nai-fixture-token-0001")
    await r.sc("create_key", [base], setup=one_key, note="token 重复 → IntegrityError")
    await r.sc("create_key", [{"name": "缺字段"}], note="缺必填项 → KeyError")

    async def two_then_delete(db):
        await mkkey(db, "x1")
        b = await mkkey(db, "x2")
        await db.delete_key(b["id"])
    await r.sc("create_key", [dict(base, token="nai-fixture-token-0003")], setup=two_then_delete,
               note="AUTOINCREMENT：删掉最大 id 后新 Key 不复用")

    # ---- get_key / get_key_by_token
    async def three(db):
        await mkkey(db, "a", t=NOW - 3 * DAYS)
        await mkkey(db, "b", t=NOW - 2 * DAYS, expires_at=NOW + DAYS, features='["x"]', is_test=True)
        await mkkey(db, "c", t=NOW - DAYS, discord_id="500000000000000001", exclude_global_v5=True)
    await r.sc("get_key", [1], setup=three)
    await r.sc("get_key", [2], setup=three)
    await r.sc("get_key", [3], setup=three)
    await r.sc("get_key", [99], setup=three, note="不存在 → None")
    await r.sc("get_key", [1])
    await r.sc("get_key_by_token", ["nai-b-0123456789abcdef"], setup=three)
    await r.sc("get_key_by_token", ["NAI-B-0123456789ABCDEF"], setup=three, note="区分大小写")
    await r.sc("get_key_by_token", ["nope"], setup=three)
    await r.sc("get_key_by_token", [""], setup=three)

    # ---- rotate_key_token
    await r.sc("rotate_key_token", [1, "nai-rotated-0001"], setup=three)
    await r.sc("rotate_key_token", [99, "nai-rotated-0002"], setup=three, note="不存在 → False")
    await r.sc("rotate_key_token", [1, "nai-b-0123456789abcdef"], setup=three, note="撞 UNIQUE → IntegrityError")
    await r.sc("rotate_key_token", [2, "nai-b-0123456789abcdef"], setup=three, note="换成自己当前的 token：行数 1")

    # ---- update_key
    await r.sc("update_key", [1, {"name": "renamed"}], setup=three)
    await r.sc("update_key", [2, {"enabled": 0, "allow_anlas": "yes", "allow_img2img": None, "exclude_global_v5": "",
                                  "is_test": 2}], setup=three, note="开关列走 Python 真值")
    await r.sc("update_key", [1, {"id": 99, "token": "hacked", "is_admin": 1, "created_at": 1, "quota_auto": 5}],
               setup=three, note="白名单之外的键全部忽略 → 不写库")
    await r.sc("update_key", [1, {}], setup=three)
    await r.sc("update_key", [2, {"expires_at": None, "v5_pinned": None, "features": None}], setup=three, note="写 NULL")
    await r.sc("update_key", [1, {"daily_images": True, "rpm": False}], setup=three, note="bool 当整数绑定")
    await r.sc("update_key", [1, {"daily_images": 5.0, "monthly_anlas": 7, "daily_anlas": 2.5, "daily_text_tokens": 9.5}],
               setup=three, note="列亲和性：5.0→5，9.5 保持浮点，7→7.0")
    await r.sc("update_key", [1, {"features": ["a"]}], setup=three, note="list 不能绑定 → 旧版抛错")
    await r.sc("update_key", [1, {"name": None}], setup=three, note="NOT NULL → IntegrityError")
    await r.sc("update_key", [99, {"name": "x"}], setup=three, note="不存在的 id：无事发生")
    await r.sc("update_key", [3, {"image_model_scope": "all", "daily_v5": 8, "v5_pinned": 3, "name": "multi"}], setup=three)
    await r.sc("update_key", [1, {"name": "n", "bogus": 1}], setup=three, note="部分忽略")
    await r.sc("update_key", [1, {"rpm": "12", "daily_images": "abc"}], setup=three, note="字符串绑定：能转的转，不能转的原样")
    await r.sc("update_key", [2, {"expires_at": 1900000000}], setup=three)
    await r.sc("update_key", [2, {"expires_at": 1.9e9 + 0.25}], setup=three)
    await r.sc("update_key", [1, {"enabled": True, "is_test": True, "allow_anlas": [] }], setup=three,
               note="空列表在开关列上是假值，不会绑定")

    # ---- delete_key
    async def rich(db):
        a = await mkkey(db, "a", t=NOW - 3 * DAYS, exclude_global_v5=True)
        b = await mkkey(db, "b", t=NOW - 3 * DAYS, is_test=True)
        c = await mkkey(db, "c", t=NOW - 3 * DAYS)
        for k in (a, b, c):
            await tag(db, k["id"])
            await tag(db, k["id"], "其他")
            await sql(db, "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
                      (f"60000000000000000{k['id']}", k["id"], NOW - DAYS))
            at(NOW - HOUR)
            await db.bump_counters(k["id"], DAY, images=2, anlas=1.5, v5=1)
            await db.reset_daily_image_quota(k["id"], DAY)
        await tag(db, 99)
        await sql(db, "INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
                  ("600000000000000099", 99, NOW - DAYS))
    await r.sc("delete_key", [1], setup=rich, note="触发器：归档 exclude_global_v5，清当日额度基线；计数器保留")
    await r.sc("delete_key", [2], setup=rich, note="测试 Key：墓碑记 is_test")
    await r.sc("delete_key", [3], setup=rich)
    await r.sc("delete_key", [99], setup=rich, note="没有这个 Key，但孤儿标签 / 领取记录照删")
    await r.sc("delete_key", [77], setup=rich, note="什么都没有")

    async def deleted_once(db):
        await rich(db)
        await db.delete_key(2)
    await r.sc("delete_key", [2], setup=deleted_once, note="重复删除")

    # ---- inactive_key_ids
    async def inactive_base(db):
        await mkkey(db, "k1_never", t=NOW - 10 * DAYS)                            # 从没生过图，创建于 10 天前
        k2 = await mkkey(db, "k2_active", t=NOW - 10 * DAYS)
        await mkok(db, k2, NOW - DAYS, kind="image")                               # 昨天生过图
        await mkkey(db, "k3_admin", t=NOW - 30 * DAYS, is_admin=True)
        await mkkey(db, "k4_test", t=NOW - 30 * DAYS, is_test=True)
        await mkkey(db, "k5_disabled", t=NOW - 30 * DAYS, enabled=False)
        k6 = await mkkey(db, "k6_invite", t=NOW - 30 * DAYS)
        await tag(db, k6["id"])
        k7 = await mkkey(db, "k7_old_image", t=NOW - 20 * DAYS)
        await mkok(db, k7, NOW - 8 * DAYS, kind="upscale")
        await mkkey(db, "k8_new", t=NOW - DAYS)
        k9 = await mkkey(db, "k9_text_only", t=NOW - 20 * DAYS)
        await mkok(db, k9, NOW - HOUR, kind="text")                              # 只聊天不算活跃
        k10 = await mkkey(db, "k10_other_tag", t=NOW - 30 * DAYS)
        await tag(db, k10["id"], "别的标签")
    cutoff = NOW - 5 * DAYS
    await r.sc("inactive_key_ids", [cutoff], setup=inactive_base, note="没有宽限设置")

    def grace(v):
        async def s(db):
            await inactive_base(db)
            await db.set_setting("key_inactivity_grace_started_at", v)
        return s
    await r.sc("inactive_key_ids", [cutoff], setup=grace(NOW - 7 * DAYS), note="宽限起点 7 天前：只比它更闲的才算")
    await r.sc("inactive_key_ids", [cutoff], setup=grace(NOW - 2 * DAYS), note="宽限起点太新 → 没人闲置")
    await r.sc("inactive_key_ids", [cutoff], setup=grace("abc"), note="设置损坏 → []")
    await r.sc("inactive_key_ids", [cutoff], setup=grace(-1), note="负数 → []")
    await r.sc("inactive_key_ids", [cutoff], setup=grace("nan"), note="nan → []")
    await r.sc("inactive_key_ids", [cutoff], setup=grace("inf"), note="inf → []")
    await r.sc("inactive_key_ids", [cutoff], setup=grace("  123.5  "), note="带空白的合法浮点")
    await r.sc("inactive_key_ids", [cutoff], setup=grace("1_000"), note="下划线数字（Python float 接受）")
    await r.sc("inactive_key_ids", [cutoff], setup=grace(""), note="空串 → float('') 失败 → []")
    await r.sc("inactive_key_ids", [cutoff], setup=grace(0))
    await r.sc("inactive_key_ids", [NOW - 100 * DAYS], setup=inactive_base, note="截止时间很早 → 没人")
    await r.sc("inactive_key_ids", [NOW + DAYS], setup=inactive_base, note="截止时间在未来 → 所有符合条件的")
    await r.sc("inactive_key_ids", [cutoff])

    # ---- list_keys / touch_key
    await r.sc("list_keys")
    await r.sc("list_keys", setup=three, note="id 倒序")

    async def three_one_deleted(db):
        await three(db)
        await db.delete_key(2)
        await db.update_key(3, {"name": "改名", "enabled": False})
    await r.sc("list_keys", setup=three_one_deleted, note="删除 / 修改之后")

    async def with_roles(db):
        await three(db)
        await db.set_key_role(1, "admin")
        await db.set_key_role(3, "test")
    await r.sc("list_keys", setup=with_roles, note="含 admin / test 身份")
    await r.sc("touch_key", [1], setup=three)
    await r.sc("touch_key", [99], setup=three, note="不存在")

    async def touched(db):
        await three(db)
        at(NOW - HOUR)
        await db.touch_key(2)
    await r.sc("touch_key", [2], setup=touched, now=NOW + 0.5, note="再次 touch 覆盖")

    # ---- get_key_role
    async def invite_tagged(db):
        await mkkey(db, "a")
        await tag(db, 1)
    await r.sc("get_key_role", [{"id": 1, "is_admin": 1, "is_test": 1}], setup=invite_tagged, note="admin 优先于 test")
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0, "is_test": 1}], setup=invite_tagged)
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0, "is_test": 0}], setup=invite_tagged, note="查库发现邀请标签")
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0, "is_test": 0}], note="没有标签 → regular")
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0, "is_test": 0}, ["邀请"]], note="给了 tags 就不查库")
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0, "is_test": 0}, []], setup=invite_tagged,
               note="tags=[] 时不查库，即使库里有标签")
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0, "is_test": 0}, ["别的"]], setup=invite_tagged)
    await r.sc("get_key_role", [{"id": 1}], setup=invite_tagged, note="没有 is_admin / is_test 字段")
    await r.sc("get_key_role", [{"id": 1, "is_admin": None, "is_test": None}])
    await r.sc("get_key_role", [{"is_admin": 0, "is_test": 0}], note="tags=None 又没有 id → KeyError")
    await r.sc("get_key_role", [{"is_admin": 0, "is_test": 0}, []], note="给了 tags 就用不着 id")
    await r.sc("get_key_role", [{"id": 1, "is_admin": "0", "is_test": 0}], note="字符串 '0' 在 Python 里是真值")
    await r.sc("get_key_role", [{"id": 1, "is_admin": 0.0, "is_test": 0.5}], note="浮点真值")

    # ---- set_key_role
    async def roles(db):
        a = await mkkey(db, "a")
        b = await mkkey(db, "b", is_admin=True)
        c = await mkkey(db, "c", is_test=True)
        d = await mkkey(db, "d")
        await tag(db, d["id"], t=NOW - 5 * DAYS)
        await tag(db, d["id"], "别的标签")
        await tag(db, a["id"], "别的标签")
    await r.sc("set_key_role", [1, "admin"], setup=roles)
    await r.sc("set_key_role", [4, "admin"], setup=roles, note="邀请 → admin：删邀请标签、保留其他标签")
    await r.sc("set_key_role", [1, "test"], setup=roles)
    await r.sc("set_key_role", [4, "test"], setup=roles)
    await r.sc("set_key_role", [1, "invite"], setup=roles, note="新建邀请标签")
    await r.sc("set_key_role", [2, "invite"], setup=roles, note="admin → invite：清掉 is_admin")
    await r.sc("set_key_role", [4, "invite"], setup=roles, note="已是邀请：标签被覆盖（备注 / 操作者 / 时间刷新）")
    await r.sc("set_key_role", [3, "regular"], setup=roles)
    await r.sc("set_key_role", [4, "regular"], setup=roles)
    await r.sc("set_key_role", [1, "boss"], setup=roles, note="未知身份组 → ValueError，不动库")
    await r.sc("set_key_role", [1, "ADMIN"], setup=roles, note="区分大小写")
    await r.sc("set_key_role", [99, "invite"], setup=roles, note="Key 不存在：UPDATE 空转，但邀请标签仍被写入（旧版行为）")
    await r.sc("set_key_role", [99, "admin"], setup=roles)
    r.save()


# ================================================================ counters / quotas
async def area_counters():
    r = Rec("counters")

    # ---- bump_counters
    await r.sc("bump_counters", [1, DAY], note="全默认：requests=1")
    await r.sc("bump_counters", [1, DAY], dict(images=3, anlas=1.5, text_tokens=100, requests=2, v5=1, legacy_free_images=2))
    await r.sc("bump_counters", [1, DAY, 3, 1.5, 100, 2, 1, 2], note="位置参数")
    await r.sc("bump_counters", [1, DAY], dict(requests=0), note="requests=0")
    await r.sc("bump_counters", [1, DAY], dict(anlas=2), note="整数 anlas → REAL")
    await r.sc("bump_counters", [1, DAY], dict(images=-1, anlas=-1.25, v5=-1), note="负数（退款）")

    async def existing(db):
        at(NOW - HOUR)
        await db.bump_counters(1, DAY, images=2, anlas=0.1, text_tokens=10, requests=1, v5=1, legacy_free_images=1)
        await db.bump_counters(1, "2030-03-14", images=9)
    await r.sc("bump_counters", [1, DAY], dict(images=3, anlas=0.2, text_tokens=5, v5=2, legacy_free_images=1),
               setup=existing, note="累加（0.1+0.2 的浮点结果）")
    await r.sc("bump_counters", [1, "2030-03-16"], dict(images=1), setup=existing, note="新的一天另起一行")
    await r.sc("bump_counters", [2, DAY], dict(images=1), setup=existing, note="另一个 Key")

    # ---- get_counter
    async def cdata(db):
        a = await mkkey(db, "a")
        at(NOW - 2 * HOUR)
        await db.bump_counters(a["id"], DAY, images=4, anlas=3.5, text_tokens=200, requests=5, v5=2, legacy_free_images=1)
        await db.bump_counters(a["id"], "2030-03-14", images=1, anlas=9.0, requests=1, v5=7)
    await r.sc("get_counter", [1, DAY], note="没有记录 → 全 0（anlas 是 0.0）")
    await r.sc("get_counter", [1, DAY], setup=cdata)
    await r.sc("get_counter", [1, "2030-03-14"], setup=cdata)
    await r.sc("get_counter", [1, "2030-03-01"], setup=cdata)
    await r.sc("get_counter", [2, DAY], setup=cdata)

    async def reset_all(db):
        await cdata(db)
        await db.reset_daily_image_quota(1, DAY)
    await r.sc("get_counter", [1, DAY], setup=reset_all, note="重置后：anlas 基线抵消 → 整数 0（max(0, 0.0) 返回 0）")

    async def reset_then_use(db):
        await reset_all(db)
        at(NOW - HOUR)
        await db.bump_counters(1, DAY, anlas=1.25, v5=1, requests=1)
    await r.sc("get_counter", [1, DAY], setup=reset_then_use, note="重置后又用了一点")

    async def refunded(db):
        await reset_all(db)
        at(NOW - HOUR)
        await db.bump_counters(1, DAY, anlas=-2.0, v5=-1, requests=0)
    await r.sc("get_counter", [1, DAY], setup=refunded, note="基线之后退款 → 负数夹到 0")

    async def offset_other_day(db):
        await cdata(db)
        await db.reset_daily_image_quota(1, "2030-03-14")
    await r.sc("get_counter", [1, DAY], setup=offset_other_day, note="别的日子的基线不影响")
    await r.sc("get_counter", [1, "2030-03-14"], setup=offset_other_day)

    async def lone_offset(db):
        await sql(db, "INSERT INTO daily_quota_offsets(key_id, day, anlas, v5) VALUES (1, ?, 5.0, 3)", (DAY,))
    await r.sc("get_counter", [1, DAY], setup=lone_offset, note="只有基线没有计数器行")

    # ---- reset_daily_image_quota
    await r.sc("reset_daily_image_quota", [1, DAY], setup=cdata, note="新建基线 = 当前累计")
    await r.sc("reset_daily_image_quota", [1, DAY], setup=reset_then_use, note="再次重置：基线更新")
    await r.sc("reset_daily_image_quota", [1, "2030-03-10"], setup=cdata, note="那天没有计数器 → 什么都不写")
    await r.sc("reset_daily_image_quota", [9, DAY], setup=cdata, note="Key 没有计数器")

    async def gone(db):
        await cdata(db)
        await db.delete_key(1)
    await r.sc("reset_daily_image_quota", [1, DAY], setup=gone, note="已删除的 Key：触发器 RAISE(IGNORE) 静默跳过")

    # ---- month_anlas
    async def months(db):
        a, b = await mkkey(db, "a"), await mkkey(db, "b")
        at(NOW - HOUR)
        for k, day, an in ((a, "2030-03-01", 1.5), (a, "2030-03-15", 2.25), (a, "2030-03-31", 0.1), (a, "2030-02-28", 9.0),
                           (a, "2030-04-01", 7.0), (b, "2030-03-10", 4.0), (a, "2030-03-11", 0.2)):
            await db.bump_counters(k["id"], day, anlas=an)
        await db.bump_counters(a["id"], "2030-03-20", anlas=0.2)
    await r.sc("month_anlas", [1, "2030-03"], setup=months, note="0.1+0.2 一类的浮点求和")
    await r.sc("month_anlas", [1, "2030-02"], setup=months)
    await r.sc("month_anlas", [2, "2030-03"], setup=months)
    await r.sc("month_anlas", [1, "2031-01"], setup=months, note="没有数据 → 0.0")
    await r.sc("month_anlas", [3, "2030-03"], setup=months)
    await r.sc("month_anlas", [1, "2030-3"], setup=months, note="格式不对 → 匹配不到")

    # ---- day_v5_total
    async def v5data(db):
        a = await mkkey(db, "a")
        b = await mkkey(db, "b", exclude_global_v5=True)
        c = await mkkey(db, "c")
        d = await mkkey(db, "d", exclude_global_v5=True)
        at(NOW - HOUR)
        for k, v5 in ((a, 3), (b, 5), (c, 2), (d, 4)):
            await db.bump_counters(k["id"], DAY, v5=v5, images=1)
        await db.bump_counters(77, DAY, v5=6)                    # 从未存在过的 Key：默认视为独立额度（排除）
        await db.bump_counters(a["id"], "2030-03-14", v5=100)
        await db.delete_key(c["id"])                             # 墓碑：exclude_global_v5=0 → 仍计入
        await db.delete_key(d["id"])                             # 墓碑：exclude_global_v5=1 → 仍排除
    await r.sc("day_v5_total", [DAY], setup=v5data)
    await r.sc("day_v5_total", ["2030-03-14"], setup=v5data)
    await r.sc("day_v5_total", ["2030-03-13"], setup=v5data, note="没有数据 → 0")
    await r.sc("day_v5_total", [DAY])

    # ---- day_anlas_total / month_anlas_all
    await r.sc("day_anlas_total", [DAY], setup=months)
    await r.sc("day_anlas_total", ["2030-03-15"], setup=months)
    await r.sc("day_anlas_total", ["2030-03-12"], setup=months, note="没有数据 → 0.0")
    await r.sc("day_anlas_total", [DAY])
    await r.sc("month_anlas_all", ["2030-03"], setup=months)
    await r.sc("month_anlas_all", ["2030-04"], setup=months)
    await r.sc("month_anlas_all", ["2029-12"], setup=months, note="没有数据 → 0.0")
    await r.sc("month_anlas_all", ["2030-03"])

    # ---- member_usage
    async def usage(db):
        a, b, c = await mkkey(db, "a"), await mkkey(db, "b"), await mkkey(db, "c")
        at(NOW - HOUR)
        for k, day, imgs, v5, an, tok, req in (
                (b, "2030-03-15", 3, 1, 1.5, 100, 4), (a, "2030-03-15", 2, 0, 0.5, 50, 2),
                (a, "2030-03-14", 5, 2, 2.25, 0, 5), (a, "2030-03-10", 7, 3, 0.1, 10, 7),
                (b, "2030-03-10", 1, 0, 0.2, 5, 1), (c, "2030-02-01", 9, 9, 9.0, 9, 9)):
            await db.bump_counters(k["id"], day, images=imgs, v5=v5, anlas=an, text_tokens=tok, requests=req)
        await db.bump_counters(99, "2030-03-15", images=1)
    await r.sc("member_usage", ["2030-03-01"], setup=usage)
    await r.sc("member_usage", ["2030-03-15"], setup=usage, note="since 包含当天（字符串比较 >=）")
    await r.sc("member_usage", ["2030-03-11"], setup=usage)
    await r.sc("member_usage", ["2030-12-31"], setup=usage, note="无数据 → 空 dict")
    await r.sc("member_usage", ["2000-01-01"], setup=usage, note="全部，含已不存在的 Key")
    await r.sc("member_usage", ["2030-03-01"])

    # ---- overview
    async def ov(db):
        adm = await mkkey(db, "admin", t=NOW - 40 * DAYS, is_admin=True)
        tst = await mkkey(db, "tester", t=NOW - 40 * DAYS, is_test=True)
        a = await mkkey(db, "a", t=NOW - 30 * DAYS)
        b = await mkkey(db, "b", t=NOW - 30 * DAYS, enabled=False)
        c = await mkkey(db, "c", t=NOW - 30 * DAYS, expires_at=NOW - DAYS)
        d = await mkkey(db, "d", t=NOW - 30 * DAYS, expires_at=NOW + DAYS, exclude_global_v5=True)
        e = await mkkey(db, "e", t=NOW - 30 * DAYS)
        at(NOW - HOUR)
        rows = (
            (adm, DAY, 1, 0.5, 10, 1, 0), (tst, DAY, 2, 0.25, 20, 2, 1), (a, DAY, 4, 3.14159, 300, 6, 2),
            (b, DAY, 0, 0.0, 0, 0, 0), (d, DAY, 3, 2.0, 0, 3, 5), (e, DAY, 1, 0.1, 5, 1, 1),
            (a, "2030-03-14", 6, 1.2, 100, 8, 3), (d, "2030-03-14", 1, 0.0, 0, 1, 2),
            (a, "2030-03-10", 2, 0.3, 0, 2, 1), (c, "2030-03-10", 5, 0.7, 0, 5, 0),
            (a, "2030-03-05", 9, 5.5, 0, 9, 4), (a, "2030-03-01", 1, 1.0, 0, 1, 0),
            (a, "2030-02-20", 3, 7.5, 0, 3, 1), (a, "2029-12-31", 1, 4.0, 0, 1, 0), (a, "2030-01-01", 1, 8.0, 0, 1, 0))
        for k, day, imgs, an, tok, req, v5 in rows:
            await db.bump_counters(k["id"], day, images=imgs, anlas=an, text_tokens=tok, requests=req, v5=v5)
        await db.bump_counters(e["id"], "2030-03-14", images=2, anlas=1.0, requests=2, v5=2)
        await db.delete_key(e["id"])                    # 已删除 Key 的当日 / 昨日计数仍在汇总里
        # 待核对金额：覆盖「今天 / 本月 / 边界」
        for t, un in ((T(2030, 3, 15, 8), 2.675), (T(2030, 3, 15, 9), 0.1), (T(2030, 3, 15, 10), 0.2),
                      (T(2030, 3, 15, 0, 0, 0), 1.0), (T(2030, 3, 16, 0, 0, 0), 5.0), (T(2030, 3, 3, 12), 0.125),
                      (T(2030, 3, 1, 0, 0, 0), 0.125), (T(2030, 2, 28, 23, 59, 59), 9.0), (T(2030, 3, 15, 11), 0.0),
                      (T(2030, 3, 15, 6, 59, 59), 0.5), (T(2030, 3, 15, 7, 0, 0), 0.25),
                      (T(2030, 3, 14, 23, 59, 59), 0.75), (T(2030, 3, 14, 7, 0, 0), 0.0625)):
            await mklog(db, a, t, status="ok", unconfirmed_anlas=un)
    wk = ["2030-03-09", "2030-03-10", "2030-03-11", "2030-03-12", "2030-03-13", "2030-03-14", "2030-03-15"]
    await r.sc("overview", [DAY, wk], setup=ov)
    await r.sc("overview", [DAY, wk], setup=ov, now=NOW + 2 * DAYS, note="keys_active 按 now：过期的不算")
    await r.sc("overview", [DAY, wk], note="空库")
    await r.sc("overview", [DAY, []], setup=ov, note="week_days 为空 → IN ()")
    await r.sc("overview", [DAY, ["2030-03-15", "2030-03-15", "2030-03-14"]], setup=ov, note="重复的日期")
    await r.sc("overview", [DAY, ["2030-03-01", "2030-02-20", "2029-12-31", "2030-01-01"]], setup=ov, note="跨月")
    await r.sc("overview", ["2030-03-01", ["2030-03-01"]], setup=ov, note="月初：month 窗 == today 窗")
    await r.sc("overview", ["2030-03-31", wk], setup=ov, note="月末：次日是下月 1 号")
    await r.sc("overview", ["2030-03-14", wk], setup=ov, note="今天不是 now 所在的天")
    await r.sc("overview", ["2030-02-28", wk], setup=ov, note="非闰年 2 月末")
    await r.sc("overview", ["2030-12-31", ["2030-12-31"]], setup=ov, note="年末")
    await r.sc("overview", ["2030-01-01", ["2029-12-31", "2030-01-01"]], setup=ov, note="年初：月窗从 1/1 起")
    r.save()


# ================================================================ usage_log
SUCCESS_KW = dict(images=2, anlas=3.5, tokens=0, v5=1, legacy_free_images=1, detail="ok detail",
                  unconfirmed_anlas=0.5, wait_ms=120, dur_ms=4500, client="Cherry/1.0", up_status=200,
                  rid="req123456789abcdef", src="1.2.*.*")


async def area_usage():
    r = Rec("usage")
    MODEL = "nai-diffusion-4-5-full"

    async def one_key(db):
        await mkkey(db, "a", t=NOW - 3 * DAYS)

    # ---- record_success
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], SUCCESS_KW, setup=one_key, note="生图：同时刷新 last_image_at")
    await r.sc("record_success", [1, "a", "text", "glm-4", DAY], dict(tokens=300), setup=one_key,
               note="文本：只刷新 last_used_at，last_image_at 保持 NULL")
    for kind in ("image_stream", "upscale", "augment-image", "vibe_encode", "imagex", "image"):
        await r.sc("record_success", [1, "a", kind, MODEL, DAY], dict(images=1), setup=one_key, note=f"kind={kind}：算生图")
    for kind in ("tags", "chat", "voice", "Image", "", "augment_image", "vibe"):
        await r.sc("record_success", [1, "a", kind, MODEL, DAY], dict(images=1), setup=one_key, note=f"kind={kind!r}：不算生图")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], setup=one_key, note="全默认")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(
        detail="详" * 600, client="客" * 300, rid="r" * 20, src="s" * 50), setup=one_key, note="按码点截断：500 / 256 / 16 / 40")
    await r.sc("record_success", [1, "k" * 100, "image", "m" * 100, DAY], setup=one_key, note="key_name / model 不截断")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(wait_ms=-5, dur_ms=-1, up_status=0), setup=one_key,
               note="负耗时夹到 0")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(wait_ms=12.9, dur_ms=7.1), setup=one_key,
               note="浮点耗时被 int() 截断")

    async def earlier_image(db):
        await one_key(db)
        await mkok(db, {"id": 1, "name": "a"}, NOW - 2 * DAYS, kind="image", images=1, anlas=1.0, v5=1, tokens=0)
    await r.sc("record_success", [1, "a", "text", "glm-4", DAY], dict(tokens=10), setup=earlier_image,
               note="之前生过图：文本请求不清掉 last_image_at，计数器累加")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(images=1, anlas=0.1, v5=1), setup=earlier_image,
               now=NOW + 0.5, note="再生一张：last_image_at 前移，计数累加")
    await r.sc("record_success", [1, "a", "image", MODEL, "2030-03-14"], dict(images=1), setup=earlier_image,
               note="另一天另起计数器行")
    await r.sc("record_success", [99, "ghost", "image", MODEL, DAY], dict(images=1, anlas=2.0), setup=one_key,
               note="Key 已被删除：日志与计数器照写（事后结算），api_keys 无可更新")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(images=1), setup=one_key, rule_version="3.0.0-golden",
               note="RULE_VERSION 写进 ver 列")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(images=1, anlas=0.1 + 0.2, unconfirmed_anlas=0.1 + 0.2),
               setup=earlier_image, note="浮点精度")
    await r.sc("record_success", [1, "a", "image", MODEL, DAY], dict(images=0, tokens=5, v5=0, legacy_free_images=0,
                                                                     unconfirmed_anlas=0), setup=one_key)

    # ---- add_log
    await r.sc("add_log", [1, "a", "image", MODEL, "rejected"], dict(detail="429 排队的人太多了", reason="queue_full"))
    await r.sc("add_log", [None, "", "image", MODEL, "rejected"], dict(detail="401 无效 Key", reason="key_rate"), note="key_id=NULL")
    await r.sc("add_log", [1, "a", "image", MODEL, "error"], dict(
        images=0, anlas=0.0, tokens=0, detail="502", unconfirmed_anlas=1.5, wait_ms=100, dur_ms=2000, client="UA",
        up_status=502, rid="rid-1", src="1.2.*.*", reason="x"))
    await r.sc("add_log", [1, "a", "image", MODEL, "error", 1, 2.5, 3], dict(detail="位置参数"))
    await r.sc("add_log", [1, "k" * 100, "image", "m" * 100, "rejected"], dict(
        detail="详" * 600, client="客" * 300, rid="r" * 20, src="s" * 50, reason="原" * 30), note="按码点截断：80 / 80 / 500 / 256 / 16 / 40 / 24")
    await r.sc("add_log", [1, "a", "image", MODEL, "rejected"], dict(wait_ms=-1, dur_ms=-9))
    await r.sc("add_log", [1, "a", "image", MODEL, "rejected"], dict(wait_ms=3.9, dur_ms=0.5, up_status=429.0), note="int() 截断")
    await r.sc("add_log", [1, "a", "image", MODEL, "rejected"], dict(reason="queue_full"), rule_version="3.0.0-golden")
    await r.sc("add_log", [1, "a", "image", MODEL, "ok"], dict(images=1, anlas=2.0), note="status 不限于 rejected")
    await r.sc("add_log", [1, "a", "image", MODEL, "rejected"], setup=one_key)

    # ---- list_logs / count_logs
    async def logs_base(db):
        a = await mkkey(db, "a")
        tst = await mkkey(db, "tester", is_test=True)
        gone = await mkkey(db, "gone", is_test=True)
        n = await mkkey(db, "n")
        rows = (
            (a, "image", "ok", dict(images=1, anlas=2.0, wait_ms=100, dur_ms=4000, up_status=200, rid="rid-aaa", client="UA-A", detail="ok")),
            (a, "image_stream", "ok", dict(images=2, wait_ms=50, dur_ms=8000, up_status=200, rid="rid-bbb")),
            (a, "image", "error", dict(wait_ms=10, up_status=502, rid="rid-ccc", detail="502 上游错误")),
            (a, "image", "rejected", dict(reason="queue_full", detail="429 排队的人太多了（请稍后）", rid="rid-ddd")),
            (a, "text", "ok", dict(tokens=300, dur_ms=2000)),
            (tst, "image", "ok", dict(images=1, dur_ms=3000, wait_ms=5, rid="rid-aaa")),
            (gone, "image", "ok", dict(images=1, dur_ms=3500)),
            (None, "image", "rejected", dict(reason="key_rate", detail="401 无效 Key")),
            (n, "upscale", "ok", dict(images=1, dur_ms=1000)),
            (n, "image", "ok", dict(images=3, dur_ms=5000, wait_ms=0, client="UA-N")),
            (n, "image", "rejected", dict(reason="member_quota", detail="403 额度已用完")),
            (a, "image", "ok", dict(images=1, dur_ms=4200, wait_ms=20, rid="rid-eee")),
        )
        for i, (k, kind, status, kw) in enumerate(rows):
            await mklog(db, k, NOW - (len(rows) - i) * 600, kind=kind, status=status, **kw)
        await db.delete_key(gone["id"])
    await r.sc("list_logs", setup=logs_base, note="默认 limit=20，新→旧")
    await r.sc("list_logs", [5, 2], setup=logs_base)
    await r.sc("list_logs", [0], setup=logs_base, note="limit=0 → 1")
    await r.sc("list_logs", [-3, -3], setup=logs_base, note="负数 → limit 1、offset 0")
    await r.sc("list_logs", [1000], setup=logs_base, note="limit 夹到 500")
    await r.sc("list_logs", [3, 50], setup=logs_base, note="offset 超出")
    await r.sc("list_logs", [20, 0, 1], setup=logs_base, note="key_id=1")
    await r.sc("list_logs", [20, 0, 0], setup=logs_base, note="key_id=0 等于不筛（if key_id:）")
    await r.sc("list_logs", [20, 0, 99], setup=logs_base, note="不存在的 Key")
    await r.sc("list_logs", [20, 0, None, ["image"]], setup=logs_base, note="kinds 单个")
    await r.sc("list_logs", [20, 0, None, ["image", "upscale"]], setup=logs_base)
    await r.sc("list_logs", [20, 0, None, []], setup=logs_base, note="kinds=[] → 条件 0，什么都不返回")
    await r.sc("list_logs", [20, 0, None, ["nope"]], setup=logs_base)
    await r.sc("list_logs", [20, 0], dict(hide_test=True), setup=logs_base, note="隐藏测试 Key（含已删除的测试 Key），NULL key_id 不隐藏")
    await r.sc("list_logs", [20, 0], dict(hide_test=True, key_id=1, kinds=["image"]), setup=logs_base, note="组合筛选")
    await r.sc("list_logs", [20, 0], dict(rid="rid-aaa"), setup=logs_base, note="按 rid（两条同 rid）")
    await r.sc("list_logs", [20, 0], dict(rid="rid-aaa", hide_test=True), setup=logs_base)
    await r.sc("list_logs", [20, 0], dict(rid="rid-aaa", kinds=["text"]), setup=logs_base)
    await r.sc("list_logs", [20, 0], dict(rid="nope"), setup=logs_base)
    await r.sc("list_logs", [20, 0], dict(rid=""), setup=logs_base, note="空 rid 不筛")
    await r.sc("list_logs", note="空库")

    await r.sc("count_logs", setup=logs_base)
    await r.sc("count_logs", [1], setup=logs_base)
    await r.sc("count_logs", [0], setup=logs_base)
    await r.sc("count_logs", [None, ["image"]], setup=logs_base)
    await r.sc("count_logs", [None, []], setup=logs_base)
    await r.sc("count_logs", [None, None, True], setup=logs_base)
    await r.sc("count_logs", [None, None, False, "rid-aaa"], setup=logs_base)
    await r.sc("count_logs", [2, ["image"], True, "rid-aaa"], setup=logs_base)
    await r.sc("count_logs", [99], setup=logs_base)
    await r.sc("count_logs")

    # ---- purge_usage_log
    await r.sc("purge_usage_log", [NOW - 3600], setup=logs_base, note="清掉一部分")
    await r.sc("purge_usage_log", [NOW - 100 * DAYS], setup=logs_base, note="没有可清")
    await r.sc("purge_usage_log", [NOW], setup=logs_base, note="全清，计数器不受影响")
    await r.sc("purge_usage_log", [NOW])

    # ---- member_milestones
    S = NOW - DAYS

    async def ms_base(db):
        a, b, c, d, e = [await mkkey(db, n) for n in "abcde"]
        # a：有成功出图 + 一堆被拒原因
        await mkok(db, a, NOW - 10 * DAYS, kind="image", images=1)                 # 首次
        await mkok(db, a, NOW - 2 * DAYS, kind="image_stream", images=1)
        await mkok(db, a, NOW - HOUR, kind="image", images=1)                      # 最近
        await mkok(db, a, NOW - 60, kind="text", tokens=5)                         # 文本不算
        await mkok(db, a, NOW - 30, kind="upscale", images=1)                      # 放大不算
        for i in range(5):
            await mklog(db, a, NOW - 3000 + i, kind="image", status="rejected", reason="queue_full", detail="429 排队的人太多了")
        for i in range(2):
            await mklog(db, a, NOW - 2000 + i, kind="image", status="rejected", reason="member_quota", detail="403 额度已用完")
        for i in range(2):
            await mklog(db, a, NOW - 1900 + i, kind="image", status="rejected", reason="key_rate", detail="429 太快")
        await mklog(db, a, NOW - 1000, kind="image", status="rejected", reason="", detail="429 请求过于频繁（请稍后再试）")
        await mklog(db, a, NOW - 900, kind="image", status="rejected", reason="", detail="429 余额不足，请充值")
        await mklog(db, a, NOW - 800, kind="image", status="rejected", reason="", detail="402 余额不足，请充值")
        await mklog(db, a, NOW - 3 * DAYS, kind="image", status="rejected", reason="queue_full", detail="太早，不计")
        # b：只有被拒（没有成功出图）
        await mklog(db, b, NOW - 500, kind="image", status="rejected", reason="", detail="400 这是一个非常非常非常长的错误说明文字超过十八个字符")
        await mklog(db, b, NOW - 400, kind="image", status="rejected", reason="", detail="")
        await mklog(db, b, NOW - 300, kind="image", status="rejected", reason="breaker", detail="503 全站熔断")
        await mklog(db, b, NOW - 200, kind="image", status="rejected", reason="breaker", detail="503 全站熔断")
        # c：成功出图都早于 since，被拒也早于 since
        await mkok(db, c, NOW - 5 * DAYS, kind="image", images=1)
        await mklog(db, c, NOW - 4 * DAYS, kind="image", status="rejected", reason="paced", detail="x")
        # d：只有上游失败（status=error），再加一条 since 整点的被拒
        await mklog(db, d, NOW - 100, kind="image", status="error", detail="502")
        await mklog(db, d, S, kind="image", status="rejected", reason="cooldown", detail="429")
        # e：只有文本；NULL key 的被拒不计
        await mkok(db, e, NOW - 100, kind="text", tokens=1)
        await mklog(db, None, NOW - 100, kind="image", status="rejected", reason="queue_full", detail="无 Key")
    await r.sc("member_milestones", [S], setup=ms_base, note="首次 / 最近 / 24h 被拒 / 前 3 个被拒原因（同次数保持首次出现的顺序）")
    await r.sc("member_milestones", [NOW - 10 * DAYS], setup=ms_base, note="窗口很大")
    await r.sc("member_milestones", [NOW + DAYS], setup=ms_base, note="窗口在未来：只有首次 / 最近")
    await r.sc("member_milestones", [S + 1], setup=ms_base, note="since 刚好排除 d 的整点被拒")
    await r.sc("member_milestones", [S])
    # reason_label 的几个角落：括号 / 逗号 / 纯空白 / 开头不是状态码
    async def labels(db):
        a = await mkkey(db, "a")
        for i, (code, detail) in enumerate((("", "(只有括号) 后面的字"), ("", "  400   前导空白与多空格，后面"), ("", "未知错误: 详情"),
                                            ("", "12 两位数不是状态码"), ("", "429"), ("", "   "), ("unknown_code", "429 未知代码走 detail"),
                                            ("", "A" * 30), ("", "中" * 19), ("", "1234 四位数字"), ("", "500，逗号开头"))):
            await mklog(db, a, NOW - 100 + i, kind="image", status="rejected", reason=code, detail=detail)
    await r.sc("member_milestones", [S], setup=labels, note="reason_label 的各种 detail 形态")

    # ---- latest_member_clients
    async def clients(db):
        a, b, c = await mkkey(db, "a"), await mkkey(db, "b"), await mkkey(db, "c")
        await mklog(db, a, NOW - 300, client="UA-1")
        await mklog(db, a, NOW - 100, client="")                         # 空 client 不算
        await mklog(db, a, NOW - 200, client="UA-2")                      # 插入晚、时间更早：按 id 最大取
        await mklog(db, b, NOW - 100, client="")
        await mklog(db, c, NOW - 50, client="UA-C", kind="text")
        await mklog(db, None, NOW - 10, client="UA-NULL")                 # 没有 Key 不算
        await mklog(db, c, NOW - 400, client="UA-C-OLD")
    await r.sc("latest_member_clients", setup=clients)

    async def only_empty(db):
        a = await mkkey(db, "a")
        await mklog(db, a, NOW - 100, client="")
        await mklog(db, None, NOW - 50, client="UA-NULL")
    await r.sc("latest_member_clients", setup=only_empty, note="只有空 client / 无 Key → 空 dict")

    async def after_purge(db):
        await clients(db)
        at(NOW)
        await db.delete_key(1)
        await db.purge_usage_log(NOW - 250)
    await r.sc("latest_member_clients", setup=after_purge, note="Key 已删除、日志部分清理后")
    await r.sc("latest_member_clients")

    # ---- image_perf_rows / shadow_rows / image_stability / usage_by_kind / generated_image_totals
    since = NOW - 6 * HOUR

    async def perf_base(db):
        a = await mkkey(db, "a")
        tst = await mkkey(db, "tester", is_test=True)
        gone = await mkkey(db, "gone", is_test=True)
        n = await mkkey(db, "n")
        rows = (
            (n, since + 500, "image", "ok", dict(images=1, wait_ms=100, dur_ms=4000, up_status=200, detail="ok")),
            (a, since + 100, "image", "ok", dict(images=2, wait_ms=0, dur_ms=8000, up_status=200, anlas=1.5)),
            (a, since + 300, "image_stream", "error", dict(wait_ms=30, dur_ms=0, up_status=502, detail="502 上游")),
            (a, since + 200, "image", "rejected", dict(reason="queue_full", detail="排队")),
            (a, since + 250, "text", "ok", dict(tokens=10)),
            (tst, since + 400, "image", "ok", dict(images=1, dur_ms=1000)),
            (gone, since + 450, "image", "error", dict(dur_ms=0, up_status=500)),
            (None, since + 600, "image", "ok", dict(images=1, dur_ms=2500, wait_ms=7)),
            (n, since, "image", "ok", dict(images=1, dur_ms=3000)),                    # 整点：包含
            (n, since - 1, "image", "ok", dict(images=1, dur_ms=3000)),                # 早 1 秒：排除
            (a, since + 700, "upscale", "ok", dict(images=1)),
            (a, since + 50, "image", "ok", dict(images=1, dur_ms=6000, wait_ms=2000, up_status=200)),
        )
        for k, t, kind, status, kw in rows:
            await mklog(db, k, t, kind=kind, status=status, **kw)
        await db.delete_key(gone["id"])
    await r.sc("image_perf_rows", [since], setup=perf_base, note="ts 升序，不含测试 Key / 被拒 / 文本")
    await r.sc("image_perf_rows", [since + 450], setup=perf_base)
    await r.sc("image_perf_rows", [NOW], setup=perf_base, note="没有")
    await r.sc("image_perf_rows", [since])
    await r.sc("shadow_rows", [since], setup=perf_base, note="含 key_id=NULL 的行")
    await r.sc("shadow_rows", [since + 450], setup=perf_base)
    await r.sc("shadow_rows", [NOW], setup=perf_base)
    await r.sc("image_stability", [since], setup=perf_base)
    await r.sc("image_stability", [since + 1000], setup=perf_base, note="没有 → 0 / 0")
    await r.sc("image_stability", [since])

    async def only_errors(db):
        a = await mkkey(db, "a")
        await mklog(db, a, NOW - 100, status="error")
        await mklog(db, a, NOW - 90, status="error")
    await r.sc("image_stability", [NOW - HOUR], setup=only_errors)

    await r.sc("usage_by_kind", [since], setup=perf_base, note="按 kind / status 汇总；users 是去重 Key 数（NULL 不算）")
    await r.sc("usage_by_kind", [since + 450], setup=perf_base)
    await r.sc("usage_by_kind", [NOW], setup=perf_base)
    await r.sc("usage_by_kind", [since])

    await r.sc("generated_image_totals", setup=perf_base, note="成功的 image / image_stream 张数，不含 NULL Key")
    await r.sc("generated_image_totals", [1], setup=perf_base)
    await r.sc("generated_image_totals", [2], setup=perf_base, note="测试 Key 也统计（只是没有成员筛选）")
    await r.sc("generated_image_totals", [0], setup=perf_base, note="key_id=0 是「有值」：按 key_id=0 查，什么都没有")
    await r.sc("generated_image_totals", [99], setup=perf_base)
    await r.sc("generated_image_totals")

    # ---- user_image_timing
    async def timing(db):
        a, b, c, d = [await mkkey(db, n) for n in "abcd"]
        for i, (dur, wait) in enumerate(((1000, 100), (2000, 200), (3000, 300), (4000, 400), (5000, 500))):
            await mklog(db, a, NOW - HOUR * (i + 1), status="ok", dur_ms=dur, wait_ms=wait)
        await mklog(db, a, NOW - 2 * DAYS, status="ok", dur_ms=9000, wait_ms=900)        # 24h 窗之外
        await mklog(db, a, NOW - HOUR, status="ok", dur_ms=0, wait_ms=50)                # dur=0 不算
        await mklog(db, a, NOW - HOUR, status="error", dur_ms=7000, wait_ms=1)           # 失败不算
        await mklog(db, a, NOW - HOUR, status="ok", dur_ms=7000, wait_ms=1, kind="text")  # 文本不算
        # b：24h 内只有 2 条，更早还有 3 条 → 退回全部历史
        for i, (dur, wait) in enumerate(((2000, 0), (3000, 100))):
            await mklog(db, b, NOW - HOUR * (i + 1), status="ok", dur_ms=dur, wait_ms=wait, kind="image_stream")
        for i, (dur, wait) in enumerate(((10000, 1000), (11000, 2000), (12000, 3000))):
            await mklog(db, b, NOW - 3 * DAYS - i * HOUR, status="ok", dur_ms=dur, wait_ms=wait)
        # c：只有一条很久以前的
        await mklog(db, c, NOW - 5 * DAYS, status="ok", dur_ms=1250, wait_ms=0)
        # d：24h 内 3 条，wait 全为 0；耗时的平均值需要舍入
        for i, dur in enumerate((1234, 2345, 3456)):
            await mklog(db, d, NOW - 100 * (i + 1), status="ok", dur_ms=dur, wait_ms=0)
    for kid in (1, 2, 3, 4, 5):
        await r.sc("user_image_timing", [kid], setup=timing, note={1: "24h 内 5 条", 2: "24h 内不足 3 条 → 全部历史",
                                                                    3: "只有很早的一条", 4: "wait 全 0 → None；舍入",
                                                                    5: "没有数据 → count 0，站点均值仍有"}[kid])
    await r.sc("user_image_timing", [1], note="空库：站点均值 9000ms 兜底")
    await r.sc("user_image_timing", [1], setup=timing, now=NOW + 10 * DAYS, note="全部在 24h 之外：站点均值也兜底")
    await r.sc("user_image_timing", [3], setup=timing, now=NOW - 4 * DAYS)

    async def exact3(db):
        a = await mkkey(db, "a")
        for i, dur in enumerate((1250, 2250, 3250)):
            await mklog(db, a, NOW - 100 * (i + 1), status="ok", dur_ms=dur, wait_ms=250 * (i + 1))
    await r.sc("user_image_timing", [1], setup=exact3, note="恰好 3 条（不回退）；x.x5 的银行家舍入")
    r.save()


# ================================================================ generation_audit（含原图文件）
async def aud(db, k, t, *, image=None, image_type="", thumb=THUMB, prompt="prompt", negative="neg", kind="image",
              model="m", status="ok", extra=""):
    at(t)
    await db.add_audit(k["id"] if k else None, k["name"] if k else "", kind, model, status, prompt, negative, thumb,
                       extra, image, image_type)


async def blob(db, k, t, image=PNG, image_type="image/png", thumb=THUMB, prompt="old-style"):
    """10-11 之前的老记录：原图直接在 image BLOB 里。"""
    await sql(db, "INSERT INTO generation_audit(ts,key_id,key_name,kind,model,status,prompt,negative,extra,thumb,"
                  "image,image_type) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
              (t, k["id"] if k else None, k["name"] if k else "", "image", "m", "ok", prompt, "neg", "", thumb, image,
               image_type))


async def file_row(db, k, t, rel, image_type="image/png", size=None, prompt="file-row"):
    """直接插一条指向 image_file 的记录（文件由调用方另行处理）。"""
    await sql(db, "INSERT INTO generation_audit(ts,key_id,key_name,kind,model,status,prompt,negative,extra,thumb,"
                  "image,image_type,image_file,image_size) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
              (t, k["id"], k["name"], "image", "m", "ok", prompt, "neg", "", THUMB, None, image_type, rel, size))


def put(db, rel, data=b"x", mtime=None, extra_seed=False):
    p = db.image_dir / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    if mtime is not None:
        os.utime(p, (mtime, mtime))
    if extra_seed:
        db._extra_seed = getattr(db, "_extra_seed", []) + [rel]


def touch_dir(db, rel, mtime):
    os.utime(db.image_dir / rel, (mtime, mtime))


async def area_audit():
    r = Rec("audit")

    async def two_keys(db):
        await mkkey(db, "a")
        await mkkey(db, "b")

    async def mixed(db):
        await two_keys(db)
        a, b = {"id": 1, "name": "a"}, {"id": 2, "name": "b"}
        await blob(db, a, NOW - 6 * DAYS)                                              # id1 BLOB
        await aud(db, a, NOW - 5 * DAYS, image=PNG, image_type="image/png", prompt="p2")   # id2 文件 png
        await aud(db, a, NOW - 4 * DAYS, prompt="p3 只有缩略图")                          # id3 无原图
        await aud(db, a, NOW - 3 * DAYS, image=WEBP, image_type="image/webp", prompt="p4")  # id4 文件 webp
        await blob(db, b, NOW - 2 * DAYS, image=JPG, image_type="image/jpeg", thumb=None, prompt="p5")  # id5 BLOB 别人
        await aud(db, a, NOW - DAYS, image=JPG, image_type="image/jpeg", prompt="p6")      # id6 文件 jpg
        await aud(db, b, NOW - HOUR, thumb=None, prompt="p7 无缩略图无原图")               # id7 什么都没有

    # ---- add_audit
    A = [1, "a", "image", "nai-diffusion-4-5-full", "ok", "a prompt", "bad hands"]
    await r.sc("add_audit", A + [THUMB], note="只有缩略图")
    await r.sc("add_audit", A + [THUMB, "extra json"], note="带 extra")
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], note="PNG 原图落文件")
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], now=T(2030, 3, 16, 0, 30),
               note="本地 00:30（UTC 还是前一天）：日期目录按本地时区")
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], now=T(2030, 3, 15, 23, 59, 59))
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], now=T(2030, 3, 15, 6, 59, 59),
               note="日期目录按自然日，不按每天 7 点重置")
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], now=T(2030, 3, 15, 7, 0, 0))
    await r.sc("add_audit", A + [THUMB, "", WEBP, "image/webp"])
    await r.sc("add_audit", A + [THUMB, "", JPG, "image/jpeg"])
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/gif"], note="未知类型 → .bin")
    await r.sc("add_audit", A + [THUMB, "", PNG, ""], note="类型为空 → .bin，image_type 仍存空")
    await r.sc("add_audit", A + [THUMB, "", b"", "image/png"], note="空字节 = 没有原图")
    await r.sc("add_audit", A + [THUMB, "", None, "image/png"], note="没有原图时 image_type 存空串")
    await r.sc("add_audit", A + [None], note="缩略图为 NULL")
    await r.sc("add_audit", A + [b""], note="缩略图为空 BLOB（不是 NULL）")
    await r.sc("add_audit", [None, "游" * 100, "image", "模" * 100, "ok", "提示词 ✓" * 50, "负面", THUMB],
               note="key_id=NULL；key_name / model 按码点截 80；提示词不截")
    await r.sc("add_audit", A + [THUMB], setup=mixed, note="id 接着递增")
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], setup=mixed, note="已有文件时再写一个")

    async def dir_is_file(db):
        db.image_dir.parent.mkdir(parents=True, exist_ok=True)
        db.image_dir.write_bytes(b"i am a file")
    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"], setup=dir_is_file,
               note="原图目录建不出来（OSError）：只记提示词，不存原图")

    await r.sc("add_audit", A + [THUMB, "", PNG, "image/png"],
               extra_ddl=["CREATE TRIGGER fail_audit BEFORE INSERT ON generation_audit "
                          "BEGIN SELECT RAISE(ABORT, 'audit blocked'); END"],
               note="入库失败：刚写的文件被删掉，异常照抛")

    # ---- list_audit / list_audit_ids
    await r.sc("list_audit", [50, 0], setup=mixed, note="has_thumb / has_image 看 thumb / image / image_file")
    await r.sc("list_audit", [3, 0], setup=mixed)
    await r.sc("list_audit", [3, 3], setup=mixed)
    await r.sc("list_audit", [3, 6], setup=mixed)
    await r.sc("list_audit", [10, 100], setup=mixed, note="offset 超出")
    await r.sc("list_audit", [50, 0, 1], setup=mixed)
    await r.sc("list_audit", [50, 0, 2], setup=mixed)
    await r.sc("list_audit", [50, 0, 99], setup=mixed)
    await r.sc("list_audit", [50, 0, 0], setup=mixed, note="key_id=0 是「有值」，按 key_id=0 过滤")
    await r.sc("list_audit", [-1, 0], setup=mixed, note="limit=-1：SQLite 不限制")
    await r.sc("list_audit", [0, 0], setup=mixed)
    await r.sc("list_audit", [2, -5], setup=mixed, note="负 offset 当 0")
    await r.sc("list_audit", [50, 0])
    await r.sc("list_audit_ids", [50, 0, (1, 2)], setup=mixed)
    await r.sc("list_audit_ids", [50, 0, (2,)], setup=mixed)
    await r.sc("list_audit_ids", [50, 0, ()], setup=mixed, note="空 → 不筛")
    await r.sc("list_audit_ids", [50, 0, (99,)], setup=mixed)
    await r.sc("list_audit_ids", [2, 1, (1, 2)], setup=mixed)
    await r.sc("list_audit_ids", [50, 0, (1, 1, 2)], setup=mixed, note="重复 id")
    await r.sc("list_audit_ids", [50, 0, ()])

    # ---- audit_thumb
    async def thumb_empty(db):
        await two_keys(db)
        await aud(db, {"id": 1, "name": "a"}, NOW - HOUR, thumb=b"")
    await r.sc("audit_thumb", [1], setup=mixed)
    await r.sc("audit_thumb", [5], setup=mixed, note="缩略图 NULL → None")
    await r.sc("audit_thumb", [99], setup=mixed)
    await r.sc("audit_thumb", [1], setup=thumb_empty, note="空 BLOB 不是 None")

    # ---- audit_image
    await r.sc("audit_image", [1], setup=mixed, note="老记录：BLOB")
    await r.sc("audit_image", [2], setup=mixed, note="文件")
    await r.sc("audit_image", [4], setup=mixed)
    await r.sc("audit_image", [5], setup=mixed)
    await r.sc("audit_image", [3], setup=mixed, note="没有原图")
    await r.sc("audit_image", [99], setup=mixed, note="没有这条记录")

    async def file_gone(db):
        await mixed(db)
        for p in db.image_dir.rglob("*.png"):
            p.unlink()
    await r.sc("audit_image", [2], setup=file_gone, note="库里有记录文件被删了 → (None, '')")

    async def blob_no_type(db):
        await two_keys(db)
        await blob(db, {"id": 1, "name": "a"}, NOW - HOUR, image_type="")
    await r.sc("audit_image", [1], setup=blob_no_type, note="类型为空 → image/png")

    async def odd_paths(db):
        await two_keys(db)
        a = {"id": 1, "name": "a"}
        put(db, "../outside.png", b"OUTSIDE", extra_seed=True)
        await file_row(db, a, NOW - 5 * HOUR, "../outside.png", size=7, prompt="越界")
        await file_row(db, a, NOW - 4 * HOUR, "/nonexistent-dir/x.png", size=1, prompt="绝对路径")
        put(db, "20300301/empty.png", b"")
        await file_row(db, a, NOW - 3 * HOUR, "20300301/empty.png", size=0, prompt="空文件")
        put(db, "20300301/real.png", PNG)
        await file_row(db, a, NOW - 2 * HOUR, "20300301/../20300301/real.png", size=24, prompt="带 .. 但仍在目录内")
        await file_row(db, a, NOW - 1 * HOUR, "20300301/missing.png", size=1, prompt="文件不存在")
    for i, note in ((1, "越界路径 → None"), (2, "绝对路径 → None"), (3, "空文件 → (b'', image/png)"),
                    (4, "路径含 .. 但解析后仍在目录内"), (5, "文件不存在")):
        await r.sc("audit_image", [i], setup=odd_paths, note=note)

    # ---- audit_images_for / audit_image_count / audit_image_bytes
    await r.sc("audit_images_for", [1], setup=mixed, note="某成员带原图的记录，ts 升序")
    await r.sc("audit_images_for", [2], setup=mixed)
    await r.sc("audit_images_for", [1, 0, NOW - 4 * DAYS], setup=mixed, note="since 过滤")
    await r.sc("audit_images_for", [1, 1], setup=mixed, note="max_bytes=1：至少给最新一条")
    await r.sc("audit_images_for", [1, 30], setup=mixed, note="最新的先装，超出就停")
    await r.sc("audit_images_for", [1, 45], setup=mixed)
    await r.sc("audit_images_for", [1, 1000], setup=mixed)
    await r.sc("audit_images_for", [1, -5], setup=mixed, note="负数当不限制")
    await r.sc("audit_images_for", [99], setup=mixed)
    await r.sc("audit_images_for", [99, 100], setup=mixed, note="max_bytes>0 且没有记录 → []")
    await r.sc("audit_images_for", [1, 0, NOW], setup=mixed, note="since 在所有记录之后")

    async def mixed_missing(db):
        await mixed(db)
        for p in db.image_dir.rglob("*.webp"):
            p.unlink()
    await r.sc("audit_images_for", [1], setup=mixed_missing, note="文件丢了的记录跳过")
    await r.sc("audit_images_for", [1, 30], setup=mixed_missing, note="max_bytes 先按库里的 image_size 选，再跳过丢文件的")
    await r.sc("audit_images_for", [1], setup=odd_paths, note="越界 / 绝对路径 / 空文件")

    await r.sc("audit_image_count", [1], setup=mixed)
    await r.sc("audit_image_count", [2], setup=mixed)
    await r.sc("audit_image_count", [1, NOW - 4 * DAYS], setup=mixed)
    await r.sc("audit_image_count", [99], setup=mixed)
    await r.sc("audit_image_count", [1])
    await r.sc("audit_image_bytes", setup=mixed, note="文件按 image_size，老 BLOB 按 LENGTH")
    await r.sc("audit_image_bytes")

    async def sizes(db):
        await two_keys(db)
        a = {"id": 1, "name": "a"}
        await blob(db, a, NOW - 3 * HOUR, image=b"B" * 11)
        await file_row(db, a, NOW - 2 * HOUR, "20300301/x.png", size=None, prompt="文件记录没有 image_size")
        await file_row(db, a, NOW - HOUR, "20300301/y.png", size=100, prompt="有 image_size")
        await aud(db, a, NOW - 60, prompt="没有原图")
    await r.sc("audit_image_bytes", setup=sizes, note="image_size 优先，其次 LENGTH(image)，都没有按 0")

    async def after_purge_all(db):
        await mixed(db)
        await db.purge_audit_images(NOW)
    await r.sc("audit_image_bytes", setup=after_purge_all, note="清空原图之后")

    # ---- purge_audit_images_to_cap
    async def capset(db):
        await two_keys(db)
        a = {"id": 1, "name": "a"}
        for i in range(8):
            await aud(db, a, NOW - (20 - i) * HOUR, image=bytes([65 + i]) * 100, image_type="image/png", prompt=f"c{i}")
        await aud(db, a, NOW - 11 * HOUR, prompt="无原图")                        # id9
        await blob(db, a, NOW - 10 * HOUR, image=b"B" * 50, prompt="blob50")       # id10
    await r.sc("purge_audit_images_to_cap", [2000], setup=capset, note="没超")
    await r.sc("purge_audit_images_to_cap", [850], setup=capset, note="刚好等于 cap")
    await r.sc("purge_audit_images_to_cap", [849], setup=capset)
    await r.sc("purge_audit_images_to_cap", [500], setup=capset)
    await r.sc("purge_audit_images_to_cap", [500, 0.5], setup=capset)
    await r.sc("purge_audit_images_to_cap", [500, 1.0], setup=capset)
    await r.sc("purge_audit_images_to_cap", [500, 0.0], setup=capset, note="清到 0")
    await r.sc("purge_audit_images_to_cap", [500, 2.0], setup=capset, note="low_water>1：目标为负，清第一条就够")
    await r.sc("purge_audit_images_to_cap", [1], setup=capset, note="清光，含 BLOB")
    await r.sc("purge_audit_images_to_cap", [0], setup=capset, note="cap<=0 → 不清")
    await r.sc("purge_audit_images_to_cap", [-5], setup=capset)
    await r.sc("purge_audit_images_to_cap", [100])

    # ---- purge_audit_images_before_id / purge_audit_images / purge_audit
    await r.sc("purge_audit_images_before_id", [4], setup=mixed, note="清 id<4 的原图（文件 + BLOB），缩略图 / 提示词保留")
    await r.sc("purge_audit_images_before_id", [1], setup=mixed, note="没有符合的")
    await r.sc("purge_audit_images_before_id", [1000], setup=mixed, note="全部")
    await r.sc("purge_audit_images_before_id", [1000])
    await r.sc("purge_audit_images_before_id", [3], setup=odd_paths, note="越界路径：库里清了，外面的文件不能删")

    async def many120(db):
        await two_keys(db)
        a = {"id": 1, "name": "a"}
        for i in range(120):
            await blob(db, a, NOW - (200 - i) * 60, image=bytes([i % 256]) * 3, prompt=f"b{i}")
        await aud(db, a, NOW - 100, image=PNG, image_type="image/png", prompt="文件")
        await aud(db, a, NOW - 90, prompt="无原图")
    await r.sc("purge_audit_images_before_id", [200], setup=many120, note="121 条分 3 批（每批 50）")
    await r.sc("purge_audit_images_before_id", [60], setup=many120)

    await r.sc("purge_audit_images", [NOW - int(3.5 * DAYS)], setup=mixed, note="按时间清原图")
    await r.sc("purge_audit_images", [NOW - 100 * DAYS], setup=mixed)
    await r.sc("purge_audit_images", [NOW], setup=mixed)
    await r.sc("purge_audit_images", [NOW])

    await r.sc("purge_audit", [NOW - int(3.5 * DAYS)], setup=mixed, note="先删原图文件，再删记录；返回删掉的记录数")
    await r.sc("purge_audit", [NOW - 100 * DAYS], setup=mixed)
    await r.sc("purge_audit", [NOW], setup=mixed)
    await r.sc("purge_audit", [NOW])
    await r.sc("purge_audit", [NOW], setup=odd_paths, note="越界路径的文件不会被删")

    # ---- sweep_orphan_images
    await r.sc("sweep_orphan_images", [NOW], note="目录不存在 → 0")

    async def sweep_dirs(db):
        await two_keys(db)
        a = {"id": 1, "name": "a"}
        await aud(db, a, NOW - 2 * DAYS, image=PNG, image_type="image/png", prompt="已知-1")    # 20300313/...01.png
        await aud(db, a, NOW - HOUR, image=WEBP, image_type="image/webp", prompt="已知-2")      # 20300315/...02.webp
        old, new = NOW - 10 * DAYS, NOW - 3600
        put(db, "20300310/old_orphan.png", b"o1", old)
        put(db, "20300310/old_crash.png.tmp", b"o2", old)
        put(db, "20300312/new_orphan.png", b"n1", new)                                   # 比 older_than 新：保留
        put(db, "20300312/equal.png", b"eq", NOW - DAYS)                                 # 恰好等于 older_than：保留
        put(db, "20300311/sub/deep_orphan.png", b"d1", old)                              # 嵌套
        put(db, "stray.tmp", b"s1", old)                                                 # 目录顶层的散文件
        (db.image_dir / "20300301").mkdir()                                              # 空的老目录
        (db.image_dir / "20300316").mkdir()                                              # 空的新目录
        # 日期目录自己的 mtime（创建文件会改它，最后统一设置）
        touch_dir(db, "20300301", old)
        touch_dir(db, "20300316", NOW + DAYS)
        touch_dir(db, "20300312", new)
        touch_dir(db, "20300311/sub", old)
        touch_dir(db, "20300311", old)
        touch_dir(db, "20300313", old)
        touch_dir(db, "20300315", new)
        touch_dir(db, "20300310", old)
    await r.sc("sweep_orphan_images", [NOW - DAYS], setup=sweep_dirs, note="删老孤儿 / .tmp / 嵌套 / 顶层散文件；空老目录删，空新目录留")
    await r.sc("sweep_orphan_images", [NOW - 100 * DAYS], setup=sweep_dirs, note="older_than 很早：什么都不删")
    await r.sc("sweep_orphan_images", [NOW + 100 * DAYS], setup=sweep_dirs, note="older_than 在未来：除已知文件外全删，空目录也全删")

    async def sweep_empty_db(db):
        put(db, "20300310/a.png", b"a", NOW - 10 * DAYS)
        put(db, "20300310/b.png", b"b", NOW - 10 * DAYS)
        touch_dir(db, "20300310", NOW - 10 * DAYS)
    await r.sc("sweep_orphan_images", [NOW - DAYS], setup=sweep_empty_db, note="库里没有任何记录：目录里全是孤儿")
    r.save()


AREAS = ["reconciliation", "upstream", "settings", "coupons", "admin_actions", "discord", "feedback", "misc",
         "keys", "counters", "usage", "audit"]


async def main(names=None):
    for name in names or AREAS:
        await globals()["area_" + name]()


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1:] or None))
