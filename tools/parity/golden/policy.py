"""app/policy.py 的 golden 夹具：crates/gate-core/tests/golden/policy/*.jsonl（供 gate_core::policy 对拍）。

覆盖：手写边界值 + 每个函数 1~2k 条固定种子的随机变异（真实形态的 V3 / V4 / V4.5 / V5 出图请求体）。
原地修改入参的函数记录 `after`；依赖随机的函数把随机源打桩成固定值。
"""
from __future__ import annotations

import copy
import random
import re
import secrets
import unicodedata

from _golden import Writer
from _payloads import (ALL_MODELS, GIF, JPEG, ODD_MODELS, PNG, PNG2, V3_MODELS, V4_MODELS, V5_MODELS, WEBP, b64,
                       base_body, hex64, long_prompts, mutate, rand_b64, rand_prompt, ref_body, unit_pool, uuid4,
                       weird_value)

from app import policy

rng = random.Random(20261014)
N = 1500          # 每个「请求体 → 结果」函数的随机条数（> 256 KB 的夹具会被 _golden 自动 gzip）

FIXED_RANDINT = 2718281828
_real_randint = random.randint


def _fake_randint(a, b):
    assert (a, b) == (0, 4294967295), (a, b)
    return FIXED_RANDINT


random.randint = _fake_randint   # normalize_image_request 在函数体里 `import random; random.randint(...)`


def compact(model, **params) -> dict:
    """体积最小的请求体（系统网格用）。"""
    return {"input": "x", "model": model, "action": "generate", "parameters": params}


def checked(fn):
    """包一层：断言被测函数没有原地修改入参（clamp/fill/medium/guard 全部是「浅拷贝后改副本」），
    这样夹具里就不必再存一份 after，Rust 侧 `&Value` 入参即保证同样的语义。"""
    def wrapper(payload, *args, **kwargs):
        before = copy.deepcopy(payload)
        try:
            return fn(payload, *args, **kwargs)
        finally:
            assert payload == before, f"{fn.__name__} 修改了入参"
    wrapper.__name__ = fn.__name__
    return wrapper


clamp_image_params = checked(policy.clamp_image_params)
fill_v4_prompt = checked(policy.fill_v4_prompt)
medium_normalize = checked(policy.medium_normalize)
guard_image_action = checked(policy.guard_image_action)
clamp_text_params = checked(policy.clamp_text_params)


def body_cases(n: int, *, extra=None, ref=False) -> list:
    """n 个请求体：真实形态 + 变异 + 少量非 dict 的 parameters。"""
    out = []
    for i in range(n):
        b = ref_body(rng) if ref else base_body(rng)
        r = rng.random()
        if r < 0.65:
            b = mutate(rng, b)
        elif r < 0.75:
            b = mutate(rng, b, 4)
        out.append(b)
    if extra:
        out.extend(extra)
    return out


# ------------------------------------------------------------------ 常量与字符类

with Writer("policy/constants") as w:
    for name in ("FREE_BURST", "FREE_REFILL_PER_HOUR", "V5_COST_MULTIPLIER", "LEGACY_IMAGE_MODEL_PREFIXES",
                 "LEGACY_IMAGE_MODEL_EXACT", "V5_NORMAL_PRESETS", "VIBE_RAW_MODELS", "VIBE_ENCODED_MODELS",
                 "PRECISE_REFERENCE_MODELS", "REFERENCE_LIMIT", "VIBE_ENCODING_ANLAS", "REFERENCE_FIELDS",
                 "INTEGER_IMAGE_PARAMS", "MAX_PROMPT_BYTES", "DIMENSION_STEP", "V4_V5_REJECTED_SAMPLERS",
                 "MAX_CHARACTERS", "V5_MEDIUM_OF", "V5_MEDIUM_STEPS", "V5_MEDIUM_SAMPLER", "V5_MEDIUM_UC_PRESET",
                 "V5_MEDIUM_UC", "TEXT_CONTEXT_TOKENS", "TEXT_MAX_GENERATION_TOKENS", "TEXT_INPUT_BUDGET_TOKENS"):
        w.record(name=name, value=getattr(policy, name))


def _ranges(pred, hi=0x110000):
    out, start = [], None
    for c in range(hi):
        ok = False if 0xD800 <= c <= 0xDFFF else pred(chr(c))
        if ok and start is None:
            start = c
        if not ok and start is not None:
            out.append([start, c - 1])
            start = None
    if start is not None:
        out.append([start, hi - 1])
    return out


with Writer("policy/charclass") as w:
    # Rust 侧用 `pyc::parse_int` / `pyc::is_space` 推导出同样的字符类，必须与 Python 3.12 的 re / str 方法逐码点一致
    w.record(name="re_s", ranges=_ranges(lambda c: re.match(r"\s", c) is not None))
    w.record(name="re_d", ranges=_ranges(lambda c: re.match(r"\d", c) is not None))
    w.record(name="isdigit", ranges=_ranges(lambda c: c.isdigit()))
    w.record(name="isdecimal", ranges=_ranges(lambda c: c.isdecimal()))
    w.record(name="unicode_version", value=unicodedata.unidata_version)

# ------------------------------------------------------------------ 小工具

with Writer("policy/free_paced_cap") as w:
    for v in [-5, -1, 0, 1, 2, 10, 29, 30, 31, 50, 95, 96, 97, 100, 125, 126, 127, 128, 200, 1000, 10 ** 9, 2 ** 62,
              2 ** 63 - 1]:
        w.call(policy.free_paced_cap, v)
    for _ in range(300):
        w.call(policy.free_paced_cap, rng.randint(-50, 400))

