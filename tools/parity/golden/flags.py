"""features / site_flags / bot_config / params 的 golden 夹具（合成数据，固定种子）。

输出（crates/gate-core/tests/golden/flags/）：
  params_p / params_snapshot / params_update / params_load   —— app/params.py
  site_registry / site_parse / site_default_of / site_get / site_encode —— app/site_flags.py
  features_pure / features_check                           —— app/features.py
  bot_validate / bot_load / bot_save / bot_report / bot_snapshot —— app/bot_config.py

注意：本文件里不要写 \\uXXXX 这样的转义（工具链会把它提前解码），非 ASCII 字符一律用 chr()。
"""
from __future__ import annotations

import asyncio
import copy
import json
import os
import random
import sys
import tempfile
import types
from pathlib import Path

from _golden import Writer, enc

from app import bot_config, features, params, site_flags

LOOP = asyncio.new_event_loop()


def run(coro):
    return LOOP.run_until_complete(coro)


def named(name):
    def deco(fn):
        fn.__name__ = name
        return fn
    return deco


MISSING = object()
NBSP = chr(0xA0)
ZWSP = chr(0x200B)
BOM = chr(0xFEFF)
IDEO_SP = chr(0x3000)
CAT = chr(0x732B)          # 猫
EMOJI = chr(0x1F600)
FULL_12 = chr(0xFF11) + chr(0xFF12)   # 全角 12


class KV:
    """site_settings 的假库：get_setting 返回原始 TEXT，写入时 str()，与 Database 一致。"""

    def __init__(self, kv=None):
        self.kv = dict(kv or {})
        self.writes: list[list[str]] = []
        self.reads: list[str] = []

    async def get_setting(self, key, default=None):
        self.reads.append(key)
        return self.kv.get(key, default)

    async def set_setting(self, key, value):
        self.kv[key] = str(value)
        self.writes.append([key, str(value)])

    async def set_settings_bulk(self, values):
        for k, v in values.items():
            await self.set_setting(k, v)


def attempt(fn, *args, **kwargs):
    """调用并把结果 / 异常装成 (out, err)。"""
    try:
        return fn(*args, **kwargs), None
    except Exception as e:  # noqa: BLE001 — 异常本身就是要对拍的行为
        return None, {"type": type(e).__name__, "msg": str(e)}


# ============================================================ params.py
real_load = params._load
DATA: dict = {}
params._load = lambda: DATA          # P / overridden / snapshot 都经由 _load()

FILE_VALUES = [
    MISSING, None, True, False, 0, 1, -1, 7, 99, 2 ** 40, 0.0, 1.5, -2.5, 3.0, 1e20, 0.1,
    "7", " 8 ", "abc", "1.5", "", "false", "true", "1_000", FULL_12, "-3", "0", "1e2",
    [], [1], [[90, 1.3]], {}, {"a": 1},
]
DEFAULTS = [True, False, 5, 0, -3, 0.5, 2.0, 100, "s", None, [[90, 1.3], [70, 1.1]], [], {}]

with Writer("flags/params_p") as w:
    for v in FILE_VALUES:
        for d in DEFAULTS:
            if isinstance(v, (int, float)) and not isinstance(v, bool) and abs(v) >= 1e19 \
                    and isinstance(d, int) and not isinstance(d, bool):
                continue          # int(1e20) 是 Python 大整数，Rust 饱和到 i64：不在对拍范围（见 params.rs 说明）
            DATA.clear()
            if v is not MISSING:
                DATA["k"] = v
            DATA["other"] = 3
            out, err = attempt(params.P, "k", copy.deepcopy(d))
            w.record(fn="P", data=copy.deepcopy(DATA), args=["k", d], out=out, err=err)
    # 键缺失时 P 就是起点值本身
    for d in DEFAULTS:
        DATA.clear()
        out, err = attempt(params.P, "nope", copy.deepcopy(d))
        w.record(fn="P", data={}, args=["nope", d], out=out, err=err)

