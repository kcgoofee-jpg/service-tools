"""调度影子模式：用真实请求日志回放「保底 + 空闲借用 + 成员间轮流（DRR）+ 账号每日上限」，只计算、不改变实际出图顺序。

为什么用日志回放而不是在请求路径里旁路计算：影子模式绝不能影响线上请求；日志里已有每个请求的
到达时间（完成时间 − 生成耗时 − 排队时间）、生成耗时和 Key，足够复现排队过程，也方便与实际等待对比。

结构参照 Linux HTB：账号是父节点（每日上限 ACCOUNT_DAILY_CAP），每把 Key 是子节点（保底 BASE_QUOTA，
借用上限 CEIL_QUOTA）；繁忙时只服务保底内请求，空闲时允许借用；同一档内按 DRR 在 Key 之间轮流。
空闲 / 繁忙用带滞回的利用率判断：> BUSY_UTIL 进入繁忙，< IDLE_UTIL 才回到空闲（M/D/1：利用率 80% 后排队急剧变长）。
1.3.0 之前的日志没有耗时，不参与回放，只计入当天张数。
"""
from __future__ import annotations

import math
from collections import defaultdict, deque
from typing import Iterable, Optional

BASE_QUOTA = 100           # 每把 Key 每天保底张数（V4.5）；线上以「账号保护与排队」的设置为准
CEIL_QUOTA = 300           # 空闲时最多借到的张数（= 成员 Key 的每日上限）
ACCOUNT_DAILY_CAP = 1000   # 每个上游账号每天总上限（fccc 的号在日均约 1370 张时被限）
BUSY_UTIL = 0.70
IDLE_UTIL = 0.50
UTIL_WINDOW = 15 * 60      # 利用率按 15 分钟窗口计算
LIGHT_USER_MAX = 10        # 当天出图 ≤10 张算轻度用户


def _pct(values: list[float], q: float) -> Optional[float]:
    if not values:
        return None
    values = sorted(values)
    return round(values[min(len(values) - 1, max(0, math.ceil(q * len(values)) - 1))], 1)


def _requests(rows: Iterable[tuple]) -> list[dict]:
    """rows: (ts, key_id, key_name, status, images, wait_ms, dur_ms)；只取有耗时的出图请求。"""
    out = []
    for ts, key_id, name, status, images, wait_ms, dur_ms in rows:
        if dur_ms <= 0 or key_id is None:
            continue
        start = ts - dur_ms / 1000
        out.append({"key": key_id, "name": name, "arrival": start - wait_ms / 1000, "start": start,
                    "dur": dur_ms / 1000, "wait": wait_ms / 1000, "ok": status == "ok", "images": images})
    out.sort(key=lambda r: r["arrival"])
    return out


def contention(reqs: list[dict]) -> dict:
    """每次真正发往上游时，还有几位「其他成员」在排队。"""
    hist = {"0": 0, "1": 0, "2": 0, "3+": 0}
    peak = 0
    for r in reqs:
        others = {o["key"] for o in reqs
                  if o["key"] != r["key"] and o["arrival"] <= r["start"] < o["start"]}
        n = len(others)
        peak = max(peak, n)
        hist["3+" if n >= 3 else str(n)] += 1
    total = sum(hist.values())
    return {"hist": hist, "dispatches": total, "peak_waiting_members": peak,
            "contended_share": round((total - hist["0"]) / total, 3) if total else None}


def replay(reqs: list[dict], policy: str, *, slots: int, interval: float, key_interval: float) -> dict:
    """在同样的到达时间和生成耗时下，按 fifo / drr 重新排队，返回等待统计。"""
    pending = deque(sorted(reqs, key=lambda r: r["arrival"]))
    queue: list[dict] = []
    free = [0.0] * max(1, slots)
    key_next: dict = defaultdict(float)
    last_served: dict = {}
    waits: dict = defaultdict(list)
    t = 0.0
    while pending or queue:
        slot = min(range(len(free)), key=free.__getitem__)
        t = max(t, free[slot])
        if not queue and pending:
            t = max(t, pending[0]["arrival"])
        while pending and pending[0]["arrival"] <= t:
            queue.append(pending.popleft())
        eligible = [q for q in queue if key_next[q["key"]] <= t]
        if not eligible:
            t = min([key_next[q["key"]] for q in queue] + ([pending[0]["arrival"]] if pending else []))
            continue
        if policy == "fifo":
            pick = min(eligible, key=lambda q: q["arrival"])
        else:
            heads: dict = {}
            for q in eligible:
                if q["key"] not in heads or q["arrival"] < heads[q["key"]]["arrival"]:
                    heads[q["key"]] = q
            # 每份额度 = 1 张时，DRR 等价于「最久没被服务的成员先出」；刚加入的成员排在刚服务过的人前面。
            pick = min(heads.values(), key=lambda q: (last_served.get(q["key"], -math.inf), q["arrival"]))
            last_served[pick["key"]] = t
        queue.remove(pick)
        waits[pick["key"]].append(t - pick["arrival"])
        key_next[pick["key"]] = t + key_interval
        free[slot] = t + max(interval, pick["dur"])
    every = [w for ws in waits.values() for w in ws]
    light = [w for ws in waits.values() if len(ws) <= LIGHT_USER_MAX for w in ws]
    return {"p50": _pct(every, .5), "p90": _pct(every, .9), "max": _pct(every, 1.0),
            "light_p90": _pct(light, .9),
            "worst_member_mean": round(max(sum(ws) / len(ws) for ws in waits.values()), 1) if waits else None}


