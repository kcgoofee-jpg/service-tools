"""生成记录：提示词与小缩略图（均可单独关闭，自动过期）。"""
from __future__ import annotations

import io
import zipfile
from typing import Optional

from PIL import Image

THUMB_SIDE = 320
THUMB_QUALITY = 55


def make_thumbnail(payload: bytes) -> Optional[bytes]:
    """从上游返回的 zip（或裸图片）里取第一张图，缩成小 JPEG；失败返回 None。"""
    try:
        data = payload
        if payload[:2] == b"PK":
            with zipfile.ZipFile(io.BytesIO(payload)) as archive:
                names = [n for n in archive.namelist() if n.lower().endswith((".png", ".jpg", ".jpeg", ".webp"))]
                if not names:
                    return None
                info = archive.getinfo(names[0])
                if info.file_size > 40 * 1024 * 1024:
                    return None
                data = archive.read(names[0])
        with Image.open(io.BytesIO(data)) as image:
            image = image.convert("RGB")
            image.thumbnail((THUMB_SIDE, THUMB_SIDE))
            out = io.BytesIO()
            image.save(out, "JPEG", quality=THUMB_QUALITY, optimize=True)
            return out.getvalue()
    except Exception:
        return None


def prompt_texts(body: dict) -> tuple[str, str]:
    """正向 / 负向提示词（截断，防止超长内容写库）。"""
    params = body.get("parameters") if isinstance(body.get("parameters"), dict) else {}
    positive = str(body.get("input") or "")
    negative = str(params.get("negative_prompt") or "")
    if not positive:
        caption = ((params.get("v4_prompt") or {}).get("caption") or {}) if isinstance(params.get("v4_prompt"), dict) else {}
        positive = str(caption.get("base_caption") or "")
    return positive[:2000], negative[:1000]


def audit_notice(prompts: bool, thumbs: bool, days: int) -> str:
    """向成员披露记录范围；未开启记录则返回空串。"""
    if not (prompts or thumbs):
        return ""
    what = "、".join(x for x, on in (("图片提示词", prompts), ("生成结果的小缩略图", thumbs)) if on)
    return f"为防止滥用，本站会保留你的{what}，{max(1, days)} 天后自动删除，仅站长可见。"