with Writer("flags/params_snapshot") as w:
    rng = random.Random(20261014)
    names = ["capacity.hourly_min", "capacity.idle_share", "allocation.v5_k_table", "share.warn", "zz", "A", "b.c", CAT]
    for _ in range(120):
        data = {n: rng.choice([1, 2.5, "x", None, [1], True]) for n in rng.sample(names, rng.randint(0, 4))}
        defaults = {n: rng.choice([5, 0.5, "s", None, [[1, 2]], False]) for n in rng.sample(names, rng.randint(0, 5))}
        DATA.clear()
        DATA.update(copy.deepcopy(data))
        params._DEFAULTS.clear()
        params._DEFAULTS.update(copy.deepcopy(defaults))
        w.record(fn="snapshot", data=data, defaults=defaults, out=params.snapshot())
        for n in names + ["missing"]:
            w.record(fn="overridden", data=data, args=[n], out=params.overridden(n))
params._DEFAULTS.clear()

# update()：真读真写临时目录里的 private_params.json，再把写出的文件读回来
params._load = real_load
with Writer("flags/params_update") as w, tempfile.TemporaryDirectory() as tmp:
    os.environ["DATA_DIR"] = tmp
    rng = random.Random(7)
    for _ in range(80):
        init = {n: rng.choice([1, 2.5, "x", [1], True]) for n in rng.sample(["a", "b", "c", "d", "e"], rng.randint(0, 4))}
        upd = {n: rng.choice([None, 7, 0.25, "y", [2], False]) for n in rng.sample(["a", "b", "c", "f", "g"], rng.randint(0, 4))}
        Path(tmp, "private_params.json").write_text(json.dumps(init), encoding="utf-8")
        params._MTIME = -1.0
        params._CACHE = {}
        # 先读一次把缓存「预热」：update() 持有 _LOCK 时再调 _load()，缓存过期的话 _load() 也要拿 _LOCK，
        # threading.Lock 不可重入 → 自己把自己锁死。线上靠 P() 的高频调用让缓存总是新的，才没踩到。
        params._load()
        params.update(copy.deepcopy(upd))
        after = json.loads(Path(tmp, "private_params.json").read_text(encoding="utf-8"))
        w.record(fn="update", data=init, args=[upd], out=after)

# _load()：文件文本 → 缓存字典
LOAD_TEXTS = [
    '{"a": 1}', '[1, 2]', 'null', '5', '"s"', '', '{', '{"a": 1,}', '{"a": {"b": [1, 2.5, null]}}', '  {"a": 2}  ',
    BOM + '{"a": 1}', '{"a": 1, "a": 2}', '{"a": 12345678901234567890}', '{"a": 1e2}', '{"capacity.hourly_min": "90"}',
    '{"k": "' + CAT + '"}', '{}', 'true', '{"a": [], "b": {}}',
]
with Writer("flags/params_load") as w, tempfile.TemporaryDirectory() as tmp:
    os.environ["DATA_DIR"] = tmp
    for text in LOAD_TEXTS:
        Path(tmp, "private_params.json").write_text(text, encoding="utf-8")
        params._MTIME = -1.0
        params._CACHE = {}
        out, err = attempt(params._load)
        w.record(fn="_load", text=text, out=out, err=err)
    Path(tmp, "private_params.json").unlink()
    params._MTIME = -1.0
    w.record(fn="_load_missing", text=None, out=params._load())
params._load = lambda: DATA

# ============================================================ site_flags.py
REGISTRY = {s.key: s for s in site_flags.ALL}
SYNTH = [
    site_flags.Spec("x_choice", "choice", "a", "t", "d", choices=("a", "B", "c d")),
    site_flags.Spec("x_int_free", "int", 5, "t", "d"),
    site_flags.Spec("x_int_lo", "int", 5, "t", "d", lo=0),
    site_flags.Spec("x_int_hi", "int", 5, "t", "d", hi=10),
    site_flags.Spec("x_float_neg", "float", 1.5, "t", "d", lo=-10, hi=10),
    site_flags.Spec("x_float_free", "float", 0.0, "t", "d"),
    site_flags.Spec("x_str", "str", "dflt", "t", "d"),
    site_flags.Spec("x_flag", "flag", False, "t", "d"),
    site_flags.Spec("x_flag_env", "flag", None, "t", "d", env="global_daily_v5"),
    site_flags.Spec("x_int_env", "int", None, "t", "d", 1, 9, env="global_daily_v5"),
]