with Writer("policy/estimate_tokens") as w:
    for t in ["", "a", "ab", "abc", "abcd", "abcdefg", "abcdefgh", "猫", "猫头鹰", "猫" * 7, "猫" * 8, "😀" * 3,
              "a" * 35, "a" * 36, "a" * 3500, "é" * 5, "x y z", "\n", "ａｂｃ"]:
        w.call(policy.estimate_tokens, t)
    alphabet = "abc 猫头鹰１\n😀é,."
    for _ in range(600):
        w.call(policy.estimate_tokens, "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 80))))

with Writer("policy/estimate_text_tokens") as w:
    edges = [0x2E7F, 0x2E80, 0x9FFF, 0xA000, 0xABFF, 0xAC00, 0xD7AF, 0xD7B0, 0xF8FF, 0xF900, 0xFAFF, 0xFB00, 0xFEFF,
             0xFF00, 0xFFEF, 0xFFF0, 0x2FFF, 0x3000, 0x303F, 0x3040, 0x1F600, 0x20000, 0x41, 0x20]
    for e in edges:
        w.call(policy.estimate_text_tokens, chr(e))
        w.call(policy.estimate_text_tokens, chr(e) * 7 + "abc")
    for t in ["", "a" * 100, "猫" * 100, "猫a" * 50, "あいうえお", "한국어", "ｱｲｳ", "，。！", "a" * 3 + "猫" * 2]:
        w.call(policy.estimate_text_tokens, t)
    alphabet = ["a", "b", " ", "猫", "あ", "한", "，", "ｱ", "😀", "é", "\n", chr(0x3000), chr(0xFF10), chr(0xF900)]
    for _ in range(800):
        w.call(policy.estimate_text_tokens, "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 120))))

with Writer("policy/_truncate_middle") as w:
    marker_len = len("\n…（中间省略）…\n")
    for text in ["", "short", "猫" * 100, "猫" * 300, "猫" * 1000, "a" * 5000, ("猫头鹰 abc " * 400), "x" * 10000,
                 "猫" * 20000, "ab猫" * 3000, "😀" * 2000]:
        big = len(text) > 4000
        for budget in ((100, 7000) if big else (1, 10, 100, 256, 500, 1000, 7000, 8000)):
            w.call(policy._truncate_middle, text, budget)
        if not big:
            w.call(policy._truncate_middle, text, 300, head_ratio=0.5)
            w.call(policy._truncate_middle, text, 300, head_ratio=0.0)
            w.call(policy._truncate_middle, text, 300, head_ratio=1.0)
    for _ in range(300):
        alphabet = ["a", "b", " ", "猫", "头", "，", "😀", "\n", "é"]
        text = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 1500)))
        w.call(policy._truncate_middle, text, rng.choice([1, 50, 256, 400, 800, 1200, 2000, 7000]),
               head_ratio=rng.choice([0.35, 0.35, 0.1, 0.5, 0.9]))

# ------------------------------------------------------------------ base64 / unit

