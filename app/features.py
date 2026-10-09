"""功能开关：站长可在后台全局开关每项上游功能，并按 Key 授权。

全局开关存放在 site_settings(features_global)；每把 Key 的授权存放在 api_keys.features：
NULL 表示沿用旧行为（全局开启的功能都可用）；逗号分隔列表表示只允许其中列出的功能。
"""
from __future__ import annotations

import json
from typing import Any, Iterable, Optional

FEATURES: dict[str, str] = {
    "image": "文生图",
    "upscale": "放大",
    "augment": "导演工具（线稿、上色等）",
    "vibe": "Vibe 编码",
    "tags": "标签补全",
    "text": "文本生成 / OpenAI 兼容聊天",
    "voice": "语音合成",
}
GLOBAL_KEY = "features_global"

# 用量日志 usage_log.kind → 功能。后台按功能统计与筛选日志时使用。
KIND_FEATURE: dict[str, str] = {
    "image": "image", "image_stream": "image",
    "upscale": "upscale", "augment-image": "augment",
    "vibe_encode": "vibe", "tags": "tags",
    "text": "text", "chat": "text", "voice": "voice",
}


def kinds_for(feature: str) -> list[str]:
    return [kind for kind, name in KIND_FEATURE.items() if name == feature]


def parse_list(value: Any) -> Optional[list[str]]:
    """把 Key 上的 features 字段解析为列表；None 表示沿用旧行为。"""
    if value is None:
        return None
    return [name for name in str(value).split(",") if name in FEATURES]


def normalize(names: Iterable[str]) -> list[str]:
    return [name for name in FEATURES if name in set(names)]


def dump(names: Optional[Iterable[str]]) -> Optional[str]:
    return None if names is None else ",".join(normalize(names))


async def global_flags(db) -> dict[str, bool]:
    """全局开关；缺省为全部开启。"""
    flags = {name: True for name in FEATURES}
    getter = getattr(db, "get_setting", None)
    if getter is None:
        return flags
    try:
        raw = await getter(GLOBAL_KEY, None)
        saved = json.loads(raw) if raw else {}
    except (ValueError, TypeError):
        saved = {}
    for name in FEATURES:
        if isinstance(saved.get(name), bool):
            flags[name] = saved[name]
    return flags


def key_features(key) -> Optional[list[str]]:
    try:
        return parse_list(key["features"])
    except (KeyError, IndexError):
        return None


async def check(db, key, name: str) -> Optional[str]:
    """允许则返回 None，否则返回面向用户的拒绝原因。"""
    try:
        if key["is_admin"]:
            return None
    except (KeyError, IndexError):
        pass
    if not (await global_flags(db)).get(name, True):
        return f"站长暂未开放此功能：{FEATURES[name]}"
    allowed = key_features(key)
    if allowed is not None and name not in allowed:
        return f"你的 Key 暂无权使用：{FEATURES[name]}（可向站长申请）"
    return None