def spec_dict(s):
    return {"key": s.key, "kind": s.kind, "default": s.default, "owner": s.owner, "doc": s.doc,
            "lo": s.lo, "hi": s.hi, "choices": list(s.choices), "env": s.env}


def spec_ref(s):
    return {"ref": s.key} if s.key in REGISTRY and REGISTRY[s.key] is s else {"inline": spec_dict(s)}


with Writer("flags/site_registry") as w:
    for s in site_flags.ALL:
        w.record(fn="spec", out=spec_dict(s))
    w.record(fn="all_keys", out=[s.key for s in site_flags.ALL])
    w.call(site_flags.facts_table)

RAWS = [
    None, "", "  ", "\t\n", "abc", "1", "0", "2", "-1", "-5", "5.7", "5.0", "1e3", "1E-2", "1_000", "1__0", " 7 ", "\t7\n",
    "true", "TRUE", " Yes ", "on", "ON", "off", "OFF", "no", "False", "false", "nope", "maybe", "inf", "-inf", "Infinity",
    "nan", "NaN", "1e400", "-1e400", "9" * 30, "0x10", ".5", "5.", "12abc", "a", "A", "B", "b", "c d", " c d ", "c  d",
    FULL_12, CAT, NBSP + "7" + NBSP, IDEO_SP + "1" + IDEO_SP, "2000000000", "2000000001", "1e10", "500", "501", "100000",
    "100001", "365", "366", "8760", "8761", "50.9", "-0.5", "0.0", "-0.0",
    True, False, 0, 1, 7, -1, 500, 501, 10 ** 9, 2.5, 7.0, 1e22, -3.5, 0.0,
    [], [1], {}, {"a": 1},
]
FALLBACKS = [None, 0, "fb", 3.5]


INF_LIKE = ("inf", "-inf", "infinity", "-infinity", "1e400", "-1e400")


def parse_skip(spec, raw):
    # 登记表里的数值类型都有上下限，所以结果必是有限数；下面两类合成 spec 的结果 Rust 的 Value 装不下，不对拍：
    # - float 缺上下限：inf 是合法的 float 结果，但 JSON 没法表示；
    # - int 缺上下限：inf 在 Python 里是 OverflowError（没被捕获），超大数是 Python 大整数。
    if spec.kind == "float" and (spec.lo is None or spec.hi is None):
        if isinstance(raw, str) and raw.strip().lower() in INF_LIKE:
            return True
    if spec.kind == "int" and (spec.lo is None or spec.hi is None):
        if isinstance(raw, str) and raw.strip().lower() in ("inf", "-inf", "infinity", "1e400", "-1e400", "9" * 30):
            return True
        if isinstance(raw, float) and abs(raw) >= 1e19:
            return True
        if isinstance(raw, int) and not isinstance(raw, bool) and abs(raw) >= 10 ** 19:
            return True
    return False


with Writer("flags/site_parse") as w:
    for spec in list(site_flags.ALL) + SYNTH:
        for raw in RAWS:
            if parse_skip(spec, raw):
                continue
            for fb in FALLBACKS:
                out, err = attempt(site_flags.parse, spec, copy.deepcopy(raw), fb)
                w.record(fn="parse", spec=spec_ref(spec), args=[raw, fb], out=out, err=err)
    rng = random.Random(11)
    for _ in range(2500):
        spec = rng.choice(list(site_flags.ALL) + SYNTH)
        raw = rng.choice([
            str(rng.randint(-10 ** 6, 10 ** 6)), str(round(rng.uniform(-1e6, 1e6), rng.randint(0, 4))),
            str(rng.randint(-50, 600)), repr(rng.uniform(-3, 3)), rng.choice(["1", "0", "true", "off"]),
            rng.randint(-100, 3 * 10 ** 9), rng.uniform(-100, 3e9),
        ])
        if parse_skip(spec, raw):
            continue
        fb = rng.choice(FALLBACKS)
        out, err = attempt(site_flags.parse, spec, raw, fb)
        w.record(fn="parse", spec=spec_ref(spec), args=[raw, fb], out=out, err=err)