with Writer("policy/_base64_data") as w:
    fixed = [PNG, PNG2, JPEG, WEBP, GIF, "", "=", "QQ==", "QQ=", "QQ", "Q", "QUI=", "QUI==", "QUJD", "QUJD=", "QUJD====",
             "=QQ=", "QQ==QQ==", "QQ=Q", "AA=A", "A=", "AAAA", "////", "+/+/", "-_-_", "QUJD\n", " QUJD", "QUJD ",
             "é", "QQ==é", "data:image/png;base64," + PNG, None, 1, True, [], {}, 1.5, b"x".decode(), "A" * 100,
             "A" * 101, "A" * 102, "A" * 103, "QR==", "AA==", "QUI=\n", PNG + "=", PNG + "==", PNG[:-1], PNG[:-2],
             PNG + "A", b64(b"\x89PNG\r\n\x1a\n"), b64(b"\x89PNG\r\n\x1a"), b64(b"\xff\xd8\xff"), b64(b"\xff\xd8"),
             b64(b"RIFFabcdWEBP"), b64(b"RIFFabcdWEBX"), b64(b"RIFFabcdWEB"), b64(b"RIFFabcdWEBP" + b"x" * 5),
             b64(b"riffabcdWEBP"), b64(b"\x00"), b64(b"\x00" * 3)]
    for v in fixed:
        for kw in ({}, {"source_image": True}, {"png": True}, {"source_image": True, "png": True}):
            w.call(policy._base64_data, v, **kw)
    # 超长（> 25 MiB）字符串太大不入库，边界由 Rust 单元测试覆盖
    for _ in range(1500):
        v = rand_b64(rng)
        w.call(policy._base64_data, v, source_image=rng.random() < 0.5, png=rng.random() < 0.3)
    # 随机字节 + 魔数前缀，覆盖 PNG/JPEG/WEBP 的各种截断
    for _ in range(600):
        magic = rng.choice([b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff", b"RIFF", b"RIFF\x00\x00\x00\x00WEBP", b"", b"GIF8"])
        data = magic + bytes(rng.randrange(256) for _ in range(rng.randint(0, 14)))
        s = b64(data)
        if rng.random() < 0.3:
            s = s.rstrip("=")
        if rng.random() < 0.1:
            s = s + rng.choice(["=", "A", "\n"])
        w.call(policy._base64_data, s, source_image=rng.random() < 0.6, png=rng.random() < 0.2)

with Writer("policy/_unit_value") as w:
    vals = [0, 1, 0.0, 1.0, 0.5, 1e-300, 0.9999999999999999, 1.0000000000000002, -0.0, -1e-300, 2, -1, True, False,
            None, "0.5", "", [], [0], {}, 10 ** 15, -10 ** 15, 2 ** 63 - 1, 2 ** 64 - 1, -2 ** 63, 1e30, -1e30, 1e300]
    for v in vals:
        w.call(policy._unit_value, v)
    for _ in range(300):
        w.call(policy._unit_value, unit_pool(rng))
        w.call(policy._unit_value, rng.random() * rng.choice([1, 1, 1.1, 2]))

with Writer("policy/_reference_list") as w:
    for key, val in [("a", [1]), ("a", []), ("a", None), ("a", "x"), ("a", {}), ("a", 0), ("a", False), ("b", [1])]:
        w.call(policy._reference_list, {"a": val}, key)
    w.call(policy._reference_list, {}, "x")
    w.call(policy._reference_list, {"x": [1, 2]}, "x")

with Writer("policy/validate_vibe_encoding") as w:
    models = V4_MODELS + V3_MODELS + V5_MODELS + ODD_MODELS + [None, 1, True, [], {}]
    for _ in range(1500):
        b = {"image": rng.choice([PNG, PNG, JPEG, WEBP, GIF, "AAAA", rand_b64(rng)]),
             "model": rng.choice(models), "informationExtracted": unit_pool(rng)}
        r = rng.random()
        if r < 0.15:
            del b[rng.choice(list(b))]
        elif r < 0.3:
            b[rng.choice(["image", "model", "informationExtracted"])] = weird_value(rng)
        elif r < 0.4:
            b["extra"] = 1
        if rng.random() < 0.5:
            b["model"] = rng.choice(V4_MODELS)
        w.call(policy.validate_vibe_encoding, b)
    w.call(policy.validate_vibe_encoding, {})
    w.call(policy.validate_vibe_encoding, {"model": "nai-diffusion-4-5-full", "image": PNG, "informationExtracted": 0.7})

# ------------------------------------------------------------------ 参考图校验与计费

with Writer("policy/validate_image_references") as w:
    fixed = [{}, {"parameters": None}, {"parameters": []}, {"parameters": "x"}, {"parameters": {}},
             {"model": "nai-diffusion-3"}, {"model": None, "parameters": {}}, {"model": 5, "parameters": {}},
             {"model": ["nai-diffusion-4"], "parameters": {}}]
    for b in fixed:
        w.call(policy.validate_image_references, b)
    for b in body_cases(2200, ref=True):
        w.call(policy.validate_image_references, b)

with Writer("policy/reference_surcharge") as w:
    fixed = [{}, {"parameters": None}, {"parameters": []}, {"parameters": {}}, {"parameters": {"director_reference_images_cached": 5}},
             {"parameters": {"director_reference_images_cached": "abc"}},
             {"parameters": {"director_reference_images_cached": "猫猫猫"}},
             {"parameters": {"director_reference_images_cached": {"a": 1, "b": 2}}},
             {"parameters": {"director_reference_images_cached": True}},
             {"parameters": {"director_reference_images_cached": 0, "reference_image_multiple": 7}},
             {"parameters": {"reference_image_multiple": 1.5}},
             {"parameters": {"reference_image_multiple": "", "reference_image_multiple_cached": [1, 2]}},
             {"parameters": {"reference_image_multiple": [], "reference_image_multiple_cached": "xyz"}},
             {"parameters": {"reference_image_multiple_cached": False}},
             {"parameters": {"reference_image_multiple": {}, "reference_image_multiple_cached": {"k": 1}}},
             {"model": "nai-diffusion-4-5-full", "parameters": {"reference_image_multiple": [1] * 6}},
             {"model": " NAI-Diffusion-4-5-Full ", "parameters": {"reference_image_multiple": [1] * 9}},
             {"model": "nai-diffusion-3", "parameters": {"reference_image_multiple": [1] * 9}},
             {"model": None, "reference_image_multiple": [1] * 9}, {"reference_image_multiple": "abcdefghij"}]
    for b in fixed:
        w.call(policy.reference_surcharge, b)
    for b in body_cases(1300, ref=True):
        w.call(policy.reference_surcharge, b)

# ------------------------------------------------------------------ normalize_image_request

with Writer("policy/normalize_image_request") as w:
    fixed = [
        {}, {"model": None}, {"model": ""}, {"model": "  "}, {"model": 5}, {"model": ["a"]}, {"model": "NAI-Diffusion-4-5-Full "},
        {"model": " Σ ", "parameters": {}}, {"model": "İNAI", "parameters": {}}, {"model": "　K　"},
        {"model": "nai-diffusion-3"}, {"model": "nai-diffusion-3", "parameters": None},
        {"model": "nai-diffusion-3", "parameters": []}, {"model": "nai-diffusion-3", "parameters": "x"},
        {"model": "nai-diffusion-3", "parameters": {}}, {"input": "x", "model": "M", "parameters": {"width": 5.0}},
        {"model": "m", "parameters": {"width": 5.5}}, {"model": "m", "parameters": {"width": True}},
        {"model": "m", "parameters": {"width": "5"}}, {"model": "m", "parameters": {"width": -1}},
        {"model": "m", "parameters": {"width": 65536}}, {"model": "m", "parameters": {"width": 65537}},
        {"model": "m", "parameters": {"width": 65536.0}}, {"model": "m", "parameters": {"width": 65537.0}},
        {"model": "m", "parameters": {"width": -0.0, "height": 0.0}}, {"model": "m", "parameters": {"width": None}},
        {"model": "m", "parameters": {"width": 1e30}}, {"model": "m", "parameters": {"width": 1e19}},
        {"model": "m", "parameters": {"width": 2 ** 63}}, {"model": "m", "parameters": {"width": 2 ** 64 - 1}},
        {"model": "m", "parameters": {"width": [1]}}, {"model": "m", "parameters": {"width": {}}},
        {"model": "m", "parameters": {"width": 1.0, "height": 2.0, "steps": 3.0, "n_samples": 4.0}},
        {"model": "m", "parameters": {"width": 1.0, "height": "x", "steps": 3.0}},
        {"model": "m", "parameters": {"n_samples": 1.0, "width": 1.0, "height": 1.5}},
        {"model": "m", "parameters": {"seed": -1}}, {"model": "m", "parameters": {"seed": 0}},
        {"model": "m", "parameters": {"seed": 4294967295}}, {"model": "m", "parameters": {"seed": 4294967296}},
        {"model": "m", "parameters": {"seed": 5.0}}, {"model": "m", "parameters": {"seed": 5.5}},
        {"model": "m", "parameters": {"seed": -5.0}}, {"model": "m", "parameters": {"seed": 4294967296.0}},
        {"model": "m", "parameters": {"seed": 1e30}}, {"model": "m", "parameters": {"seed": True}},
        {"model": "m", "parameters": {"seed": False}}, {"model": "m", "parameters": {"seed": None}},
        {"model": "m", "parameters": {"seed": "7"}}, {"model": "m", "parameters": {"seed": [1]}},
        {"model": "m", "parameters": {"seed": {}}}, {"model": "m", "parameters": {"seed": 2 ** 63}},
        {"model": "m", "parameters": {"seed": -2 ** 63}}, {"model": "m", "parameters": {"seed": 2 ** 64 - 1}},
        {"model": "m", "parameters": {"seed": -0.0}}, {"model": "m", "parameters": {"a": 1, "seed": 7.0, "b": 2}},
        {"model": "m", "parameters": {"scale": 0}}, {"model": "m", "parameters": {"scale": 50}},
        {"model": "m", "parameters": {"scale": 50.0}}, {"model": "m", "parameters": {"scale": 50.0000001}},
        {"model": "m", "parameters": {"scale": 51}}, {"model": "m", "parameters": {"scale": -0.0}},
        {"model": "m", "parameters": {"scale": -0.1}}, {"model": "m", "parameters": {"scale": True}},
        {"model": "m", "parameters": {"scale": "5"}}, {"model": "m", "parameters": {"scale": None}},
        {"model": "m", "parameters": {"scale": []}}, {"model": "m", "parameters": {"scale": 1e30}},
        {"model": "m", "parameters": {"scale": 2 ** 64 - 1}}, {"model": "m", "parameters": {"scale": -2 ** 63}},
        {"model": "m", "parameters": {"width": 5.5, "scale": "x"}}, {"model": "m", "parameters": {"seed": -1, "scale": -1}},
    ]
    for b in fixed:
        w.call(policy.normalize_image_request, b, mutates=[0])
    for b in body_cases(N):
        w.call(policy.normalize_image_request, b, mutates=[0])

# ------------------------------------------------------------------ 提示词遍历 / 清洗 / 校验


def _prompt_texts(body):
    return [[path, text] for path, text in policy._prompt_texts(body)]


with Writer("policy/_prompt_texts") as w:
    for b in body_cases(1200):
        w.call(_prompt_texts, b)


def prompt_bodies(n: int) -> list:
    """提示词字段都带文本的请求体（V4 结构 / characterPrompts 里也放提示词），再变异。"""
    out = []
    for _ in range(n):
        b = base_body(rng)
        p = b["parameters"]
        if rng.random() < 0.7:
            b["input"] = rand_prompt(rng)
        if rng.random() < 0.7:
            p["prompt"] = rand_prompt(rng)
        if rng.random() < 0.7:
            p["negative_prompt"] = rand_prompt(rng)
        for sk in ("v4_prompt", "v4_negative_prompt"):
            s = p.get(sk)
            if isinstance(s, dict) and isinstance(s.get("caption"), dict):
                s["caption"]["base_caption"] = rand_prompt(rng)
                for c in s["caption"].get("char_captions") or []:
                    if isinstance(c, dict):
                        c["char_caption"] = rand_prompt(rng)
        for c in p.get("characterPrompts") or []:
            if isinstance(c, dict):
                c["prompt"], c["uc"] = rand_prompt(rng), rand_prompt(rng)
        out.append(mutate(rng, b, rng.choice([0, 0, 1, 2])))
    return out


with Writer("policy/fix_name_digit_weights") as w:
    for t in ["bm94199::", "bm94199::, 1girl", "x_12.5::", "a1::", "1.2::", "-3::", "bm94199:: bm9::", "bm94199 ::",
              "name12:: name34::", "ab12.::", "ab12.5.6::", "ａｂ12::", "bm١٢٣::", "bm１２３::", "_5::", "9bm5::", "bm5:::",
              "bm5::::", "bm\n5::", "bm5 ::", "bm5：：", ""]:
        w.call(policy.fix_name_digit_weights, {"input": t}, mutates=[0])
        w.call(policy.fix_name_digit_weights, {"parameters": {"prompt": t, "negative_prompt": t}}, mutates=[0])
    for b in prompt_bodies(N):
        w.call(policy.fix_name_digit_weights, b, mutates=[0])
    for lp in long_prompts()[7:9]:
        w.call(policy.fix_name_digit_weights, {"input": lp}, mutates=[0])

with Writer("policy/clean_prompt_syntax") as w:
    for t in ["", " ", ",", ", ,", " , ", "::", " :: ", "a,::,b", ",::,", "::,a", "a,::", "a, :: ,b",
              ",\n::\n,", "(a:1.2)", "(a:1.2),(b:0.8)", "(a b:c)", "(a:.5)", "(a:1.)", "((a:1))", "(a:1)(b:2)",
              "a,, b,,, c", "a ,b , c", " a , b ", "a,,,", ",,,a", "a\n,\nb", "a,　b", "a , b", "a\x1c,\x1db",
              "(a:١٢)", "(a:１.２)", "(a:1 .2)", "(:1)", "(a:1:2)", "(a:b)", "(a,b:1)", "(a\n:1)", "x ::: y", "a::,b::",
              "::,::", "a,::,::,b", ",", " ,", ", ", "   a   ", "\ta\t", ",a,", " , a , ", "1.2::a::", "tag, ::  ,  tag",
              "a, ::\n", "a,::\n,b", "a, ::\n\n"]:
        w.call(policy.clean_prompt_syntax, t)
    for _ in range(2200):
        w.call(policy.clean_prompt_syntax, rand_prompt(rng))
    for lp in long_prompts()[6:9]:
        w.call(policy.clean_prompt_syntax, lp)

with Writer("policy/sanitize_and_translate_prompts") as w:
    for b in prompt_bodies(N):
        w.call(policy.sanitize_and_translate_prompts, b, mutates=[0])
    for lp in long_prompts()[6:9]:
        w.call(policy.sanitize_and_translate_prompts, {"input": lp}, mutates=[0])

with Writer("policy/upstream_parameter_problem") as w:
    fixed = [{}, {"parameters": None}, {"input": 5}, {"input": "a" * 51199}, {"input": "a" * 51200}, {"input": "猫" * 17066},
             {"input": "猫" * 17067}, {"input": "1" * 20000 + "::"}, {"input": "x" + "9" * 50 + "::"}]
    for b in fixed:
        w.call(policy.upstream_parameter_problem, b)
    for lp in long_prompts():
        w.call(policy.upstream_parameter_problem, {"input": lp})
    for lp in long_prompts()[:3] + long_prompts()[4:5]:
        w.call(policy.upstream_parameter_problem, {"parameters": {"negative_prompt": lp}})
    for t in ["100::", "100.0::", "100.5::", "101::", "-101::", "+101::", "-100::", "99.99::", "1e3::", "0100::", "a101::",
              "a100.01::", "1.2.101::", "1-101::", "x-101::", "x+101::", ".101::", "５００::", "٥٠٠::", "500 ::", "500:",
              "a 500::b", "(500::)", "1.5::a::, 500::b::", "000000000000100.5::", "1" * 400 + "::", "-" + "1" * 400 + "::",
              "1" * 30 + ".5::", "0.00001::", "-0.5::", "+0.5::"]:
        w.call(policy.upstream_parameter_problem, {"input": t})
        w.call(policy.upstream_parameter_problem, {"parameters": {"characterPrompts": [{"prompt": "ok"}, {"uc": t}]}})
    for b in prompt_bodies(N):
        w.call(policy.upstream_parameter_problem, b)
    # 尺寸 / 角色数 / 采样器 / 调度器的组合
    for _ in range(N):
        b = mutate(rng, base_body(rng), rng.choice([1, 2, 3]))
        if rng.random() < 0.25 and isinstance(b.get("parameters"), dict):
            b["parameters"]["characterPrompts"] = [{"prompt": "x", "uc": ""} for _ in range(rng.choice([0, 1, 6, 7, 8]))]
        if rng.random() < 0.2 and isinstance(b.get("parameters"), dict):
            for sk in ("v4_prompt", "v4_negative_prompt"):
                b["parameters"][sk] = {"caption": {"base_caption": "x", "char_captions": [{"char_caption": "c"} for _ in range(rng.choice([0, 6, 7, 9]))]}}
        w.call(policy.upstream_parameter_problem, b)

# ------------------------------------------------------------------ 模型判定

with Writer("policy/is_v5_model") as w:
    for m in ALL_MODELS + ["", " ", "nai-diffusion-5-", "nai-v5-", "NAI-V5-FULL", "nai-diffusion-5\n", "nai-diffusion-4-5",
                           "nai-diffusion-50", "nai-v50", "NAI-DIFFUSION-5", "nai-diffusion-5-full-medium", None, "　nai-v5　",
                           "ſ", "İ", "ǅ"]:
        w.call(policy.is_v5_model, m)
    for _ in range(300):
        w.call(policy.is_v5_model, rng.choice(ALL_MODELS) + rng.choice(["", "-", "-x", " ", "\n", "x"]))

with Writer("policy/image_model_tier") as w:
    for m in ALL_MODELS + ["", " ", None, "nai-diffusion-4-", "nai-diffusion-4-5-", "nai-diffusion-4-x", "nai-diffusion-3x",
                           "nai-diffusion-2-x", "nai-diffusion-furry-3", "nai-diffusion-furry-3-", "nai-diffusion-furry-4",
                           "nai-diffusion-furry", "nai-diffusion-furry-", "safe-diffusion-", "nai-diffusion-",
                           "NAI-DIFFUSION-3", "Safe-Diffusion", "nai-diffusion-4 ", " nai-diffusion-4"]:
        w.call(policy.image_model_tier, m)
    for _ in range(300):
        w.call(policy.image_model_tier, rng.choice(ALL_MODELS) + rng.choice(["", "-", "-x", " ", "\n", "x", "-inpainting"]))

# ------------------------------------------------------------------ 计费 / 免费档

with Writer("policy/_per_image_cost") as w:
    for width, height, steps in [(1024, 1024, 28), (832, 1216, 28), (512, 512, 20), (512, 512, 29), (64, 64, 1),
                                 (64, 64, 0), (0, 0, 0), (0, 5, 5), (-64, 64, 28), (64, -64, 28), (-64, -64, -28),
                                 (1024, 1024, -5), (65536, 65536, 65536), (4096, 4096, 50), (10 ** 6, 10 ** 6, 100),
                                 (1, 1, 1), (2 ** 20, 1, 28), (1216, 1856, 50), (2048, 512, 28), (832, 1216, 23)]:
        for smea, dyn in [(False, False), (True, False), (False, True), (True, True)]:
            w.call(policy._per_image_cost, width, height, steps, smea, dyn)
    for _ in range(700):
        w.call(policy._per_image_cost, rng.choice([64, 128, 512, 832, 1024, 1216, rng.randint(1, 4096)]),
               rng.choice([64, 128, 512, 832, 1024, 1216, rng.randint(1, 4096)]),
               rng.choice([1, 14, 23, 28, 29, 50, rng.randint(0, 100)]), rng.random() < 0.4, rng.random() < 0.4)

with Writer("policy/_has_paid_extras") as w:
    for b in body_cases(800, ref=True) + [{}, {"parameters": None}, {"parameters": []}, {"parameters": "x"},
                                           {"controlnet_model": "x"}, {"controlnet_condition": "x"},
                                           {"characterReferences": [1]}, {"parameters": {"characterReferences": [1]}},
                                           {"parameters": {"controlnet_model": ""}},
                                           {"parameters": {"director_reference_images_cached": [1]}},
                                           {"parameters": {"director_reference_images_cached": 3}}]:
        w.call(policy._has_paid_extras, b)


def eligible_cases(n: int) -> list:
    out = []
    for b in body_cases(n, ref=True):
        out.append(b)
    for b in body_cases(n // 2):
        out.append(b)
    for steps in (27, 28, 28.0, 28.5, 29, 0, None, "28", "29", " 29 ", "x", [], True, False, "", -5, 1e30, "1_0"):
        for dims in ((1024, 1024), (1024, 1025), (1025, 1024), (1, 1048576), (0, 5), (5, 0), (-5, -5), ("1024", "1024"),
                     (1024.9, 1024.9), (1024, "x"), (None, None), (True, True), (1e30, 1e30), (2 ** 63, 5)):
            out.append({"model": "nai-diffusion-4-5-full",
                        "parameters": {"steps": steps, "width": dims[0], "height": dims[1]}})
            out.append({"steps": steps, "width": dims[0], "height": dims[1]})
    out.extend([{}, {"parameters": None}, {"parameters": []}, {"parameters": {}}, {"parameters": {"width": [1]}},
                {"parameters": {"width": 5, "height": 5, "steps": {}}}, {"width": 5, "height": 5, "steps": [1]}])
    return out


with Writer("policy/opus_free_eligible") as w:
    for b in eligible_cases(900):
        w.call(policy.opus_free_eligible, b)
with Writer("policy/legacy_normal_free_eligible") as w:
    for b in eligible_cases(600):
        w.call(policy.legacy_normal_free_eligible, b)
with Writer("policy/v5_allowance_eligible") as w:
    for b in eligible_cases(450):
        w.call(policy.v5_allowance_eligible, b)

with Writer("policy/snap_v5_preset") as w:
    for width, height in [(1216, 832), (832, 1216), (1024, 1024), (1000, 900), (900, 1000), (990, 900), (1100, 1000),
                          (1101, 1000), (1100, 1001), (900, 1000), (810, 900), (809, 900), (1, 1), (0, 0), (0, 5), (5, 0),
                          (-5, 5), (5, -5), (-5, -5), (1, 0), (10, 0), (10 ** 9, 1), (1, 10 ** 9), (2 ** 40, 3), (3, 2 ** 40),
                          (1216, 1856), (1536, 1024), (64, 65536)]:
        w.call(policy.snap_v5_preset, width, height)
    for _ in range(600):
        w.call(policy.snap_v5_preset, rng.randint(-100, 5000), rng.randint(-100, 5000))
    for _ in range(300):
        h = rng.randint(1, 3000)
        w.call(policy.snap_v5_preset, int(h * rng.choice([1.1, 0.9, 1.1000001, 0.8999999, 1.0])) + rng.choice([-1, 0, 1]), h)

with Writer("policy/estimate_image_cost") as w:
    cases = body_cases(1800, ref=True) + body_cases(600)
    # 贴近线上的计费样本（官网实测值）
    for model in ("nai-diffusion-3", "nai-diffusion-4-5-full", "nai-diffusion-5", "nai-diffusion-5-full",
                  "nai-diffusion-4-5-full-inpainting", "nai-diffusion-5-full-inpainting"):
        for (width, height) in ((832, 1216), (1024, 1024), (512, 512), (1216, 1856), (512, 2048)):
            for steps in (20, 28, 29):
                for n in (1, 2, 3):
                    for extra in ({}, {"sm": True}, {"sm": True, "sm_dyn": True}, {"image": PNG, "strength": 0.6},
                                  {"image": PNG}, {"mask": PNG, "strength": 0.1},
                                  {"mask": PNG, "img2img": {"strength": 0.6}, "strength": 0.1},
                                  {"mask": PNG, "img2img": {"strength": 0.2}}, {"image": PNG, "strength": 0}):
                        cases.append(compact(model, width=width, height=height, steps=steps, n_samples=n, **extra))
    cases.extend([{}, {"parameters": None}, {"parameters": []}, {"model": "nai-diffusion-3"},
                  {"model": "nai-diffusion-3", "width": 1024, "height": 1024}])
    # 局部重绘 / 图生图的强度字段：img2img 非 dict、strength 非法、内外两个强度取较大者
    odd_img2img = [None, "x", [], [1], 5, 0, {}, {"strength": 0.3}, {"strength": 1}, {"strength": 0}, {"strength": True},
                   {"strength": None}, {"strength": "0.5"}, {"strength": 1.5}, {"strength": -0.1}, {"strength": 2 ** 63},
                   {"strength": 0.6, "color_correct": True}]
    for model in ("nai-diffusion-4-5-full-inpainting", "nai-diffusion-5-full-inpainting", "nai-diffusion-3-inpainting", "NAI-DIFFUSION-4-FULL"):
        for img2img in odd_img2img:
            for outer in (None, 0, 0.1, 0.9, 1, 1.0, True, "x", 2, -1):
                params = dict(width=832, height=1216, steps=28, mask=PNG, img2img=img2img)
                if outer is not None:
                    params["strength"] = outer
                cases.append(compact(model, **params))
        for strength in (None, 0, 0.1, 0.5, 1, 1.0, True, False, "x", 2, -1, 1.0000001, [], {}):
            params = dict(width=832, height=1216, steps=29, image=PNG)
            if strength is not None:
                params["strength"] = strength
            cases.append(compact(model, **params))
    skipped = 0
    for b in cases:
        kw = {}
        r = rng.random()
        if r < 0.3:
            kw["is_opus"] = False
        if rng.random() < 0.3:
            kw["v5_allowance_available"] = False
        try:
            probe = policy.estimate_image_cost(copy.deepcopy(b), **kw)
        except Exception:  # noqa: BLE001
            probe = None
        if probe is not None and probe["anlas"] >= 2 ** 62:
            # Python 是任意精度整数；天文数字（边长 ≥ 1e9）超出 JSON 夹具能无损表示的范围，
            # 线上 normalize_image_request 已把边长限在 65536 以内，这类输入走不到这里
            skipped += 1
            continue
        w.call(policy.estimate_image_cost, b, **kw)
    print("estimate_image_cost: 跳过天文数字用例", skipped)

with Writer("policy/fit_size") as w:
    for width, height, mp in [(1024, 1024, 1048576), (1025, 1024, 1048576), (2048, 2048, 1048576), (4096, 4096, 1048576),
                              (1216, 1856, 1048576), (64, 64, 4096), (65, 64, 4096), (64, 65536, 1048576),
                              (65536, 64, 1048576), (65536, 65536, 1048576), (100, 100, 4096), (1, 1, 1), (1, 1, 4096),
                              (0, 0, 4096), (0, 5, 4096), (5, 0, 4096), (0, 0, -1), (5, 0, -1), (-5, 5, 4096), (-5, -5, 4096),
                              (-5, -5, 20), (-5, 5, -100), (10, 10, -1), (10, 10, 0), (10, 10, 50), (3000, 2000, 1000000),
                              (832, 1216, 500000), (1536, 1024, 1048576), (1088, 1024, 1048576), (4097, 4096, 4096),
                              (100000, 100000, 4096), (100000, 1, 4096), (1, 100000, 4096), (10 ** 7, 10 ** 7, 1048576),
                              (64, 64, 4095), (64, 64, 4097), (128, 64, 4096), (128, 64, 8191), (70, 70, 4096)]:
        w.call(policy.fit_size, width, height, mp)
    # 银行家舍入：scale 取 1/2、1/4、3/4、1/8、5/8 这类精确可表示的值，width*scale/64 会恰好落在 x.5 上
    ties = 0
    for num, den in ((1, 2), (1, 4), (3, 4), (1, 8), (3, 8), (5, 8), (7, 8), (1, 3), (2, 3)):
        for _ in range(60):
            width = 64 * rng.randint(1, 80) * (1 if den != 3 else 3)
            height = 64 * rng.randint(1, 80) * (1 if den != 3 else 3)
            mp = width * height * num * num // (den * den)
            if mp * den * den != width * height * num * num:
                continue
            x = width * (num / den) / 64
            ties += (abs(x - int(x)) == 0.5)
            w.call(policy.fit_size, width, height, mp)
    assert ties > 50, ties
    for _ in range(1800):
        width = rng.choice([64, 128, 512, 832, 1024, 1216, 2048, 4096, rng.randint(1, 20000)])
        height = rng.choice([64, 128, 512, 832, 1024, 1216, 2048, 4096, rng.randint(1, 20000)])
        mp = rng.choice([4096, 65536, 262144, 524288, 1048576, 1048576, 1048576, 2097152, rng.randint(4096, 3000000)])
        w.call(policy.fit_size, width, height, mp)

with Writer("policy/clamp_image_params") as w:
    cases = []
    for b in body_cases(N):
        cases.append(b)
    for model in ("nai-diffusion-3", "nai-diffusion-4-5-full", "nai-diffusion-5", "nai-diffusion-5-full", "nai-v5"):
        for (width, height) in ((832, 1216), (1216, 832), (1024, 1024), (1216, 1856), (1536, 1024), (896, 1152),
                                (2048, 512), (512, 768), (4096, 4096), (64, 64), (100, 100), (1000, 900), (900, 1000),
                                (1100, 1000), (0, 0), (-5, 10), ("1024", "1024"), (1024.5, 1024.5), (None, None)):
            for steps in (23, 28, 29, 50, 0, None, "30", 28.5):
                cases.append(compact(model, width=width, height=height, steps=steps, n_samples=rng.choice([1, 1, 2]),
                                     sm=rng.random() < 0.3))
    cases.extend([{}, {"model": "nai-diffusion-3"}, {"model": "nai-diffusion-3", "parameters": None},
                  {"model": "nai-diffusion-3", "parameters": []}, {"model": "nai-diffusion-3", "parameters": "x"},
                  {"model": "nai-diffusion-3", "parameters": {"sm_dyn": True, "a": 1}},
                  {"model": "nai-diffusion-3", "parameters": {"characterReferences": [1], "b": 2, "controlnet_model": ""}}])
    settings = [(1048576, 28, False), (1048576, 28, True), (1048576, 23, False), (4096, 28, True), (4095, 28, True),
                (1048576, 0, True), (1048576, -1, True), (65536, 28, True), (2097152, 50, True), (1048576, 28, False),
                (100, 28, True), (0, 28, True), (-5, 28, True)]
    for b in cases:
        mp, ms, ai = rng.choice(settings)
        w.call(clamp_image_params, b, max_pixels=mp, max_steps=ms, allow_img2img=ai)
    for mp, ms, ai in settings:
        w.call(clamp_image_params, {"model": "nai-diffusion-5", "parameters": {"width": 1216, "height": 1856,
               "steps": 50, "n_samples": 4, "sm": True, "image": PNG}},
               max_pixels=mp, max_steps=ms, allow_img2img=ai)

with Writer("policy/fill_v4_prompt") as w:
    cases = body_cases(N)
    for model in V4_MODELS + V3_MODELS + V5_MODELS + ["", None, 5, "NAI-DIFFUSION-4"]:
        for case in range(6):
            b = base_body(rng, model if isinstance(model, str) else "nai-diffusion-4")
            b["model"] = model
            p = b["parameters"]
            for k in ("v4_prompt", "v4_negative_prompt"):
                if rng.random() < 0.6:
                    p.pop(k, None)
            if case % 3 == 0:
                p["characterPrompts"] = [
                    {"prompt": "c1", "uc": "u1", "center": {"x": 0.1, "y": 0.9}}, {"prompt": 5, "uc": None},
                    "bad", {"prompt": ["a", 1], "uc": {"k": 1.5}, "center": {"x": "q"}}, {"prompt": "c3", "center": []},
                    {"prompt": "", "uc": "", "center": {}}, None, {"center": {"y": 0.3}}]
            if case % 4 == 1:
                b["input"] = rng.choice([5, 1.5, True, False, None, ["a", "b"], {"k": "v"}, "", " ", "猫", 0, [], {}, 1e22, 2 ** 63,
                                         ["猫", 1.0, None, True, {"a": [1, 2]}], "it's", "q\"q", "both ' and \"", "tab\there",
                                         "nl\nx", "bs\\x", "\x07bell", "é", "😀"])
            if case % 4 == 2:
                p["negative_prompt"] = rng.choice([5, None, ["x"], False, "", 2.5, {"k": None}])
            if case % 5 == 3:
                p["use_coords"] = rng.choice([True, False, None, 0, 1, "x", "", [], [1]])
            if case == 5:
                p["v4_prompt"] = rng.choice([None, [], "x", 5, {}, {"caption": 1}])
                p["v4_negative_prompt"] = rng.choice([None, [], "x", 5, {}, {"caption": 1}])
            cases.append(b)
    for b in cases:
        w.call(fill_v4_prompt, b)

with Writer("policy/is_v5_medium") as w:
    for m in ALL_MODELS + ["", None, " nai-diffusion-5-full-medium ", "NAI-DIFFUSION-5-FULL-MEDIUM-INPAINTING",
                           "nai-diffusion-5-full-medium-x", "nai-diffusion-5-medium"]:
        w.call(policy.is_v5_medium, m)

with Writer("policy/medium_normalize") as w:
    cases = body_cases(N)
    for case in range(300):
        b = base_body(rng, rng.choice(["nai-diffusion-5-full-medium", "nai-diffusion-5-full", "nai-diffusion-5-full-medium-inpainting"]))
        p = b["parameters"]
        p["steps"] = rng.choice([14, 14.0, 23, 28, None, "14", True, 0, 14.5])
        p["sampler"] = rng.choice(["k_euler_ancestral", "k_euler", None, 5, ""])
        p["cfg_rescale"] = rng.choice([0, 0.0, 0.2, None, "", "0", False, True, [], [0], {}, -0.0, 1e-9])
        p["negative_prompt"] = rng.choice([policy.V5_MEDIUM_UC, " " + policy.V5_MEDIUM_UC + " ", "", "  ", "lowres", None, 5,
                                           ["x"], [], 0, False, "猫", "　"])
        if rng.random() < 0.7:
            p["v4_negative_prompt"] = rng.choice([
                {"caption": {"base_caption": "x", "char_captions": [{"char_caption": "a", "centers": [{"x": 1, "y": 2}]},
                                                                   {"centers": []}, "s", None, 5]}, "legacy_uc": False},
                {"caption": {"base_caption": "x", "char_captions": []}}, {"caption": {"char_captions": None}},
                {"caption": []}, {"caption": None}, {}, {"legacy_uc": True}, [], "x", None, {"caption": {"char_captions": "x"}}])
        if rng.random() < 0.7:
            p["characterPrompts"] = rng.choice([
                [{"prompt": "a", "uc": "b"}, {"prompt": "c"}, "s", None, 1, {"uc": 5, "x": 1}], [], None, "x", {}, 5,
                [{"uc": "u", "prompt": "p", "center": {"x": 0.5, "y": 0.5}}]])
        cases.append(b)
    for b in cases:
        w.call(medium_normalize, b)

# ------------------------------------------------------------------ 文本钳制

with Writer("policy/clamp_text_params") as w:
    cases = []
    small = ["", "hello", "猫" * 100, "猫" * 260, "猫" * 300, "猫" * 1000, "猫头鹰 abc " * 100, "line\n" * 200, "😀" * 400,
             "a" * 900, "a" * 3000, "猫a" * 600, 5, None, ["a"], {"k": "猫" * 50}, True, 0, 1.5, "ab\ncd", "x y z " * 60]
    large = ["猫" * 7100, "猫" * 7200, "猫" * 9000, "a" * 24000, "a" * 25000, "猫头鹰 abc " * 800, "x" * 31000]
    for i in range(N):
        p = {}
        for k, pool in (("max_length", [None, 0, -1, 1, 10, 50, 100, 150, 249, 250, 251, 300, 1000, 100.0, 100.7, 0.0, -0.5, "100",
                                         " 100 ", "x", True, False, [], [1], {}, 1e30, 2 ** 40, ""]),
                        ("min_length", [None, 0, 1, 5, 10, 100, 300, 1.5, "3", True, False, [], [2], {}, "x", -3]),
                        ("temperature", [0.7, 1, "x", None])):
            if rng.random() < 0.8:
                p[k] = rng.choice(pool)
        text = rng.choice(large) if i % 70 == 0 else rng.choice(small)
        b = {"model": "kayra-v1", "input": text, "parameters": p}
        r = rng.random()
        if r < 0.1:
            del b["parameters"]
        elif r < 0.15:
            b["parameters"] = rng.choice([None, [], "x", 5])
        elif r < 0.2:
            del b["input"]
        cases.append(b)
    for b in cases:
        mot = rng.choice([1, 50, 150, 249, 250, 251, 300, 1000, 0, -5])
        mic = rng.choice([100, 1000, 5000, 30000, 100000, 100000, 100000, 0, -1])
        kw = {"max_output_tokens": mot, "max_input_chars": mic}
        if rng.random() < 0.7:
            kw["max_input_tokens"] = rng.choice([100, 256, 300, 500, 1000, 7000, 8192, 20000, 0, -5])
        w.call(clamp_text_params, b, **kw)

# ------------------------------------------------------------------ 图生图 / 局部重绘守卫

with Writer("policy/guard_image_action") as w:
    cases = body_cases(N)
    for action in ["infill", "img2img", "generate", "INFILL", " Infill ", "　img2img　", "", None, 5, True, [], "inpaint",
                   "infill\n", "ınfill"]:
        for params in [{}, {"mask": PNG}, {"mask": ""}, {"sm": True}, {"sm_dyn": True}, {"sm": False, "sm_dyn": False},
                       {"mask": PNG, "sm": True, "x": 1}, {"mask": PNG, "sm_dyn": 1, "sm": 0}, {"sm_dyn": "x", "b": 1}]:
            cases.append({"model": "nai-diffusion-4-5-full", "action": action, "parameters": dict(params)})
            cases.append({"action": action, "parameters": dict(params), "input": "x"})
        cases.append({"action": action})
        cases.append({"action": action, "parameters": None})
        cases.append({"action": action, "parameters": [1]})
    for b in cases:
        w.call(guard_image_action, b)

with Writer("policy/text_model_host") as w:
    for m in ["kayra-v1", "llama-3-erato-v1", "glm-4-6", "xialong-v1", "KAYRA", "Erato", "GLM", "XiaLong", "clio-v1",
              "euterpe-v2", "", None, "nai-glm-4-6", "text-davinci", "my-kayra-model", "kay-ra", "XIALONG ", " glm"]:
        w.call(policy.text_model_host, m, "https://text.example.test", "https://api.example.test")

with Writer("policy/mask_token") as w:
    for t in ["", "a", "abcdef", "abcdefg", "abcdefghij", "pst-abcdefghijklmnop", "猫头鹰猫头鹰猫头鹰猫头鹰", "短", "😀" * 12,
              "tok_" + "x" * 40, "123456", "1234567"]:
        w.call(policy.mask_token, t)
        for keep in (0, 1, 3, 4, 6, 7, 10, 20):
            w.call(policy.mask_token, t, keep)

# ------------------------------------------------------------------ gen_key（随机源打桩，逐字节对拍）

with Writer("policy/gen_key") as w:
    real_token_bytes = secrets.token_bytes
    for i in range(300):
        data = bytes(rng.randrange(256) for _ in range(24))
        if i < 4:
            data = [b"\x00" * 24, b"\xff" * 24, bytes(range(24)), bytes(range(232, 256))][i]
        secrets.token_bytes = lambda n=None, _d=data: _d
        prefix = rng.choice(["nai", "nai", "pst", "", "猫", "k-e-y", "NAI"])
        out = policy.gen_key(prefix)
        w.record(fn="gen_key", args=[prefix, data], out=out, err=None)
    secrets.token_bytes = real_token_bytes
    # 真随机：只记录格式
    for _ in range(5):
        k = policy.gen_key()
        assert re.fullmatch(r"nai-[A-Za-z0-9_-]{32}", k), k
