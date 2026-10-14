"""policy 夹具共用的合成请求体生成器（全部是合成数据：无真实 Key / ID / IP / 提示词）。

所有随机性都来自调用方传入的 `random.Random`，重复运行输出逐字节一致。
"""
from __future__ import annotations

import base64
import copy
import random
import struct
import zlib
from typing import Any


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


def png_bytes(w: int = 1, h: int = 1, rgb: tuple[int, int, int] = (0, 0, 0)) -> bytes:
    """用 zlib / struct 现拼一张合法的小 PNG（不依赖 Pillow）。"""
    def chunk(tag: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw))
            + chunk(b"IEND", b""))


PNG = b64(png_bytes(1, 1, (0, 0, 0)))
PNG2 = b64(png_bytes(2, 3, (255, 128, 0)))
JPEG = b64(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9")
WEBP = b64(b"RIFF" + struct.pack("<I", 20) + b"WEBP" + b"VP8L" + struct.pack("<I", 8) + b"\x2f\x00\x00\x00\x00\x00\x00\x00")
GIF = b64(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;")
RIFF_NOT_WEBP = b64(b"RIFF" + struct.pack("<I", 20) + b"WAVE" + b"fmt " + b"\x00" * 8)
# 编码后含 '+' '/' 的字节，用来造 urlsafe 变体
_PLUS_SLASH = bytes([0xFB, 0xEF, 0xBE, 0xFF, 0xFF, 0xFF, 0xFB, 0xFF])

HEX = "0123456789abcdef"


def hex64(rng: random.Random) -> str:
    return "".join(rng.choice(HEX) for _ in range(64))


def uuid4(rng: random.Random, upper: bool = False, version: str = "4", variant: str | None = None) -> str:
    h = lambda n: "".join(rng.choice(HEX) for _ in range(n))  # noqa: E731
    v = variant or rng.choice("89ab")
    s = f"{h(8)}-{h(4)}-{version}{h(3)}-{v}{h(3)}-{h(12)}"
    return s.upper() if upper else s


def rand_b64(rng: random.Random) -> Any:
    """各种形状的 base64 / 非 base64 值，覆盖 `_base64_data` 的每条分支。"""
    kind = rng.choice(["png", "png2", "jpeg", "webp", "gif", "riff", "rand", "png_trunc", "png_pad", "png_junk",
                       "png_ws", "empty", "bad_char", "urlsafe", "dataurl", "short", "nonstr", "unicode",
                       "pad_mid", "pad_many", "nul"])
    if kind == "png":
        return PNG
    if kind == "png2":
        return PNG2
    if kind == "jpeg":
        return JPEG
    if kind == "webp":
        return WEBP
    if kind == "gif":
        return GIF
    if kind == "riff":
        return RIFF_NOT_WEBP
    if kind == "rand":
        return b64(bytes(rng.randrange(256) for _ in range(rng.randint(1, 40))))
    if kind == "png_trunc":
        return PNG[: rng.randint(1, len(PNG) - 1)]
    if kind == "png_pad":
        return PNG + rng.choice(["=", "==", "===", "===="])
    if kind == "png_junk":
        return PNG + rng.choice(["!", "@@", "é", "-", "_", " x"])
    if kind == "png_ws":
        return rng.choice([" ", "\n", "\t", "\r\n"]) + PNG + rng.choice(["", " ", "\n"])
    if kind == "empty":
        return rng.choice(["", " ", "=", "==", "===="])
    if kind == "bad_char":
        s = list(PNG)
        s[rng.randrange(len(s))] = rng.choice("!@#$%^&*()[]{}<>?,.; -_\\")
        return "".join(s)
    if kind == "urlsafe":
        return base64.urlsafe_b64encode(_PLUS_SLASH + png_bytes(1, 1)).decode()
    if kind == "dataurl":
        return "data:image/png;base64," + PNG
    if kind == "short":
        return rng.choice(["A", "AA", "AAA", "AAAA", "QQ==", "QQ=", "QUI=", "QUI==", "Q===", "QQ=Q", "AA=A", "////"])
    if kind == "nonstr":
        return rng.choice([None, 0, 1, True, False, 1.5, [], [PNG], {}, {"data": PNG}])
    if kind == "unicode":
        return PNG[:20] + rng.choice(["猫", "é", " ", "　", "１"]) + PNG[20:]
    if kind == "pad_mid":
        return PNG[:8] + "==" + PNG[8:]
    if kind == "pad_many":
        return "QUJD" + "=" * rng.randint(1, 6)
    return b64(b"\x00" * rng.randint(1, 5))   # nul


# ----------------------------------------------------------------------------- 提示词

PROMPT_ATOMS = [
    "1girl", "solo", "smile", "masterpiece", "best quality", "very aesthetic", "absurdres", "artist:bm94199",
    "artist_a12", "name123.5", "year2026", "bm", "tag_9", "猫头鹰", "公益站", "少女", "蓝色眼睛", "location: park",
    "red hair", "blue eyes", "no humans", "lowres", "bad anatomy", "text", "watermark", "ＡＢＣ", "ｂｍ９４１９９",
]
SYNTAX_ATOMS = [
    "::", ",", ", ", " ", "(", ")", ":", "1.2", "0.8", "94199", "12", "3.5", "-3", "+2", "100", "101", "100.5",
    "_", "{", "}", "[", "]", "\n", "　", " ", "٣", "１", "１.２", ".5", "5.", "e3", "-", "+", ".",
    "1.2::", "-2::", "bm94199::", "x12::", "x1.5::", "(a:1.2)", "(tag:0.5)", "(a b:c)", "((x:1.1))",
    "（全角:1.2）", ",,", "，", " , ", "\t",
]
FIXED_PROMPTS = [
    "", " ", "1girl, solo, smile", "masterpiece, best quality, very aesthetic", "artist:bm94199::, 1girl",
    "1.2::red hair::, blue eyes", "(red hair:1.2), (blue eyes:0.8)", "(a b:c)", "(tag:1)", "(tag:.5)", "(tag:1.)",
    "((nested:1.1))", "(a:1.2)(b:1.3)", "{{{best quality}}}, [[lowres]]", "猫头鹰, 公益站, 少女",
    "bm94199::, 1.5::tag::", "tag1,, tag2,,,tag3", ", , ,", "::", ", :: ,", "a, ::, b", "a,::,b", "x::y",
    "artist_a12::tag", "name123.5::", "-3::tag::", "+2::tag::", "9999::foo::", "1e3::", "0.5::a::0.5::b::",
    "a 1.2:: b", "tag,,\n,tag2", "   ", "(tag:１.２)", "（全角括号:1.2）", "x\n::\ny", ",　::　,", " :: ",
    "::,a", "a,::", "a, :: ,b", "a ,, ,, b", ",a,", ", a ,", "a , b", "bm١٢٣::", "bm１２３::",
    "a1.5.5::", "a-1::", "1-5::", "-5::a", "a.5::", "foo100::bar", "5::", "1::2::3::", "x99999::", "1.2.3::",
    "tag:1.2", "(tag:1.2", "tag:1.2)", "()", "(:1)", "(a:b)", "(a:1:2)", "((a:1))", "(a:1)(b:2)", "(a:1) (b:2)",
    "a::b::c::", ":::", "::::", "a:::b", "k_euler_ancestral", "year 2026::", "z_9::", "z_9.9::", "_5::",
]


def rand_prompt(rng: random.Random) -> str:
    r = rng.random()
    if r < 0.35:
        return rng.choice(FIXED_PROMPTS)
    if r < 0.6:
        return ", ".join(rng.choice(PROMPT_ATOMS) for _ in range(rng.randint(1, 6)))
    n = rng.randint(0, 24)
    return "".join(rng.choice(SYNTAX_ATOMS + PROMPT_ATOMS) for _ in range(n))


def long_prompts() -> list[str]:
    return [
        "a" * 51199, "a" * 51200, "a" * 51201, "猫" * 17066 + "a", "猫" * 17067, "猫" * 17066 + "ab",
        "a, " * 17000, "1" * 40000 + "::", "x" + "1" * 40000 + "::", "x" + "1" * 300 + "::", "+" + "9" * 400 + "::",
        "1." + "1" * 500 + "::",
    ]


# ----------------------------------------------------------------------------- 请求体

V4_MODELS = ["nai-diffusion-4-5-full", "nai-diffusion-4-5-curated", "nai-diffusion-4-5-full-inpainting",
             "nai-diffusion-4-5-curated-inpainting", "nai-diffusion-4-5", "nai-diffusion-4-full",
             "nai-diffusion-4-curated-preview", "nai-diffusion-4-curated", "nai-diffusion-4",
             "nai-diffusion-4-full-inpainting", "nai-diffusion-4-curated-inpainting"]
V3_MODELS = ["nai-diffusion-3", "nai-diffusion-3-inpainting", "nai-diffusion-furry-3",
             "nai-diffusion-furry-3-inpainting", "nai-diffusion-2", "nai-diffusion", "safe-diffusion",
             "nai-diffusion-furry"]
V5_MODELS = ["nai-diffusion-5", "nai-diffusion-5-full", "nai-diffusion-5-curated", "nai-diffusion-5-full-medium",
             "nai-diffusion-5-full-inpainting", "nai-diffusion-5-full-medium-inpainting", "nai-v5", "nai-v5-full"]
ODD_MODELS = ["NAI-Diffusion-4-5-Full", " nai-diffusion-3 ", "nai-diffusion-6", "made-up", "", " ", "nai-diffusion-4-5-fullx",
              "nai-diffusion-45", "nai-diffusion-5x", "nai-v5x", "nai-diffusion-3-", "nai-diffusion-furry-3-x",
              "　nai-diffusion-5-full　", "NAI-V5", "İ", "Σ", "K", "nai-diffusion-4-5\n", "text-model", "kayra-v1"]
ALL_MODELS = V4_MODELS + V3_MODELS + V5_MODELS + ODD_MODELS

SIZES = [(832, 1216), (1216, 832), (1024, 1024), (512, 768), (768, 512), (640, 640), (896, 1152), (1152, 896),
         (960, 1024), (2048, 512), (512, 2048), (1088, 1024), (1216, 1856), (1536, 1024), (1472, 1472), (64, 64),
         (4096, 4096), (786, 786), (1000, 1000), (833, 1217), (100, 100), (65536, 64), (64, 65536), (1, 1)]
SAMPLERS = ["k_euler", "k_euler_ancestral", "k_dpmpp_2m", "k_dpmpp_2s_ancestral", "k_dpmpp_sde", "ddim_v3", "ddim",
            "plms", "k_dpm_adaptive", "k_dpm_fast", "k_dpmpp_3m_sde", "nai_smea", "nai_smea_dyn", "k_lms", "K_EULER",
            "", "_", "__a__", "k euler", "k-euler", "é", "x" * 64, "x" * 65, "k_euler\n", "１", 1, True, None, [], {}]
SCHEDULES = ["karras", "native", "exponential", "polyexponential", "", "é", "x" * 32, "x" * 33, 1, True, [], {}]
ACTIONS = ["generate", "img2img", "infill", "inpaint", "Generate", "INFILL", " infill ", "", "color", None, 1, True, []]


def weird_value(rng: random.Random) -> Any:
    pool = [None, True, False, 0, 1, -1, 2, 3, 6, 7, 14, 28, 29, 30, 50, 63, 64, 65, 100, 101, 128, 250, 251, 300,
            512, 832, 1000, 1024, 1088, 1216, 4096, 65536, 65537, 10 ** 9, 2 ** 31, 2 ** 32 - 1, 2 ** 32, 2 ** 63 - 1,
            2 ** 63, 2 ** 64 - 1, -2 ** 63, -5,
            0.0, -0.0, 0.1, 0.5, 0.75, 1.0, 1.5, 2.5, 28.0, 28.5, 28.99, 50.0, 832.0, 1e10, 1e15, 1e19, 1e30, -1.5,
            -0.001, 1.0000001, 0.9999999, 4294967295.0, 4294967296.0,
            "", " ", "abc", "28", " 28 ", "-28", "+28", "1_0", "1e3", "12.5", "0", "０", "１０２４", "832", "true",
            "null", "28.0", "١٢",
            [], [1], ["a"], [[]], {}, {"a": 1}, {"x": 0.1, "y": 0.9}, [None], [{}], [0.5], [1, 2]]
    return copy.deepcopy(rng.choice(pool))


def unit_pool(rng: random.Random) -> Any:
    return rng.choice([0, 1, 0.0, 1.0, 0.5, 0.3, 0.999, 0.0001, -0.1, 1.1, 2, -1, 1.0000001, True, False, None, "0.5",
                       "1", [], {}, 1e-300, 1e300, 10 ** 15, -2 ** 63, 2 ** 64 - 1])


def v4_structs(rng: random.Random, prompt: str, neg: str, chars: list[dict]) -> tuple[dict, dict]:
    cen = lambda c: [{"x": (c.get("center") or {}).get("x", 0.5), "y": (c.get("center") or {}).get("y", 0.5)}]  # noqa: E731
    pos = {"caption": {"base_caption": prompt,
                       "char_captions": [{"char_caption": c.get("prompt", ""), "centers": cen(c)} for c in chars]},
           "use_coords": rng.random() < 0.3, "use_order": True}
    ng = {"caption": {"base_caption": neg,
                      "char_captions": [{"char_caption": c.get("uc", ""), "centers": cen(c)} for c in chars]},
          "legacy_uc": False}
    return pos, ng


def base_body(rng: random.Random, model: str | None = None) -> dict:
    """贴近真实官方客户端的 V4 / V4.5 / V5 / V3 出图请求体（键顺序固定）。"""
    model = model or rng.choice(ALL_MODELS[:40])
    w, h = rng.choice(SIZES)
    prompt, neg = rand_prompt(rng), rand_prompt(rng)
    chars = []
    for _ in range(rng.choice([0, 0, 0, 1, 2, 7])):
        c = {"prompt": rand_prompt(rng), "uc": rand_prompt(rng),
             "center": {"x": rng.choice([0.1, 0.5, 0.9]), "y": rng.choice([0.1, 0.5, 0.9])}}
        if rng.random() < 0.3:
            c.pop("center")
        chars.append(c)
    p: dict[str, Any] = {
        "params_version": 3, "width": w, "height": h, "scale": rng.choice([5.0, 6, 3.5, 0, 50]),
        "sampler": rng.choice(["k_euler_ancestral", "k_euler", "k_dpmpp_2m"]),
        "steps": rng.choice([28, 23, 28, 29, 50, 14]), "n_samples": rng.choice([1, 1, 1, 2, 4]),
        "ucPreset": rng.choice([0, 1, 2, 3]), "qualityToggle": rng.random() < 0.5,
        "sm": rng.random() < 0.15, "sm_dyn": rng.random() < 0.1, "dynamic_thresholding": False,
        "controlnet_strength": 1, "legacy": False, "add_original_image": True, "cfg_rescale": rng.choice([0, 0.0, 0.2]),
        "noise_schedule": "karras", "legacy_v3_extend": False, "seed": rng.choice([0, 1, 123456789, -1, 4294967295]),
        "negative_prompt": neg, "prompt": prompt,
    }
    # 夹具体积：多数样本只保留必要键，约 1/4 保留完整官方形态
    if rng.random() > 0.25:
        for k in list(p):
            if k not in ("width", "height", "steps", "n_samples", "prompt", "negative_prompt", "seed", "sampler") \
                    and rng.random() < 0.65:
                del p[k]
    if rng.random() < 0.45:
        p["characterPrompts"] = chars
        p["use_coords"] = rng.random() < 0.3
    if model.startswith("nai-diffusion-4") or rng.random() < 0.3:
        if rng.random() < 0.55:
            p["v4_prompt"], p["v4_negative_prompt"] = v4_structs(rng, prompt, neg, chars)
    if rng.random() < 0.2:
        p["image"] = rng.choice([PNG, "AAAA", PNG2])
        p["strength"] = rng.choice([0.7, 1, 0.5, 0.0])
        p["noise"] = rng.choice([0, 0.1])
    if rng.random() < 0.12:
        p["mask"] = rng.choice([PNG, "AAAA"])
        p["img2img"] = rng.choice([{"strength": 0.6, "color_correct": True}, {"strength": 1.0}, {}, {"strength": 0.0}])
    body = {"input": prompt, "model": model, "action": rng.choice(["generate", "generate", "generate", "img2img", "infill"]),
            "parameters": p}
    if rng.random() < 0.05:
        del body["action"]
    return body


def _mutate_value(rng: random.Random, key: str) -> Any:
    """按键名挑「有意义」的替换值，其余用通用怪值。"""
    r = rng.random()
    if key in ("width", "height"):
        if r < 0.5:
            return rng.choice([64, 128, 256, 512, 576, 640, 704, 768, 832, 896, 960, 1024, 1088, 1152, 1216, 1536, 2048,
                               63, 65, 100, 786, 833, 1000, 1217, 4096, 8192, 65536, 0, -64])
    if key in ("steps", "n_samples"):
        if r < 0.5:
            return rng.choice([0, 1, 2, 4, 8, 14, 23, 27, 28, 29, 30, 50, 100, 28.0, 4.0, "28", -1])
    if key in ("sm", "sm_dyn"):
        if r < 0.6:
            return rng.choice([True, False, 1, 0, None, "x", [], [1]])
    if key in ("strength", "noise") and r < 0.6:
        return unit_pool(rng)
    if key == "sampler" and r < 0.7:
        return rng.choice(SAMPLERS)
    if key == "noise_schedule" and r < 0.7:
        return rng.choice(SCHEDULES)
    if key == "model" and r < 0.8:
        return rng.choice(ALL_MODELS)
    if key == "action" and r < 0.8:
        return rng.choice(ACTIONS)
    if key in ("input", "prompt", "negative_prompt", "base_caption", "char_caption") and r < 0.7:
        return rand_prompt(rng)
    if key in ("image", "mask") and r < 0.6:
        return rng.choice([PNG, "AAAA", "", None, 1, True])
    if key == "seed" and r < 0.6:
        return rng.choice([0, -1, 1, 4294967295, 4294967296, 5.0, 5.5, True, False, None, "7", [], 2 ** 40, -2 ** 40, 1e30])
    if key == "scale" and r < 0.6:
        return rng.choice([0, 5, 5.0, 50, 50.5, -0.5, 51, True, "5", None, 1e30, [], 0.0, -0.0])
    if key in ("cfg_rescale",) and r < 0.6:
        return rng.choice([0, 0.0, 0.2, 1, -0.1, True, "0", None, [], {}])
    if key in ("controlnet_model", "controlnet_condition", "characterReferences") and r < 0.7:
        return rng.choice(["canny", "x", 1, True, [], [{"image": PNG}], {}, {"a": 1}, None, 0, ""])
    return weird_value(rng)


PARAM_KEYS = ["width", "height", "scale", "sampler", "steps", "n_samples", "ucPreset", "sm", "sm_dyn", "cfg_rescale",
              "noise_schedule", "seed", "negative_prompt", "prompt", "characterPrompts", "use_coords", "v4_prompt",
              "v4_negative_prompt", "image", "strength", "noise", "mask", "img2img", "controlnet_model",
              "controlnet_condition", "characterReferences", "legacy", "extra_unknown"]
TOP_KEYS = ["input", "model", "action", "parameters", "image", "mask", "extra_unknown"]


def mutate(rng: random.Random, body: dict, ops: int | None = None) -> dict:
    """对请求体做 1~4 处随机变异（改值 / 删键 / 加键 / 换结构），返回新对象。"""
    body = copy.deepcopy(body)
    for _ in range(ops if ops is not None else rng.choice([1, 1, 2, 2, 3, 4])):
        r = rng.random()
        p = body.get("parameters")
        if r < 0.22 or not isinstance(p, dict):
            key = rng.choice(TOP_KEYS)
            op = rng.random()
            if key == "parameters" and op < 0.6:
                body[key] = rng.choice([None, [], "x", 1, {}, True, [1]])
            elif op < 0.15 and key in body:
                del body[key]
            else:
                body[key] = _mutate_value(rng, key)
        elif r < 0.7:
            key = rng.choice(PARAM_KEYS)
            op = rng.random()
            if op < 0.18 and key in p:
                del p[key]
            else:
                p[key] = _mutate_value(rng, key)
        elif r < 0.85:
            # V4 结构里的键
            for sk in ("v4_prompt", "v4_negative_prompt"):
                if rng.random() < 0.5:
                    s = p.get(sk)
                    if not isinstance(s, dict):
                        s = p[sk] = {"caption": {"base_caption": rand_prompt(rng), "char_captions": []}}
                    cap = s.get("caption")
                    op = rng.random()
                    if op < 0.2:
                        s["caption"] = rng.choice([None, [], "x", {}, 1])
                    elif isinstance(cap, dict):
                        if op < 0.5:
                            cap["base_caption"] = _mutate_value(rng, "base_caption")
                        elif op < 0.8:
                            cap["char_captions"] = rng.choice([
                                [], [{"char_caption": rand_prompt(rng), "centers": [{"x": 0.5, "y": 0.5}]}],
                                [{"char_caption": rand_prompt(rng)} for _ in range(rng.randint(1, 8))], None, "x", {},
                                [None, 1, "x", {"char_caption": rand_prompt(rng)}],
                                [{"char_caption": _mutate_value(rng, "char_caption")}]])
                        else:
                            cap.pop("base_caption", None)
        else:
            # characterPrompts 里的项
            cp = p.get("characterPrompts")
            if not isinstance(cp, list) or not cp:
                cp = p["characterPrompts"] = [{"prompt": rand_prompt(rng), "uc": rand_prompt(rng),
                                               "center": {"x": 0.5, "y": 0.5}}]
            i = rng.randrange(len(cp))
            op = rng.random()
            if op < 0.2:
                cp[i] = rng.choice([None, 1, "x", [], {}])
            elif op < 0.3:
                cp.append(copy.deepcopy(cp[i]))
            elif isinstance(cp[i], dict):
                k = rng.choice(["prompt", "uc", "center", "extra"])
                cp[i][k] = rng.choice([weird_value(rng), rand_prompt(rng), {"x": 0.2, "y": 0.8}, {"x": "a"}, {}])
    return body


# ----------------------------------------------------------------------------- 参考图（Vibe / 精确参考）

VIBE_RAW = ["nai-diffusion-3", "nai-diffusion-3-inpainting", "nai-diffusion-furry-3", "nai-diffusion-furry-3-inpainting"]
VIBE_ENC = ["nai-diffusion-4", "nai-diffusion-4-full", "nai-diffusion-4-full-inpainting", "nai-diffusion-4-curated",
            "nai-diffusion-4-curated-preview", "nai-diffusion-4-curated-inpainting", "nai-diffusion-4-5",
            "nai-diffusion-4-5-full", "nai-diffusion-4-5-full-inpainting", "nai-diffusion-4-5-curated",
            "nai-diffusion-4-5-curated-inpainting"]
PRECISE = [m for m in VIBE_ENC if m.startswith("nai-diffusion-4-5")]


def ref_body(rng: random.Random) -> dict:
    """参考图相关请求体：大多数结构自洽（能走到后面的分支），再由 mutate 少量破坏。"""
    kind = rng.choice(["none", "none", "vibe_raw", "vibe_enc", "vibe_enc_noinfo", "vibe_cached", "precise", "precise",
                       "mixed", "v5"])
    if kind in ("vibe_raw",):
        model = rng.choice(VIBE_RAW + ODD_MODELS[:3])
    elif kind.startswith("vibe_enc") or kind == "vibe_cached":
        model = rng.choice(VIBE_ENC + VIBE_RAW)
    elif kind == "precise":
        model = rng.choice(PRECISE + VIBE_ENC)
    elif kind == "v5":
        model = rng.choice(V5_MODELS)
    else:
        model = rng.choice(ALL_MODELS)
    body = base_body(rng, model)
    p = body["parameters"]
    for k in ("image", "mask", "img2img", "strength", "noise"):
        p.pop(k, None)
    n = rng.choice([1, 1, 2, 3, 4, 5, 6, 8, 16, 17])
    unit = lambda: rng.choice([0, 1, 0.5, 0.7, 0.35, 1.0, 0.0])  # noqa: E731
    flat = rng.random() < 0.12
    if kind in ("vibe_raw", "vibe_enc", "vibe_enc_noinfo", "mixed"):
        src = rng.choice([[PNG, JPEG, WEBP], [PNG], [PNG2, PNG], [JPEG]]) if kind == "vibe_raw" else \
            [rng.choice(["QUJDREVGR0hJSktM", "AAAA", PNG[:12], PNG[:12] + "AAAA", "QQ==", rand_b64(rng)]) for _ in range(4)]
        if rng.random() < 0.55 and kind != "vibe_raw":
            src = ["AAAAAAAAAAAAAAAA", "QUJDREVGR0hJSktM", "bm90LWFuLWltYWdl", "AAECAwQFBgcICQoL"]
        p["reference_image_multiple"] = [rng.choice(src) for _ in range(n)]
        if kind != "vibe_enc_noinfo":
            p["reference_information_extracted_multiple"] = [unit() for _ in range(n)]
        p["reference_strength_multiple"] = [unit() for _ in range(n)]
    if kind == "vibe_cached" or (kind == "mixed" and rng.random() < 0.5):
        keyf = lambda: rng.choice([hex64(rng), uuid4(rng), hex64(rng)])  # noqa: E731
        datas = [PNG, JPEG, WEBP, "AAAA", "QUJDREVGR0hJSktM", rand_b64(rng)]
        p["reference_image_multiple_cached"] = [{"cache_secret_key": keyf(), "data": rng.choice(datas)} for _ in range(n)]
        p["reference_information_extracted_multiple"] = [unit() for _ in range(n)]
        p["reference_strength_multiple"] = [unit() for _ in range(n)]
    if kind in ("precise", "mixed", "v5"):
        k = n if kind != "mixed" else rng.choice([1, 2])
        keyf = lambda: rng.choice([hex64(rng), hex64(rng), uuid4(rng)])  # noqa: E731
        p["director_reference_images_cached"] = [
            {"cache_secret_key": keyf(), "data": rng.choice([PNG, PNG, PNG2, JPEG, "AAAA", rand_b64(rng)])} for _ in range(k)]
        p["director_reference_descriptions"] = [
            {"caption": {"base_caption": rng.choice(["character", "style", "character&style", "character", "x"]),
                         "char_captions": []}, "legacy_uc": False} for _ in range(k)]
        p["director_reference_information_extracted"] = [unit() for _ in range(k)]
        p["director_reference_strength_values"] = [unit() for _ in range(k)]
        p["director_reference_secondary_strength_values"] = [unit() for _ in range(k)]
    r = rng.random()
    if r < 0.45:
        body = mutate(rng, body, rng.choice([1, 1, 2]))
    elif r < 0.7:
        body = mutate_refs(rng, body)
    if flat:
        flat_p = dict(body.get("parameters") if isinstance(body.get("parameters"), dict) else {})
        flat_p["model"] = body.get("model")
        flat_p["input"] = body.get("input")
        flat_p["action"] = body.get("action")
        return flat_p
    return body


REF_KEYS = ["reference_image_multiple", "reference_image_multiple_cached", "reference_information_extracted_multiple",
            "reference_strength_multiple", "director_reference_images_cached", "director_reference_descriptions",
            "director_reference_information_extracted", "director_reference_strength_values",
            "director_reference_secondary_strength_values", "director_reference_images", "characterReferences",
            "reference_image"]


def mutate_refs(rng: random.Random, body: dict) -> dict:
    body = copy.deepcopy(body)
    p = body.get("parameters")
    if not isinstance(p, dict):
        return body
    for _ in range(rng.choice([1, 1, 2, 3])):
        key = rng.choice(REF_KEYS)
        v = p.get(key)
        op = rng.random()
        if op < 0.12 and key in p:
            del p[key]
        elif op < 0.25:
            p[key] = rng.choice([None, "x", 1, True, {}, [], "", 0, [None], [[]], ["x"]])
        elif isinstance(v, list) and v:
            i = rng.randrange(len(v))
            sub = rng.random()
            if sub < 0.25:
                v.pop(i)
            elif sub < 0.4:
                v.append(copy.deepcopy(v[i]))
            elif sub < 0.6:
                v[i] = unit_pool(rng) if key.endswith(("multiple", "values", "extracted")) else weird_value(rng)
            elif isinstance(v[i], dict):
                k2 = rng.choice(list(v[i].keys()) + ["extra"])
                if v[i].get("caption") is not None and rng.random() < 0.5:
                    cap = v[i]["caption"]
                    if isinstance(cap, dict):
                        cap[rng.choice(["base_caption", "char_captions", "extra"])] = rng.choice(
                            ["character", "style", "character&style", "Character", [], [1], None, 1, "x"])
                else:
                    v[i][k2] = rng.choice([hex64(rng), uuid4(rng), uuid4(rng, upper=True), uuid4(rng, version="1"),
                                           uuid4(rng, variant="c"), "A" * 64, hex64(rng).upper(), hex64(rng)[:63],
                                           hex64(rng) + "0", rand_b64(rng), None, 1, [], False, True,
                                           hex64(rng) + "\n", uuid4(rng) + "\n"])
            else:
                v[i] = rng.choice([None, 1, "x", [], {}, {"cache_secret_key": hex64(rng), "data": PNG}, PNG, rand_b64(rng)])
        else:
            p[key] = [weird_value(rng) for _ in range(rng.randint(0, 3))]
    return body
