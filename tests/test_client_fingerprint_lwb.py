"""客户端指纹：认出小白盒（LittleWhiteBox）插件的请求 + 流式格式容错。

为什么需要：小白盒「前端直连」时用的是宿主浏览器 UA，和柏宝绘、智绘姬这些完全一样，
线上 7 天的 client 分布里绝大多数就是裸浏览器 UA，认不出来。它的请求体里有自己的字段
（ucPresetId / qualityPresetId，官方 SDK 用的是 ucPreset / qualityToggle），据此可以标记。

参考它自己的源码（modules/draw/providers/novelai/novel-v5-request.js）：
parameters.stream 固定是字符串 'msgpack'；顶层带 use_new_shared_trial: true；
parameters 里有 params_version: 4 / characterPrompts / v4_prompt / v4_negative_prompt。
"""
import pytest

from app import request_timing
from app.routers.image_routes import normalize_wire_format


def _begin(ua="undici"):
    return request_timing.begin({"headers": [(b"user-agent", ua.encode())]})


def _lwb_body(**overrides):
    params = {
        "params_version": 4, "width": 832, "height": 1216, "steps": 23, "n_samples": 1,
        "ucPresetId": "heavy", "qualityPresetId": "standard",
        "v4_prompt": {"caption": {"base_caption": "1girl", "char_captions": []}, "use_coords": True, "use_order": True},
        "v4_negative_prompt": {"caption": {"base_caption": "lowres", "char_captions": []}},
        "noise_schedule": "karras", "image_format": "png", "stream": "msgpack",
    }
    params.update(overrides.pop("parameters", {}))
    return {"input": "1girl", "model": "nai-diffusion-5-full", "action": "generate",
            "parameters": params, "use_new_shared_trial": True, **overrides}


def test_littlewhitebox_by_payload_markers():
    """有自有字段 ucPresetId / qualityPresetId 就直接认出来（浏览器 UA 也能认）。"""
    token = _begin("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/155.0.0.0 Safari/537.36")
    request_timing.refine_client_from_payload(_lwb_body())
    assert request_timing.snapshot()["client"].startswith("LittleWhiteBox")
    request_timing.end(token)


def test_littlewhitebox_by_fallback_combination():
    """没有那两个字段时，用「use_new_shared_trial + params_version 4 + msgpack + png」组合认。"""
    body = _lwb_body()
    body["parameters"].pop("ucPresetId")
    body["parameters"].pop("qualityPresetId")
    token = _begin()
    request_timing.refine_client_from_payload(body)
    assert request_timing.snapshot()["client"].startswith("LittleWhiteBox")
    request_timing.end(token)


def test_littlewhitebox_connection_probe():
    """「测试连接」探针：非 V5 路径固定发 input=test + nai-diffusion-3 + 64x64 + 1 step。

    2026-10-13 站长用小白盒测试时，日志里这两条（探针与实际出图）都落成了裸 Chrome UA —— 认不出来。
    """
    probe = {"input": "test", "model": "nai-diffusion-3", "action": "generate",
             "parameters": {"width": 64, "height": 64, "steps": 1, "n_samples": 1}}
    token = _begin("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/154.0.0.0 Safari/537.36")
    request_timing.refine_client_from_payload(probe)
    assert request_timing.snapshot()["client"].startswith("LittleWhiteBox")
    request_timing.end(token)


def test_probe_signature_does_not_swallow_real_small_requests():
    """别把真的 64x64 出图（步数不是 1，或 input 不是 test）也认成小白盒。"""
    token = _begin("Mozilla/5.0 Chrome/154")
    request_timing.refine_client_from_payload(
        {"input": "1girl", "model": "nai-diffusion-4-5-full", "action": "generate",
         "parameters": {"width": 64, "height": 64, "steps": 28, "n_samples": 1}})
    assert "LittleWhiteBox" not in request_timing.snapshot()["client"]
    request_timing.end(token)


def test_littlewhitebox_by_user_agent():
    """「后端发送」模式带自己的 UA。"""
    token = _begin("LittleWhiteBox/1.2 (SillyTavern)")
    assert request_timing.snapshot()["client"].startswith("LittleWhiteBox")
    request_timing.end(token)


def test_littlewhitebox_keeps_original_ua_as_suffix():
    token = _begin("undici")
    request_timing.refine_client_from_payload(_lwb_body())
    assert "(undici)" in request_timing.snapshot()["client"]
    request_timing.end(token)


def test_other_clients_are_not_swallowed():
    """不能把别的客户端误判成小白盒：官方 SDK 的 ucPreset / 柏宝绘风格体都要保持原样。"""
    official = {"action": "generate", "parameters": {"params_version": 3, "ucPreset": 0, "qualityToggle": True,
                                                     "stream": "msgpack"}}
    token = _begin("Mozilla/5.0 Chrome/155")
    request_timing.refine_client_from_payload(official)
    assert "LittleWhiteBox" not in request_timing.snapshot()["client"]
    request_timing.end(token)


def test_dsh_fingerprint_still_works():
    """原有指纹不能被后面的改动挤掉。"""
    body = {"action": "generate", "parameters": {
        "params_version": 3, "ucPreset": 3, "add_original_image": True, "controlnet_strength": 1,
        "v4_prompt": {"use_order": True, "use_coords": False, "caption": {"base_caption": "1girl", "char_captions": []}}}}
    token = _begin()
    request_timing.refine_client_from_payload(body)
    assert request_timing.snapshot()["client"].startswith("DSH-Tavern-Image")
    request_timing.end(token)


def test_known_client_is_not_overwritten():
    token = _begin("Aaalice/1.0")
    request_timing.refine_client_from_payload(_lwb_body())
    assert request_timing.snapshot()["client"].startswith("Aaalice")
    request_timing.end(token)


@pytest.mark.parametrize("raw,expected", [
    ("msgpack", "msgpack"), ("MessagePack", "msgpack"), ("binary", "msgpack"),
    ("sse", "sse"), ("text/event-stream", "sse"), ("stream", "sse"), ("", "sse"),
    (True, "sse"), (False, "sse"), (None, "sse"),
])
def test_normalize_wire_format_tolerates_common_spellings(raw, expected):
    assert normalize_wire_format(raw) == expected


def test_normalize_wire_format_rejects_junk():
    from app.main import GateError
    for junk in ("chunked", 42, {"a": 1}):
        with pytest.raises(GateError):
            normalize_wire_format(junk)
