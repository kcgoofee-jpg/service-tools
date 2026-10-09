"""首页实时架构图的数据：只给汇总数字，不含任何人的 Key、提示词、IP 或 Discord ID。

/public/live 每秒最多计算一次（结果缓存 1 秒），前端每秒轮询；/v1/live/me 需要 Key，只返回这把 Key 的图在队列里的位置。
"""
from __future__ import annotations

import time
from collections import deque
from typing import Any, Optional

WINDOW = 60
_EVENTS: deque = deque(maxlen=5000)          # (ts, status) 最近的请求结果，用于「近 1 分钟」
_cache: dict[str, Any] = {"at": 0.0, "body": None}


def note(status: str) -> None:
    _EVENTS.append((time.time(), status))


def recent(now: Optional[float] = None) -> dict[str, int]:
    now = time.time() if now is None else now
    while _EVENTS and _EVENTS[0][0] < now - WINDOW:
        _EVENTS.popleft()
    total = len(_EVENTS)
    rejected = sum(1 for _, s in _EVENTS if s == "rejected")
    return {"requests": total, "rejected": rejected}


def mask(name: str) -> str:
    name = (name or "").strip()
    return (name[:2] + "**") if name else "**"


async def build(state, registrar, now: Optional[float] = None) -> dict[str, Any]:
    now = time.time() if now is None else now
    if _cache["body"] is not None and now - _cache["at"] < 1.0:
        return _cache["body"]
    guard = getattr(state, "guard", None)
    pool = list(getattr(state.nai, "pool", []))
    day = state.day()
    mono = time.monotonic()
    accounts = []
    images_today = 0
    for t in pool:
        used = (await state.db.get_upstream_counter(t.token_id, day))["images"]
        images_today += used
        accounts.append({
            "usable": t.usable,
            "cooling": max(0, int(t.blocked_until - now)) if not t.disabled and t.admin_enabled else 0,
            "next_in": round(max(0.0, t.image_next_at - mono), 1),
            "hour": guard.hour_count(t.token_id, now) if guard else 0,
        })
    gv = guard.values if guard else {}
    usable = max(1, sum(1 for a in accounts if a["usable"]))
    try:
        v5_cap = int(float(await state.db.get_setting("global_daily_v5", state.settings.global_daily_v5) or 0))
    except (TypeError, ValueError):
        v5_cap = 0
    body: dict[str, Any] = {
        "t": now,
        "mode": "fifo",                         # 启用轮流出图后为 "drr"
        "auth": recent(now),
        "quota": {"images_today": images_today,
                  "images_cap": (gv.get("account_daily_cap") or 0) * usable,
                  "v5_today": await state.db.day_v5_total(day), "v5_cap": v5_cap},
        "queue": guard.queue_view() if guard else {"waiting": 0, "running": 0},
        "guard": {"hour": sum(a["hour"] for a in accounts),
                  "hour_cap": (guard.hourly_cap(now) if guard else 0) * usable,
                  "quiet": guard.in_quiet(now) if guard else False,
                  "next_in": min((a["next_in"] for a in accounts if a["usable"]), default=0),
                  "cooldown": state.image_cooldown_remaining() if hasattr(state, "image_cooldown_remaining") else 0},
        "accounts": [{"usable": a["usable"], "cooling": a["cooling"]} for a in accounts],
        "members": {},
    }
    if registrar is not None:
        cfg = await registrar.settings()
        active = await registrar.count_active()
        wl = await registrar.waitlist()
        body["members"] = {"max": cfg["max_users"], "active": active, "open": cfg["open"],
                           "waitlist": [{"name": mask(w["name"]), "invited": bool(w["invited_at"])} for w in wl[:5]],
                           "waitlist_total": len(wl)}
    _cache.update(at=now, body=body)
    return body


def mine(state, key_id: int) -> dict[str, Any]:
    guard = getattr(state, "guard", None)
    view = guard.queue_view(key_id) if guard else {"waiting": 0, "running": 0, "mine": []}
    interval = float(getattr(state.settings, "image_min_interval", 15) or 15)
    for item in view["mine"]:
        if item["state"] == "waiting":
            item["eta"] = int(item["position"] * interval)
    return {"mine": view["mine"], "waiting": view["waiting"], "running": view["running"]}