ENVS = [
    None, {}, {"global_daily_v5": 190}, {"global_daily_v5": None}, {"global_daily_v5": "abc"}, {"global_daily_v5": -4},
    {"global_daily_v5": 10 ** 7}, {"global_daily_v5": 12.9}, {"global_daily_v5": True}, {"global_daily_v5": "7"},
    {"global_daily_v5": 3, "global_daily_anlas": 250, "global_monthly_anlas": 9000},
    {"global_monthly_anlas": 9000.5}, {"global_monthly_anlas": 0}, {"global_monthly_anlas": 10 ** 8},
    {"global_monthly_anlas": -1.5}, {"global_daily_anlas": 0}, {"global_daily_anlas": 1e9}, {"other": 1},
]
with Writer("flags/site_default_of") as w:
    for spec in list(site_flags.ALL) + SYNTH:
        for env in ENVS:
            settings = types.SimpleNamespace(**env) if env is not None else None
            out, err = attempt(site_flags.default_of, spec, settings)
            w.record(fn="default_of", spec=spec_ref(spec), env=env, out=out, err=err)

with Writer("flags/site_get") as w:
    for spec in list(site_flags.ALL) + SYNTH:
        for raw in [MISSING, "", " ", "0", "1", "true", "off", "7", "x", "-3", "9999999", "12.5", "nan", "c d", "dflt", CAT]:
            for env in ([None, {"global_daily_v5": 190, "global_daily_anlas": 250, "global_monthly_anlas": 9000}]):
                db = KV({} if raw is MISSING else {spec.key: raw})
                settings = types.SimpleNamespace(**env) if env is not None else None
                out, err = attempt(lambda: run(site_flags.get(db, spec, settings)))
                w.record(fn="get", spec=spec_ref(spec), raw=None if raw is MISSING else raw,
                         present=raw is not MISSING, env=env, out=out, err=err)

ENCODE_VALUES = [
    True, False, 0, 1, 2, -1, 7, 50, 500, 501, 100000, 2 * 10 ** 9, 3 * 10 ** 9, 0.0, 1.5, 12.5, -0.5, 1e22, 2.5e9,
    "", " ", "0", "1", "7", "12.5", "abc", "true", "off", "a", "B", "c d", "b", " a ", CAT, None, [], [1, "a"], {}, {"k": "v"},
    "a\bb\x0c\x1f\x7f", ["a\bb\x0c\x1f\x7f", "~", "a~b", " ", "'"], {"k": [1, None, True, "it's", 'say "hi"']}, ["it's"], ['say "hi"'], ["a\nb\\c"], [CAT, EMOJI], [NBSP, ZWSP, BOM, IDEO_SP],
    [1.5, 2.0, 1e22, 1e-7], {"a": {"b": {}}}, "x" * 3, NBSP,
]
with Writer("flags/site_encode") as w:
    for spec in list(site_flags.ALL) + SYNTH:
        for v in ENCODE_VALUES:
            if spec.kind == "int" and (spec.lo is None or spec.hi is None) and isinstance(v, float) and abs(v) >= 1e19:
                continue          # Python 大整数，见 parse_skip
            out, err = attempt(site_flags.encode, spec, copy.deepcopy(v))
            w.record(fn="encode", spec=spec_ref(spec), args=[v], out=out, err=err)

# ============================================================ features.py
NAMES = list(features.FEATURES) + ["bogus"]