def busy_windows(reqs: list[dict], *, slots: int, interval: float) -> dict:
    """15 分钟窗口利用率 + 滞回状态机：多少时间处于「繁忙」（只服务保底内请求）。"""
    if not reqs:
        return {"windows": [], "busy_minutes": 0, "peak_util": None, "state": "idle"}
    start = math.floor(reqs[0]["arrival"] / UTIL_WINDOW) * UTIL_WINDOW
    end = max(r["start"] for r in reqs)
    windows, state, busy = [], "idle", 0
    w = start
    while w <= end:
        used = sum(max(interval, r["dur"]) for r in reqs if w <= r["start"] < w + UTIL_WINDOW)
        util = used / (UTIL_WINDOW * max(1, slots))
        if state == "idle" and util > BUSY_UTIL:
            state = "busy"
        elif state == "busy" and util < IDLE_UTIL:
            state = "idle"
        if state == "busy":
            busy += UTIL_WINDOW // 60
        windows.append({"t": w, "util": round(util, 3), "state": state})
        w += UTIL_WINDOW
    return {"windows": windows, "busy_minutes": busy, "peak_util": max(x["util"] for x in windows), "state": state}


def borrowing(day_images: dict, names: dict, *, accounts: int, base: int = BASE_QUOTA,
              ceil: int = CEIL_QUOTA, account_cap: int = ACCOUNT_DAILY_CAP) -> dict:
    """当天每把 Key 的张数相对保底 / 借用上限的位置，以及账号总量相对每日上限。"""
    rows = []
    for key, n in sorted(day_images.items(), key=lambda kv: -kv[1]):
        rows.append({"key": key, "name": names.get(key, f"#{key}"), "images": n,
                     "borrowed": max(0, n - base) if base else 0, "over_ceil": max(0, n - ceil)})
    total = sum(day_images.values())
    cap = account_cap * max(1, accounts) if account_cap else 0
    return {"members": rows, "total": total, "account_cap": cap, "cap_used": round(total / cap, 3) if cap else None,
            "borrowers": sum(1 for r in rows if r["borrowed"]), "borrowed_images": sum(r["borrowed"] for r in rows),
            "base": base, "ceil": ceil}


def analyze(rows: Iterable[tuple], *, slots: int = 1, interval: float = 15, key_interval: float = 15,
            accounts: int = 1, base: int = BASE_QUOTA, account_cap: int = ACCOUNT_DAILY_CAP,
            ceil: int = CEIL_QUOTA) -> dict:
    rows = list(rows)
    reqs = _requests(rows)
    day_images: dict = defaultdict(int)
    names: dict = {}
    for ts, key_id, name, status, images, wait_ms, dur_ms in rows:
        if status == "ok" and key_id is not None:
            day_images[key_id] += images
            names[key_id] = name
    actual = [r["wait"] for r in reqs]
    light_keys = {k for k, n in day_images.items() if n <= LIGHT_USER_MAX}
    return {
        "samples": len(reqs),
        "actual": {"p50": _pct(actual, .5), "p90": _pct(actual, .9), "max": _pct(actual, 1.0),
                   "light_p90": _pct([r["wait"] for r in reqs if r["key"] in light_keys], .9)},
        "fifo": replay(reqs, "fifo", slots=slots, interval=interval, key_interval=key_interval) if reqs else None,
        "drr": replay(reqs, "drr", slots=slots, interval=interval, key_interval=key_interval) if reqs else None,
        "contention": contention(reqs),
        "load": busy_windows(reqs, slots=slots, interval=interval),
        "quota": borrowing(day_images, names, accounts=accounts, base=base, ceil=ceil, account_cap=account_cap),
        "params": {"base": base, "ceil": ceil, "account_cap": account_cap,
                   "busy_util": BUSY_UTIL, "idle_util": IDLE_UTIL, "slots": slots, "interval": interval},
    }


async def collect(state, since: float) -> dict:
    rows = await state.db.shadow_rows(since)
    pool = getattr(getattr(state, "nai", None), "pool", []) or []
    usable = [t for t in pool if t.usable]
    slots = sum(t.image_slots.limit for t in usable) or 1
    guard = getattr(state, "guard", None)
    extra = {"base": guard.values["base_daily_images"], "account_cap": guard.values["account_daily_cap"]} if guard else {}
    try:   # 借用上限跟随动态额度算法当前的 quota_ceiling（100～300 浮动），不再固定 300，否则影子回放和真实规则对不上
        live_ceil = await state.db.get_setting("quota_ceiling", None)
        if live_ceil is not None:
            extra["ceil"] = int(float(live_ceil))
    except (TypeError, ValueError):
        pass
    return analyze(rows, slots=slots, interval=float(state.settings.image_min_interval),
                   key_interval=float(state.settings.key_image_min_interval), accounts=max(1, len(usable)), **extra)
