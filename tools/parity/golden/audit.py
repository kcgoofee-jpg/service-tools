"""app/audit.py 的 golden 夹具（合成数据，固定种子）。

输出到 crates/gate-server/tests/fixtures/（audit 属于 gate-server，不放 gate-core 的 golden 目录）：
  audit_prompts   —— prompt_texts / _captions / capture_prompts：逐字节对拍（含 Python 抛的异常类型和消息）
  audit_notice    —— audit_notice：逐字节对拍
  audit_settings  —— audit_flags / audit_image_days / audit_image_keep_hours / audit_image_cap_mb / audit_disclosure：
                     假库里的原始设置串 + 环境默认值 → 结果（或异常）
  audit_images    —— make_thumbnail / full_image（一条记录两个函数的结果）：只对「决定」——收不收、返回类型串、缩略图尺寸、几个采样点的颜色。
                     字节不要求一致（Pillow 与 image crate 的重采样 / JPEG 编码本来就不同）

用法（仓库根目录，Python 3.12，与线上一致）：
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/audit.py

注意：本文件里不要写 \\uXXXX 这样的转义（工具链会把它提前解码），非 ASCII 字符一律用 chr()。
图片夹具里的 payload 要么是 {"$b": base64}（直接给字节），要么是 {"recipe": ...}（超大 / 全零的 blob 现场造，避免夹具膨胀）：
  {"recipe": {"kind": "blob", "len": N, "head": "<base64>"}}                        head + 零填充到 N 字节
  {"recipe": {"kind": "zip", "method": "deflate" | "stored",
              "entries": [{"name": "a.png", "len": N, "head": "<base64>"}, ...]}}   每个条目 = head + 零填充到 N 字节
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import io
import random
import types
import warnings
import zipfile

import _golden
from _golden import Writer

_golden.OUT_DIR = _golden.ROOT / "crates" / "gate-server" / "tests" / "fixtures"

from PIL import Image

from app import audit

CAT = chr(0x732B)              # 猫
OWL = chr(0x1F989)             # 🦉（BMP 之外：UTF-16 里是两个码元，Python 切片按码点）
EACUTE = chr(0xE9)
COMBINING = "e" + chr(0x301)
NBSP = chr(0xA0)
LSEP = chr(0x2028)
PRIVATE = chr(0xE000)
UNASSIGNED = chr(0x378)
ZWJ = chr(0x200D)

LOOP = asyncio.new_event_loop()


def run(coro):
    return LOOP.run_until_complete(coro)


def attempt(fn, *args, **kwargs):
    try:
        return fn(*args, **kwargs), None
    except Exception as e:  # noqa: BLE001 — 异常本身就是要对拍的行为
        return None, {"type": type(e).__name__, "msg": str(e)}


MISSING = object()

# ============================================================ 提示词
SHORT_TEXTS = [
    "1girl, solo, sky", "masterpiece, best quality, very aesthetic", "a", " ", "  lead and trail  ", "\n", "line1\nline2\r\n",
    "tab\there", 'quote " and \' mixed', "single ' only", 'double " only', "back\\slash", "<b>html</b> & entity", "{curly}, [square], (paren)",
    "0", "false", "None", "null", "[]", "{}", "x" * 10,
    CAT * 3, CAT + "," + EACUTE + "," + COMBINING, OWL * 3, "a" + OWL + "b", NBSP + "nbsp" + NBSP, "line" + LSEP + "sep",
    PRIVATE + UNASSIGNED, "zero" + ZWJ + "joiner", "\x00nul", "\x1f\x7fcontrol", "del\x7f",
]
LONG_TEXTS = [
    "x" * 999, "x" * 1000, "x" * 1001, "x" * 1999, "x" * 2000, "x" * 2001, "y" * 3999, "y" * 4000, "y" * 4001, "z" * 9000,
    CAT * 1999, CAT * 2000, CAT * 2001, CAT * 4001, OWL * 1001, OWL * 2001, OWL * 4001, ("a" + OWL) * 1500, COMBINING * 2100,
]
NONSTR = [
    None, True, False, 0, 1, -1, 42, 2 ** 31, 2 ** 53 + 1, 2 ** 63, 0.0, -0.0, 0.5, 3.14, 1e16, 1e22, 1e-7, 1.5e300, 5e-324, 123456789.123456789,
    [], [1, "a", None], ["x", "y"], [CAT], [[1, [2]], {"k": None}], {}, {"a": 1}, {"k": [1, 2.5, "s", None, True, False]},
    {CAT: OWL, "q": "it's", "d": 'say "hi"', "b": "both ' and \""}, {"nested": {"deep": {"deeper": [1, {"x": 1e16}]}}},
    # 未分配码点（如 U+0378）不放进嵌套容器：gate-core 的 repr 近似把它当可打印，Python 会写成 \\u0378（见 pyext.rs 说明）
    ["tab\t", "nl\n", "nul\x00", NBSP, LSEP, PRIVATE, OWL, COMBINING, EACUTE, "\x7f"],
]
SEEDS = [0, 1, 7, 42, 1234567890, 2 ** 32 - 1, 2 ** 53, 2 ** 63 - 1, 2 ** 63, 2 ** 64 - 1, -1, -2 ** 31, 3.0, 0.5, "123", None, True, False, [], {}]
SAMPLERS = ["k_euler", "k_euler_ancestral", "k_dpmpp_2m", "k_dpmpp_sde", "ddim_v3", "", None, 5, True, ["k_euler"], {"s": 1}, CAT]
SCALES = [5, 5.0, 6.5, 0, 0.0, 10, 1e-3, 1e16, "5", None, False, True, [5]]
STEPS = [28, 23, 1, 50, 0, 28.0, "28", None, False, 2 ** 40]
DIMS = [832, 1216, 1024, 512, 64, 0, 1024.0, "1024", None, False, 65536, -1]
SCHEDULES = ["karras", "native", "exponential", "polyexponential", "", None, 3]
RESCALES = [0, 0.0, 0.5, 0.7, 1, 1.0, None, False, "0.5"]
BOOLS = [True, False, None, 0, 1, "true", "", [], {}]
ENUMS = [0, 1, 2, 3, 4, None, False, "heavy", ""]
SAMPLES = [1, 2, 4, 0, None, False, 1.0, "1"]

PARAM_POOLS = {
    "seed": SEEDS, "sampler": SAMPLERS, "steps": STEPS, "scale": SCALES, "width": DIMS, "height": DIMS,
    "noise_schedule": SCHEDULES, "cfg_rescale": RESCALES, "sm": BOOLS, "sm_dyn": BOOLS, "ucPreset": ENUMS,
    "qualityToggle": BOOLS, "n_samples": SAMPLES,
}
# 不在 extra.params 白名单里的键：不许漏进去
OTHER_PARAMS = {
    "legacy_v3_extend": True, "image": "not-a-real-image", "reference_image_multiple": ["aaa"], "controlnet_strength": 1.0,
    "add_original_image": False, "autoSmea": False, "dynamic_thresholding": True, "skip_cfg_above_sigma": None, "noise": 0.0,
    "extra_noise_seed": 5, "use_coords": False, "legacy": True, "prefer_brownian": True,
}


def rtext(rng):
    r = rng.random()
    if r < 0.55:
        return rng.choice(SHORT_TEXTS)
    if r < 0.8:
        return ", ".join(rng.choice(SHORT_TEXTS) for _ in range(rng.randint(2, 6)))
    if r < 0.9:
        return rng.choice(LONG_TEXTS)
    return rng.choice(NONSTR)


def rtext_or_missing(rng, p_missing=0.15):
    """文本字段：缺失 / 空 / None / 非字符串 / 普通文本。返回 MISSING 表示不写这个键。"""
    r = rng.random()
    if r < p_missing:
        return MISSING
    if r < p_missing + 0.08:
        return rng.choice(["", None, 0, False, [], {}])
    return rtext(rng)


def put(d, k, v):
    if v is not MISSING:
        d[k] = v


def rchar_captions(rng):
    r = rng.random()
    if r < 0.07:
        return MISSING
    if r < 0.14:
        return rng.choice([None, [], {}, "", 0, False])
    if r < 0.19:                                    # 真值但不是列表
        return rng.choice(["abc", {"a": 1}, {"char_caption": "x"}, 5, 1.5, True, -1])
    n = rng.choice([0, 1, 1, 2, 2, 3, 5, 12, 13, 15, 20])
    items = []
    for _ in range(n):
        q = rng.random()
        if q < 0.65:
            c = {}
            put(c, "char_caption", rtext_or_missing(rng, 0.1))
            if rng.random() < 0.7:
                c["centers"] = [{"x": rng.choice([0.1, 0.5, 0.9]), "y": rng.choice([0.1, 0.5, 0.9])}]
            items.append(c)
        elif q < 0.75:
            items.append({})
        else:
            items.append(rng.choice([None, "text", 5, ["a"], True, 1.5]))
    return items


def rnode(rng, base_weight=0.6):
    """v4_prompt / v4_negative_prompt 的值。"""
    r = rng.random()
    if r < 0.12:
        return MISSING
    if r < 0.2:
        return rng.choice([None, "", 0, False, [], "str", ["x"], 5, True, {}])
    node = {}
    q = rng.random()
    if q < 0.82:
        caption = {}
        put(caption, "base_caption", rtext_or_missing(rng, 1 - base_weight))
        put(caption, "char_captions", rchar_captions(rng))
        node["caption"] = caption
    elif q < 0.9:
        node["caption"] = rng.choice([None, {}, "", 0, False, []])
    else:                                           # 真值但不是 dict：.get 抛 AttributeError
        node["caption"] = rng.choice(["caption text", ["x"], 5, 1.5, True, [{}]])
    if rng.random() < 0.6:
        node["use_coords"] = rng.choice([True, False])
    if rng.random() < 0.6:
        node["use_order"] = rng.choice([True, False])
    return node


def rbody(rng):
    body = {}
    put(body, "input", rtext_or_missing(rng, 0.12))
    if rng.random() < 0.9:
        body["model"] = rng.choice(["nai-diffusion-4-5-full", "nai-diffusion-4-full", "nai-diffusion-3"])
    if rng.random() < 0.85:
        body["action"] = "generate"
    r = rng.random()
    if r < 0.03:
        return body
    if r < 0.07:
        body["parameters"] = rng.choice([None, "text", [], ["a"], 5, 0, False, True, {}])
        return body
    p = {}
    put(p, "negative_prompt", rtext_or_missing(rng, 0.2))
    put(p, "v4_prompt", rnode(rng))
    put(p, "v4_negative_prompt", rnode(rng, 0.5))
    for k, pool in PARAM_POOLS.items():
        if rng.random() < 0.55:
            p[k] = rng.choice(pool)
    for k in rng.sample(sorted(OTHER_PARAMS), rng.randint(0, 4)):
        p[k] = OTHER_PARAMS[k]
    keys = list(p)
    rng.shuffle(keys)                               # 键的插入顺序不影响 extra.params 的顺序（按白名单顺序）
    body["parameters"] = {k: p[k] for k in keys}
    return body


def curated_bodies():
    v4 = lambda base, chars=None: {"caption": {"base_caption": base, "char_captions": chars if chars is not None else []}}
    ch = lambda text: {"char_caption": text, "centers": [{"x": 0.5, "y": 0.5}]}
    out = [
        {},
        {"input": ""},
        {"input": "1girl, solo", "parameters": {}},
        {"input": "1girl", "parameters": {"negative_prompt": "lowres, bad anatomy"}},
        {"parameters": {"v4_prompt": v4("two girls, park", [ch("girl A, red hair"), ch("girl B, blue hair")]),
                        "v4_negative_prompt": v4("lowres", [ch("bad hands"), ch("")])}},
        {"input": "from input", "parameters": {"v4_prompt": v4("from v4"), "v4_negative_prompt": v4("neg v4")}},
        {"input": "", "parameters": {"negative_prompt": "", "v4_prompt": v4("only v4 base"), "v4_negative_prompt": v4("only v4 neg")}},
        {"parameters": {"v4_prompt": v4("x" * 3000), "v4_negative_prompt": v4("n" * 2500)}},
        {"input": "i" * 2500, "parameters": {"negative_prompt": "n" * 1500, "v4_negative_prompt": v4("m" * 3000)}},
        {"input": "1girl", "parameters": {"seed": 0, "scale": 0, "sm": False, "sm_dyn": False, "qualityToggle": False}},
        {"input": "1girl", "parameters": {"seed": None, "sampler": None, "steps": None}},
        {"input": "1girl", "parameters": {"seed": 12345, "sampler": "k_euler_ancestral", "steps": 28, "scale": 5.0, "width": 832, "height": 1216,
                                           "noise_schedule": "karras", "cfg_rescale": 0.0, "sm": True, "sm_dyn": False, "ucPreset": 0,
                                           "qualityToggle": True, "n_samples": 1}},
        {"input": "1girl", "parameters": {"n_samples": 1, "qualityToggle": True, "ucPreset": 0, "sm_dyn": False, "sm": True, "cfg_rescale": 0.0,
                                           "noise_schedule": "karras", "height": 1216, "width": 832, "scale": 5.0, "steps": 28,
                                           "sampler": "k_euler_ancestral", "seed": 12345}},
        {"input": 12345},
        {"input": ["a", "b"]},
        {"input": {"k": "v"}},
        {"input": 0, "parameters": {"v4_prompt": v4("fallback")}},
        {"input": 0.0, "parameters": {"v4_prompt": v4(0)}},
        {"input": False, "parameters": {"negative_prompt": 0, "v4_negative_prompt": v4(["x"])}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": "oops"}}},                       # AttributeError（即便 input 非空）
        {"input": "", "parameters": {"v4_prompt": {"caption": ["x"]}}},
        {"input": "a", "parameters": {"v4_negative_prompt": {"caption": 5}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": 5}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": True}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": 1.5}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": "abc"}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": {"char_caption": "x"}}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": [None, "x", 5, {"char_caption": "ok"}]}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": [{"char_caption": 7}, {"char_caption": [1]}, {"char_caption": 0}, {"char_caption": {"a": 1}}]}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"base_caption": "b", "char_captions": [{"char_caption": "c%d" % i} for i in range(20)]}},
                                       "v4_negative_prompt": {"caption": {"char_captions": [{"char_caption": "n%d" % i} for i in range(13)]}}}},
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"char_captions": [{"char_caption": CAT * 600}] * 12}}}},     # extra 超过 4000 被截断
        {"input": "a", "parameters": {"v4_prompt": {"caption": {"char_captions": [{"char_caption": OWL * 700}] * 8}}}},
        {"input": "a", "parameters": {"seed": {"deep": [1, 2, {"x": OWL * 2500}]}, "sampler": [OWL * 3000]}},
        {"input": "a", "parameters": {"v4_prompt": None, "v4_negative_prompt": None}},
        {"input": "a", "parameters": {"v4_prompt": [], "v4_negative_prompt": "text"}},
        {"input": "a", "parameters": {"v4_prompt": {}, "v4_negative_prompt": {"caption": {}}}},
        {"input": "a", "parameters": "not a dict"},
        {"input": "a", "parameters": None},
        {"input": "a", "parameters": [{"seed": 1}]},
        {"input": "a", "parameters": {"scale": 1e16, "cfg_rescale": 1e-7, "steps": 28.0, "seed": 5e-324}},
        {"input": "a", "parameters": {"sampler": CAT, "noise_schedule": EACUTE, "seed": OWL}},
        {"input": 'q"uote', "parameters": {"negative_prompt": "back\\slash\n\ttab", "sampler": 'q"uote\\n\x01\x7f'}},
        {"input": "a", "extra_top": {"parameters": {"seed": 1}}},
    ]
    return out


with Writer("audit_prompts") as w:
    rng = random.Random(20261014)
    bodies = curated_bodies()
    # 系统性组合：input / negative_prompt / base_caption 的每一种取值
    for t in SHORT_TEXTS + LONG_TEXTS + NONSTR:
        bodies.append({"input": t})
        bodies.append({"input": "x", "parameters": {"negative_prompt": t}})
        bodies.append({"parameters": {"v4_prompt": {"caption": {"base_caption": t}}}})
        bodies.append({"parameters": {"v4_negative_prompt": {"caption": {"base_caption": t}}}})
        bodies.append({"input": "x", "parameters": {"v4_prompt": {"caption": {"char_captions": [{"char_caption": t}]}}}})
    for k, pool in PARAM_POOLS.items():
        for v in pool:
            bodies.append({"input": "x", "parameters": {k: v}})
    for _ in range(900):
        bodies.append(rbody(rng))
    for b in bodies:
        w.call(audit.capture_prompts, b)
    # 子函数单独对拍
    for b in bodies[:260] + [rbody(rng) for _ in range(160)]:
        w.call(audit.prompt_texts, b)
    nodes = [MISSING, None, "", 0, False, [], "str", {}, {"caption": None}, {"caption": {}}, {"caption": "x"}, {"caption": [1]}]
    nodes += [rnode(rng) for _ in range(160)]
    for n in nodes:
        if n is MISSING:
            w.call(audit._captions, None, note="node=None")
        else:
            w.call(audit._captions, n)
    # 非 dict 的 body（线上不会出现：请求体已校验为对象）；Python 抛 AttributeError
    for b in [None, "text", [], [1], 5, 1.5, True]:
        w.call(audit.capture_prompts, b)

# ============================================================ audit_notice
with Writer("audit_notice") as w:
    rng = random.Random(77)
    for prompts in (True, False):
        for thumbs in (True, False):
            for days in (0, 1, 7, 3650, -1):
                for image_days in (0, 2):
                    for keep_hours in (0, 1, 24, 47, 48, 49, 8760, -3):
                        w.call(audit.audit_notice, prompts, thumbs, days, image_days, keep_hours)
    for _ in range(300):
        w.call(audit.audit_notice, rng.random() < 0.6, rng.random() < 0.6, rng.choice([0, 1, 2, 7, 30, 365, 3650, -5, 10 ** 12]),
               rng.choice([0, 0, 1, 2, 3, 30, -1]), rng.choice([0, 1, 12, 24, 47, 48, 72, 168, 8760, -1, 10 ** 12]))
    # 旧版默认参数（image_days=0, keep_hours=24）：调用时省略，夹具里记成显式值
    w.record(fn="audit_notice", args=[True, True, 7, 0, 24], out=audit.audit_notice(True, True, 7), err=None, note="defaults")
    w.record(fn="audit_notice", args=[True, False, 0, 0, 24], out=audit.audit_notice(True, False, 0), err=None, note="defaults")


# ============================================================ 设置读取（假库）
class KV:
    """site_settings 的假库：get_setting 返回原始 TEXT，没有这一行返回调用方给的默认值（旧版 Database.get_setting）。"""

    def __init__(self, kv=None):
        self.kv = dict(kv or {})

    async def get_setting(self, key, default=None):
        return self.kv.get(key, default)


FLAG_RAWS = [MISSING, "1", "0", "true", "True", "TRUE", "false", " yes ", "YES", "on", "ON", "off", "no", "2", "-1", "", "  ", "\t1\n", "y", "t",
             "maybe", "truee", "ture", "0.0", "1.0", chr(0xFF11), NBSP + "1" + NBSP, "yes\n", "On ", CAT]
DAY_RAWS = [MISSING, "7", " 7 ", "0", "-5", "3650", "3651", "1", "30", "12.9", "-0.5", "1e2", "1E-1", "1_0", "1__0", "1e3", "99999999999999999999",
            "", "  ", "abc", "7d", "0x10", "nan", "NaN", "inf", "-inf", "Infinity", "1e999", "-1e999", ".5", "5.", "+8", "1,000", chr(0xFF11) + chr(0xFF12),
            chr(0x663), NBSP + "9" + NBSP, "3649.99", "3650.5", "-0.0"]
ENV_DAYS = [7, 0, 1, 30, 3650, 3651, -3, 10 ** 12, -10 ** 12, 2 ** 62]
HOUR_RAWS = [MISSING, "24", "0", "1", "8760", "8761", "-1", "48", " 12 ", "12.7", "1e3", "1e5", "abc", "", "  ", "nan", "inf", "-inf", "1_0", "9" * 30,
             "-0.0", chr(0xFF14) + chr(0xFF18)]
CAP_RAWS = [MISSING, "8192", "0", "1", "500000", "500001", "-1", "100", " 64 ", "10.9", "1e4", "abc", "", "nan", "inf", "-inf", "9" * 30]
DL_RAWS = [MISSING, "1", "0", "true", "false", "yes", "no", "on", "off", " ON ", "2", "", "  ", "maybe", "TRUE", "True", "FALSE", CAT]


def kvdict(prompts=MISSING, thumbs=MISSING, days=MISSING, hours=MISSING, cap=MISSING, dl=MISSING):
    kv = {}
    for key, v in (("audit_prompts", prompts), ("audit_thumbs", thumbs), ("audit_retention_days", days),
                   ("audit_image_keep_hours", hours), ("audit_image_cap_mb", cap), ("member_image_download", dl)):
        if v is not MISSING:
            kv[key] = v
    return kv


def settings_case(w, kv, env, only=None):
    db = KV(kv)
    st = types.SimpleNamespace(**env)
    fns = {
        "audit_flags": lambda: run(audit.audit_flags(db, st)),
        "audit_image_days": lambda: run(audit.audit_image_days(db)),
        "audit_image_keep_hours": lambda: run(audit.audit_image_keep_hours(db)),
        "audit_image_cap_mb": lambda: run(audit.audit_image_cap_mb(db)),
        "audit_disclosure": lambda: run(audit.audit_disclosure(db, st)),
    }
    for name, fn in fns.items():
        if only and name not in only:
            continue
        out, err = attempt(fn)
        w.record(fn=name, kv=kv, env=env, out=out, err=err)


with Writer("audit_settings") as w:
    rng = random.Random(31337)
    envs = [{"audit_prompts": a, "audit_thumbs": b, "audit_retention_days": d}
            for a in (False, True) for b in (False, True) for d in (7, 0, 3651)]
    # audit_flags：每个原始值 × 环境默认
    for raw in FLAG_RAWS:
        for env in envs[:8]:
            settings_case(w, kvdict(prompts=raw, thumbs=raw), env, only=("audit_flags",))
            settings_case(w, kvdict(prompts=raw), env, only=("audit_flags",))
            settings_case(w, kvdict(thumbs=raw), env, only=("audit_flags",))
    for raw in DAY_RAWS:
        for ed in ENV_DAYS[:3]:
            settings_case(w, kvdict(days=raw), {"audit_prompts": False, "audit_thumbs": False, "audit_retention_days": ed}, only=("audit_flags",))
    for ed in ENV_DAYS:
        settings_case(w, {}, {"audit_prompts": True, "audit_thumbs": True, "audit_retention_days": ed}, only=("audit_flags",))
    settings_case(w, {}, {"audit_prompts": False, "audit_thumbs": False}, only=("audit_flags",))       # settings 缺属性：getattr 默认值
    settings_case(w, {}, {}, only=("audit_flags",))
    # 三个整数 / 开关读取
    for raw in HOUR_RAWS:
        settings_case(w, kvdict(hours=raw), envs[0], only=("audit_image_keep_hours",))
    for raw in CAP_RAWS:
        settings_case(w, kvdict(cap=raw), envs[0], only=("audit_image_cap_mb",))
    for raw in DL_RAWS:
        settings_case(w, kvdict(dl=raw), envs[0], only=("audit_image_days",))
    # 整体披露文案：随机组合
    pick = lambda pool: rng.choice(pool)
    for _ in range(500):
        kv = kvdict(pick(FLAG_RAWS), pick(FLAG_RAWS), pick(DAY_RAWS), pick(HOUR_RAWS), pick(CAP_RAWS), pick(DL_RAWS))
        env = {"audit_prompts": rng.random() < 0.5, "audit_thumbs": rng.random() < 0.5, "audit_retention_days": pick(ENV_DAYS)}
        settings_case(w, kv, env, only=("audit_disclosure",))
    # 常见的「线上真实组合」
    for prompts in ("1", "0"):
        for thumbs in ("1", "0"):
            for days in ("7", "0", "30"):
                for hours in ("24", "0", "48", "72"):
                    for dl in ("1", "0"):
                        settings_case(w, kvdict(prompts, thumbs, days, hours, "8192", dl), envs[0], only=("audit_disclosure",))
    # 登记表键名与旧版常量一致
    w.record(fn="constants", out={"IMAGE_RETENTION_KEY": audit.IMAGE_RETENTION_KEY, "IMAGE_CAP_KEY": audit.IMAGE_CAP_KEY,
                                  "MEMBER_DOWNLOAD_DAYS": audit.MEMBER_DOWNLOAD_DAYS, "THUMB_SIDE": audit.THUMB_SIDE,
                                  "THUMB_QUALITY": audit.THUMB_QUALITY, "MAX_IMAGE_BYTES": audit.MAX_IMAGE_BYTES,
                                  "MAX_IMAGE_PIXELS": Image.MAX_IMAGE_PIXELS}, err=None)

# ============================================================ 图片：收不收 / 返回什么
RED, GREEN, BLUE, YELLOW = (220, 30, 30), (30, 200, 30), (30, 30, 220), (230, 220, 30)
SAMPLE_POINTS = ((0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75))


def quad(w, h, mode="RGB", colors=(RED, GREEN, BLUE, YELLOW)):
    """四个象限四种纯色：缩放 / 转换后在各象限中心采样，颜色基本不受重采样影响。"""
    im = Image.new("RGB", (w, h))
    hw, hh = max(w // 2, 1), max(h // 2, 1)
    im.paste(colors[0], (0, 0, hw, hh))
    im.paste(colors[1], (hw, 0, w, hh))
    im.paste(colors[2], (0, hh, hw, h))
    im.paste(colors[3], (hw, hh, w, h))
    return im.convert(mode) if mode != "RGB" else im


def bilevel(w, h):
    """1 位图：左半黑、右半白（不抖动，PNG 压得很小）。"""
    im = Image.new("1", (w, h), 1)
    im.paste(0, (0, 0, max(w // 2, 1), h))
    return im


def save(im, fmt, **kw):
    buf = io.BytesIO()
    im.save(buf, fmt, **kw)
    return buf.getvalue()


def b(data):
    return {"$b": base64.b64encode(data).decode()}


def summarize_thumb(out):
    if out is None:
        return None
    im = Image.open(io.BytesIO(out))
    im.load()
    w_, h_ = im.size
    samples = [list(im.getpixel((int(w_ * fx), int(h_ * fy)))) for fx, fy in SAMPLE_POINTS]
    return {"size": [w_, h_], "format": im.format, "mode": im.mode, "samples": samples}


def zinfo(name, method):
    """固定时间戳的 ZipInfo：zipfile 默认用当前时间，重复生成的夹具就不会逐字节一致。"""
    info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
    info.compress_type = method
    return info


def build(recipe):
    """recipe → bytes（Rust 测试里有同样的构造器）。"""
    kind = recipe["kind"]

    def blob(spec):
        head = base64.b64decode(spec.get("head", ""))
        return head + b"\x00" * (spec["len"] - len(head))
    if kind == "blob":
        return blob(recipe)
    buf = io.BytesIO()
    method = zipfile.ZIP_DEFLATED if recipe.get("method") == "deflate" else zipfile.ZIP_STORED
    with zipfile.ZipFile(buf, "w", method) as z:
        for e in recipe["entries"]:
            z.writestr(zinfo(e["name"], method), blob(e))
    return buf.getvalue()


IMAGE_CASES = []        # (name, payload bytes | recipe dict, extra record fields)


def case(name, payload, **extra):
    IMAGE_CASES.append((name, payload, extra))


# ---- PNG：尺寸（缩放规则 / 不放大 / 取整）
for (sw, sh) in [(1, 1), (2, 2), (10, 10), (100, 50), (767, 767), (768, 768), (769, 769), (768, 769), (769, 768), (768, 100), (100, 768),
                 (1000, 700), (700, 1000), (1000, 500), (500, 1000), (1600, 800), (3000, 100), (100, 3000), (2000, 1999),
                 (1, 2000), (2000, 1), (30000, 1), (1, 30000), (1023, 1025), (999, 1001), (1366, 768), (768, 1366)]:
    case(f"png-rgb-{sw}x{sh}", save(quad(sw, sh), "PNG"))

# ---- 像素上限（MAX_IMAGE_PIXELS = 25,000,000：恰好等于收，多一个像素就不收）
# at-limit 只留一张：Rust 测试在 debug 构建下解 2500 万像素要 5 秒多
case("png-1bit-5001x5000-over-limit", save(bilevel(5001, 5000), "PNG"))
case("png-1bit-25000x1000-at-limit", save(bilevel(25000, 1000), "PNG"))
case("png-1bit-25001x1000-over-limit", save(bilevel(25001, 1000), "PNG"))
case("png-1bit-10000x5001-over-2x-limit", save(bilevel(10000, 5001), "PNG"))

# ---- PNG：颜色模式
rgba = quad(200, 120).convert("RGBA")
rgba.putalpha(Image.new("L", rgba.size, 0).point(lambda v: 0))             # 整张全透明：Pillow 转 RGB 只丢 alpha，不合成背景
case("png-rgba-fully-transparent", save(rgba, "PNG"))
half = quad(200, 120).convert("RGBA")
half.putalpha(128)
case("png-rgba-half-alpha", save(half, "PNG"))
case("png-la", save(quad(200, 120, "L").convert("LA"), "PNG"))
case("png-l", save(quad(200, 120, "L"), "PNG"))
case("png-1bit", save(bilevel(200, 120), "PNG"))
pal = quad(200, 120).convert("P", palette=Image.ADAPTIVE, colors=8)
case("png-palette", save(pal, "PNG"))
pal_t = quad(200, 120).convert("P", palette=Image.ADAPTIVE, colors=8)
case("png-palette-transparency", save(pal_t, "PNG", transparency=0))
i16 = Image.new("I;16", (200, 120))
i16.paste(0, (0, 0, 100, 60))
i16.paste(21845, (100, 0, 200, 60))
i16.paste(43690, (0, 60, 100, 120))
i16.paste(65535, (100, 60, 200, 120))
case("png-gray-16bit", save(i16, "PNG"), nocolor="I;16 转 RGB：Pillow 把大于 255 的值截成 255，image crate 取高 8 位；上游只返回 8 位图")
png_exif = Image.Exif()
png_exif[0x0112] = 6
case("png-exif-orientation-ignored", save(quad(1600, 800), "PNG", exif=png_exif))
frames = [quad(300, 200), quad(300, 200, colors=(BLUE, YELLOW, RED, GREEN))]
case("png-apng-first-frame", save(frames[0], "PNG", save_all=True, append_images=frames[1:], duration=100, loop=0))

# ---- JPEG
case("jpeg-rgb-q95-optimize", save(quad(1600, 800), "JPEG", quality=95, optimize=True))
case("jpeg-rgb-small", save(quad(64, 48), "JPEG", quality=90))
case("jpeg-rgb-444", save(quad(800, 600), "JPEG", quality=90, subsampling=0))
case("jpeg-gray", save(quad(400, 300, "L"), "JPEG", quality=90))
case("jpeg-progressive", save(quad(900, 700), "JPEG", quality=90, progressive=True))
case("jpeg-cmyk", save(quad(300, 200, "CMYK"), "JPEG", quality=90), note="CMYK")
exif = Image.Exif()
exif[0x0112] = 6
case("jpeg-exif-orientation-6-ignored", save(quad(1600, 800), "JPEG", quality=90, exif=exif))

# ---- WebP
case("webp-lossless-rgb", save(quad(1000, 500), "WEBP", lossless=True))
case("webp-lossy-rgb", save(quad(1000, 500), "WEBP", quality=90))
webp_a = quad(300, 200).convert("RGBA")
webp_a.putalpha(200)
case("webp-lossless-rgba", save(webp_a, "WEBP", lossless=True))
wframes = [quad(300, 200), quad(300, 200, colors=(BLUE, YELLOW, RED, GREEN))]
case("webp-animated-first-frame", save(wframes[0], "WEBP", save_all=True, append_images=wframes[1:], duration=100, lossless=True), note="animated")

# ---- Pillow 能开、image crate（只开 png/jpeg/webp）不能开的格式：已知差异，Rust 侧断言「不收」
case("gif-deviation", save(quad(40, 20).convert("P"), "GIF"), deviation="gif")
case("bmp-deviation", save(quad(40, 20), "BMP"), deviation="bmp")

# ---- 损坏 / 截断 / 不是图片
good_png = save(quad(600, 400), "PNG")
good_jpg = save(quad(600, 400), "JPEG", quality=90)
good_webp = save(quad(600, 400), "WEBP", lossless=True)
flipped = bytearray(good_png)
flipped[len(flipped) // 2] ^= 0xFF
case("png-corrupt-middle-byte", bytes(flipped))
case("png-truncated-half", good_png[: len(good_png) // 2])
case("png-truncated-minus-20", good_png[:-20], deviation="png-trailer")
case("png-signature-only", b"\x89PNG\r\n\x1a\n")
case("png-header-only", good_png[:33])
case("jpeg-truncated-half", good_jpg[: len(good_jpg) // 2])
case("jpeg-truncated-90pct", good_jpg[: len(good_jpg) * 9 // 10])
case("jpeg-missing-eoi", good_jpg[:-2])
case("jpeg-soi-only", b"\xff\xd8\xff\xe0")
case("webp-truncated-half", good_webp[: len(good_webp) // 2])
case("webp-riff-header-only", good_webp[:12])
case("empty", b"")
case("one-byte", b"x")
case("ascii-text", b"hello, this is not an image\n" * 20)
case("random-1k", random.Random(5).randbytes(1024))
case("random-after-png-magic", b"\x89PNG\r\n\x1a\n" + random.Random(6).randbytes(512))
case("random-after-jpeg-magic", b"\xff\xd8\xff" + random.Random(7).randbytes(512))
case("riff-not-webp", b"RIFF" + b"\x10\x00\x00\x00" + b"WAVE" + b"fmt " + b"\x00" * 16)
case("pk-garbage", b"PK" + random.Random(8).randbytes(300))
case("pk-local-header-no-central-dir", b"PK\x03\x04" + b"\x14\x00\x00\x00\x00\x00" + random.Random(9).randbytes(100))
case("pk-only", b"PK")

# ---- zip：取第一张图（按后缀挑条目，按内容解码）
def mkzip(entries, method=zipfile.ZIP_DEFLATED, comment=b""):
    buf = io.BytesIO()
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")               # 重名条目 Python 会警告
        with zipfile.ZipFile(buf, "w", method) as z:
            for name, data in entries:
                z.writestr(zinfo(name, method), data)
            z.comment = comment
    return buf.getvalue()


png_red = save(quad(400, 300, colors=(RED, RED, RED, RED)), "PNG")
png_blue = save(quad(400, 300, colors=(BLUE, BLUE, BLUE, BLUE)), "PNG")
jpg_green = save(quad(400, 300, colors=(GREEN, GREEN, GREEN, GREEN)), "JPEG", quality=90)
webp_yellow = save(quad(400, 300, colors=(YELLOW, YELLOW, YELLOW, YELLOW)), "WEBP", lossless=True)
case("zip-one-png", mkzip([("image_0.png", png_red)]))
case("zip-one-png-stored", mkzip([("image_0.png", png_red)], zipfile.ZIP_STORED))
case("zip-two-png-first-wins", mkzip([("image_0.png", png_red), ("image_1.png", png_blue)]))
case("zip-text-then-png", mkzip([("readme.txt", b"hello"), ("image_0.png", png_red)]))
case("zip-only-text", mkzip([("readme.txt", b"hello"), ("meta.json", b"{}")]))
case("zip-empty-archive", mkzip([]))
case("zip-uppercase-ext", mkzip([("IMAGE.PNG", png_red)]))
case("zip-mixed-case-jpeg", mkzip([("pic.JpEg", jpg_green)]))
case("zip-subdir-name", mkzip([("sub/dir/a.png", png_red)]))
case("zip-dir-entry-then-png", mkzip([("folder.png/", b""), ("image_0.png", png_red)]))
case("zip-jpg-entry", mkzip([("a.jpg", jpg_green)]))
case("zip-webp-entry", mkzip([("a.webp", webp_yellow)]))
case("zip-ext-lies-jpg-is-png", mkzip([("a.jpg", png_red)]))
case("zip-gif-ext-ignored", mkzip([("a.gif", png_red), ("b.png", png_blue)]))
case("zip-first-entry-garbage-png", mkzip([("a.png", b"not an image at all"), ("b.png", png_blue)]))
case("zip-empty-png-entry", mkzip([("a.png", b"")]))
case("zip-garbage-png-entry", mkzip([("a.png", b"garbage" * 50)]))
case("zip-duplicate-names-last-wins", mkzip([("a.png", png_red), ("a.png", png_blue)]), note="Python getinfo 取同名的最后一条")
case("zip-duplicate-after-other", mkzip([("a.png", png_red), ("b.png", png_blue), ("a.png", png_blue)]))
case("zip-with-comment", mkzip([("image_0.png", png_red)], comment=b"synthetic comment"))
stored_bad = bytearray(mkzip([("image_0.png", png_red)], zipfile.ZIP_STORED))
at = bytes(stored_bad).find(png_red) + len(png_red) // 2
stored_bad[at] ^= 0xFF
case("zip-stored-bad-crc", bytes(stored_bad))
case("zip-truncated", mkzip([("image_0.png", png_red)])[:-30])
case("zip-bzip2-deviation", mkzip([("image_0.png", png_red)], zipfile.ZIP_BZIP2), deviation="bzip2")
case("zip-lzma-deviation", mkzip([("image_0.png", png_red)], zipfile.ZIP_LZMA), deviation="lzma")

# ---- 大小上限（MAX_IMAGE_BYTES = 15 MiB，等于收、多 1 字节不收；声明大小 > 上限的 zip 条目不读）
M15 = 15 * 1024 * 1024
case("blob-zeros-15MiB", {"recipe": {"kind": "blob", "len": M15}})
case("blob-zeros-15MiB-plus-1", {"recipe": {"kind": "blob", "len": M15 + 1}})
case("blob-png-magic-15MiB", {"recipe": {"kind": "blob", "len": M15, "head": base64.b64encode(b"\x89PNG\r\n\x1a\n").decode()}})
case("blob-png-magic-15MiB-plus-1", {"recipe": {"kind": "blob", "len": M15 + 1, "head": base64.b64encode(b"\x89PNG\r\n\x1a\n").decode()}})
case("blob-jpeg-magic-small", b"\xff\xd8\xff" + b"\x00" * 20)
case("blob-jpeg-magic-two-bytes", b"\xff\xd8")
case("blob-webp-magic-small", b"RIFF\x00\x00\x00\x00WEBPVP8 " + b"\x00" * 20)
case("blob-riff-short", b"RIFF\x00\x00\x00\x00WEB")
case("blob-png-magic-short", b"\x89PNG\r\n\x1a")
case("zip-entry-15MiB-deflate", {"recipe": {"kind": "zip", "method": "deflate", "entries": [{"name": "a.png", "len": M15}]}})
case("zip-entry-15MiB-plus-1-deflate", {"recipe": {"kind": "zip", "method": "deflate", "entries": [{"name": "a.png", "len": M15 + 1}]}})
case("zip-entry-15MiB-plus-1-then-small-png", {"recipe": {"kind": "zip", "method": "deflate", "entries": [{"name": "a.png", "len": M15 + 1}, {"name": "b.png", "len": 10}]}})
case("zip-entry-png-magic-15MiB-deflate", {"recipe": {"kind": "zip", "method": "deflate", "entries": [
    {"name": "a.png", "len": M15, "head": base64.b64encode(b"\x89PNG\r\n\x1a\n").decode()}]}})

with Writer("audit_images") as w:
    for name, payload, extra in IMAGE_CASES:
        if isinstance(payload, dict):
            data = build(payload["recipe"])
            enc_payload = payload
        else:
            data = payload
            enc_payload = b(payload)
        thumb, terr = attempt(audit.make_thumbnail, data)
        (img, ctype), ferr = attempt(audit.full_image, data)
        summary = None if img is None else {"len": len(img), "sha256": hashlib.sha256(img).hexdigest()}
        # 一条记录同时给两个函数的结果，payload 只存一份
        w.record(fn="image", name=name, payload=enc_payload, thumb=summarize_thumb(thumb), thumb_err=terr,
                 full=[summary, ctype], full_err=ferr, **extra)