with Writer("flags/features_pure") as w:
    w.record(fn="constants", out={
        "FEATURES": list(features.FEATURES.items()), "GLOBAL_KEY": features.GLOBAL_KEY,
        "ANLAS_FEATURES": list(features.ANLAS_FEATURES.items()), "KIND_FEATURE": list(features.KIND_FEATURE.items())})
    for f in NAMES + ["", "text "]:
        w.call(features.kinds_for, f)
    PARSE_IN = [None, "", "image", "image,vibe", "image, vibe", "bogus,tags", "image,image", "voice,image,text", ",", ",,", " ",
                "IMAGE", "image,", ",image", "image;vibe", 5, 0, True, False, 1.5, ["image"], ["image", "vibe"], {}, {"image": 1},
                ",".join(features.FEATURES), ",".join(reversed(list(features.FEATURES))), CAT, "image," + CAT]
    for v in PARSE_IN:
        w.call(features.parse_list, v)
    NORM_IN = [[], ["image"], ["voice", "image"], ["image", "image"], ["bogus"], ["tags", "bogus", "vibe", "text"],
               list(reversed(list(features.FEATURES))), ["IMAGE"], [""], ["image,vibe"]]
    for v in NORM_IN:
        w.call(features.normalize, v)
        w.call(features.dump, v)
    w.call(features.dump, None)
    KEYS = [{}, {"features": None}, {"features": ""}, {"features": "image"}, {"features": "image,vibe,upscale"},
            {"features": "bogus"}, {"features": 5}, {"id": 3}, {"features": "voice,voice"}]
    for k in KEYS:
        w.call(features.key_features, k)
    # global_flags 的解析
    RAW_FLAGS = [None, "", "{}", '{"image": false}', '{"image": 0}', '{"vibe": "no"}', '[1]', 'null', 'garbage', '5', '"s"', 'true',
                 '{"image": true, "voice": false, "zzz": false}', '1.5', '{"image": false, "tags": false, "text": true}',
                 '{"image": null}', '{"augment": false, "upscale": false, "vibe": false}', ' ', '{"image": false', '[]', '{"a":1}']
    for raw in RAW_FLAGS:
        db = KV({} if raw is None else {features.GLOBAL_KEY: raw})
        out, err = attempt(lambda: run(features.global_flags(db)))
        w.record(fn="global_flags", args=[raw], out=out, err=err)

with Writer("flags/features_check") as w:
    rng = random.Random(424242)
    NOW = 1_790_000_000.0
    holder = types.SimpleNamespace(now=NOW)
    features.time = types.SimpleNamespace(time=lambda: holder.now)

    @named("check")
    def check_case(kv, key, name, env, now):
        holder.now = now
        db = KV(kv)
        settings = types.SimpleNamespace(**env) if env is not None else None
        out = run(features.check(db, key, name, settings))
        vibe_reads = sum(1 for k in db.reads if k.startswith("global_daily_anlas"))
        return {"out": out, "vibe_reads": vibe_reads}

    FLAG_RAWS = [None, "{}", '{"image": false}', '{"vibe": false}', '{"voice": false, "tags": false}', '{"upscale": false}',
                 '[1]', 'garbage', '{"text": false, "image": true}', '{"augment": false}']
    UNTILS = [MISSING, "0", "1790000500", "1789999000", "1790000000", "abc", "-5", "3000000000", "1790000000.5"]
    CAPS = [MISSING, "0", "200", "5", "abc", "-1", "100001"]
    ENV2 = [None, {}, {"global_daily_anlas": 200}, {"global_daily_anlas": 0}, {"global_daily_anlas": "x"}]
    ADMIN = [MISSING, 0, 1, None, True, False]
    FEAT = [MISSING, None, "", "image", "vibe", "image,vibe", "image,upscale,augment,tags,text,voice", "tags", "bogus"]
    ANLAS = [MISSING, 0, 1, None, True, False, "0"]
    for _ in range(4000):
        key = {}
        a, f, an = rng.choice(ADMIN), rng.choice(FEAT), rng.choice(ANLAS)
        if a is not MISSING:
            key["is_admin"] = a
        if f is not MISSING:
            key["features"] = f
        if an is not MISSING:
            key["allow_anlas"] = an
        kv = {}
        fr = rng.choice(FLAG_RAWS)
        if fr is not None:
            kv[features.GLOBAL_KEY] = fr
        u, c = rng.choice(UNTILS), rng.choice(CAPS)
        if u is not MISSING:
            kv["global_daily_anlas_until"] = u
        if c is not MISSING:
            kv["global_daily_anlas"] = c
        name = rng.choice(NAMES if rng.random() < 0.95 else ["bogus", "image"])
        w.call(check_case, kv, key, name, rng.choice(ENV2), NOW)

# ============================================================ bot_config.py
with Writer("flags/bot_constants") as w:
    w.record(fn="constants", out={
        "FIELDS": [[n, d, k.__name__, lo, hi] for n, (d, k, lo, hi) in bot_config.FIELDS.items()],
        "STATUS_KEY": bot_config.STATUS_KEY, "EVENTS_KEY": bot_config.EVENTS_KEY,
        "EVENT_KINDS": list(bot_config.EVENT_KINDS.items())})

with Writer("flags/bot_validate") as w:
    rng = random.Random(99)
    VALS = [True, False, 0, 1, 2, -1, 30, 31, 500, 501, 50, 80, 81, 1.0, 0.0, 2.5, "5", "", "  ", " x ", "x" * 50, "x" * 51,
            "x" * 80, "x" * 81, CAT * 50, CAT * 51, CAT * 80, CAT * 81, NBSP + "ab" + NBSP, IDEO_SP, None, [], [1], {"a": 1},
            "a\nb", "\tz\t", ZWSP, EMOJI * 30, "  " + CAT + "  ", 1e30]
    FIELDS = list(bot_config.FIELDS)
    for _ in range(3000):
        body = {}
        for n in rng.sample(FIELDS + ["extra", "x"], rng.randint(0, 5)):
            body[n] = rng.choice(VALS)
        w.call(bot_config.validate, body)
    for n in FIELDS:
        for v in VALS:
            w.call(bot_config.validate, {n: v})
    w.call(bot_config.validate, {})


def stored_map(rng):
    d = {}
    for n in bot_config.FIELDS:
        if rng.random() < 0.7:
            d["bot_" + n] = rng.choice(["0", "1", "2", "-1", "30", " 7 ", "1.5", "abc", "", "1_0", "500", "501", FULL_12, "跑图分享",
                                        "  spaced  ", "x" * 90, "+3", "0x1"])
    if rng.random() < 0.3:
        d["bot_other"] = "1"
    return d


with Writer("flags/bot_load") as w:
    rng = random.Random(5)
    for _ in range(600):
        st = stored_map(rng)
        out, err = attempt(lambda: run(bot_config.load(KV(st))))
        w.record(fn="load", args=[st], out=out, err=err)

with Writer("flags/bot_save") as w:
    rng = random.Random(6)
    for _ in range(300):
        vals = {}
        for n, (d, k, lo, hi) in bot_config.FIELDS.items():
            if rng.random() < 0.5:
                vals[n] = rng.randint(lo, hi) if k is int else rng.choice(["a", CAT * 3, "LongCat", "x y"])
        db = KV()
        run(bot_config.save(db, vals))
        w.record(fn="save", args=[vals], out=db.writes)


def rand_text(rng):
    return rng.choice(["", "t", "a\bb", "x\x0c", "\x1f", "\x7f", "tab\there", "标题", "## 标题", "  # 带空格", "###", "# # x", "x" * 119, "x" * 120, "x" * 121, "#" + "y" * 130,
                       CAT * 130, "a\nb", 'quote"s', "it's", EMOJI * 5, ZWSP + "z", "  ", 0, 7, 1.5, True, False, None, [], [1, 2], {"a": 1}])


STATUS_KEYS = ["user", "guild", "latency_ms", "ready_at", "ai_ready", "ai_today", "version", "token", "secret", "seen_at"]
with Writer("flags/bot_report") as w:
    rng = random.Random(8)
    for _ in range(500):
        status = {k: rng.choice(["奶妹", CAT + "x", 12, 3.75, True, None, "v1.2", [1, "a"], {"x": 1}, 1e22, 0.1])
                  for k in rng.sample(STATUS_KEYS, rng.randint(0, 8))}
        db = KV()
        now = 1_790_000_000.0 + rng.randint(0, 10 ** 6) + rng.choice([0.0, 0.25, 0.123456])
        run(bot_config.report(db, status, None, now=now))
        w.record(fn="report_status", args=[status, now], out=[v for k, v in db.writes if k == bot_config.STATUS_KEY][-1])
    EVENTS_PRE = [None, "", "[]", "{}", "null", "5", "garbage", "[", '[{"at": 1.5, "kind": "like"}]', '"abc"',
                  json.dumps([{"at": 1790000000.5, "kind": "skip", "title": "t" + str(i)} for i in range(49)]),
                  json.dumps([{"at": 1790000000.5, "kind": "skip", "title": "t" + str(i)} for i in range(50)]),
                  json.dumps([{"at": 1.0, "kind": "error", "title": CAT * 3}], ensure_ascii=False),
                  json.dumps([1, "two", None, [3]]), json.dumps([{"at": 1.0, "n": 1e22}])]
    for _ in range(700):
        pre = rng.choice(EVENTS_PRE)
        ev = {"kind": rng.choice(list(bot_config.EVENT_KINDS) * 3 + ["bogus", "", None, 5, ["like"]])}
        for k in ("title", "author", "detail", "url", "junk"):
            if rng.random() < 0.8:
                ev[k] = rand_text(rng)
        if rng.random() < 0.1:
            ev.pop("kind")
        db = KV({} if pre is None else {bot_config.EVENTS_KEY: pre})
        now = 1_790_000_000.0 + rng.randint(0, 10 ** 6) + rng.choice([0.0, 0.5, 0.123456])
        out, err = attempt(lambda: run(bot_config.report(db, None, copy.deepcopy(ev), now=now)))
        written = [v for k, v in db.writes if k == bot_config.EVENTS_KEY]      # 没写库（事件类型不认识）= None
        w.record(fn="report_event", args=[pre, ev, now], out=written[-1] if written else None, err=err)

with Writer("flags/bot_snapshot") as w:
    rng = random.Random(9)
    NOW = 1_790_000_000.0
    STATUS_RAWS = [None, "", "{}", "[1]", "null", "garbage", '{"seen_at": 1789999900.0}', '{"seen_at": 1789999800}',
                   '{"seen_at": 1789999820.0}', '{"seen_at": 1789999819.9}', '{"seen_at": 0}', '{"seen_at": null}', '{"seen_at": ""}',
                   '{"seen_at": "x"}', '{"seen_at": [1]}', '{"seen_at": true}', '{"seen_at": false}', '{"seen_at": 1790000500}',
                   '{"user": "' + CAT + '", "seen_at": 1789999990.5, "online": false}', '{"online": 5, "seen_at": 1}',
                   '{"seen_at": 1789999990.5, "ai_ready": true, "version": "1"}']
    for _ in range(700):
        sr = rng.choice(STATUS_RAWS)
        er = rng.choice(EVENTS_PRE + ['{"a": 1, "b": 2}', '"xyz"', "{}", "[]"])
        st = stored_map(rng)
        kv = dict(st)
        if sr is not None:
            kv[bot_config.STATUS_KEY] = sr
        if er is not None:
            kv[bot_config.EVENTS_KEY] = er
        out, err = attempt(lambda: run(bot_config.snapshot(KV(kv), NOW)))
        w.record(fn="snapshot", args=[kv, NOW], out=out, err=err)
