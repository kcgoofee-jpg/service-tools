"""上游表现分析：生成速度、排队延迟、限流 / 失败、实际产能，按 V4.5 / V5 分开统计。

用途：公益站有被上游限流或封控的风险，这里把「最近 1 小时 / 24 小时」与「过去 7 天（不含最近 24 小时）」
的基线对比，变慢、限流增多、失败率上升、账号受限时给出提示，并由维护循环私信站长。
只读 usage_log，不额外采集数据；1.3.0 之前的日志没有耗时，不参与耗时统计。
"""
from __future__ import annotations

import math
from typing import Iterable, Optional

FAMILIES = ("V4.5", "V5", "其他")
HOUR = 3600
DAY = 24 * HOUR
BASELINE_DAYS = 7
MIN_SAMPLES = 5            # 少于这么多样本不下结论
SLOW_RATIO = 1.5           # 中位生成耗时比基线慢 50% 视为变慢
THROTTLE_COUNT_1H = 3      # 最近 1 小时 ≥3 次 429
THROTTLE_SHARE = 0.10      # 或 429 占比 ≥10%
FAIL_DROP = 0.10           # 成功率比基线低 10 个百分点
FAIL_FLOOR_1H = 0.80       # 最近 1 小时成功率低于 80%


def family(model: str) -> str:
    m = (model or "").strip().lower()
    if m.startswith("nai-diffusion-5"):
        return "V5"
    if m.startswith("nai-diffusion-4-5"):
        return "V4.5"
    return "其他"


def _pct(values: list[int], q: float) -> Optional[int]:
    if not values:
        return None
    values = sorted(values)
    idx = min(len(values) - 1, max(0, math.ceil(q * len(values)) - 1))
    return values[idx]


def _kind(status: str, up_status: int, detail: str) -> str:
    if status == "ok":
        return "ok"
    if up_status == 429 or "429" in (detail or ""):
        return "429"
    if up_status in (401, 402, 403):
        return "auth"
    if up_status >= 500:
        return "5xx"
    return "other"


def _stats(rows: list[tuple], slots: int, site_interval: float, key_interval: float) -> dict:
    ok = [r for r in rows if r[2] == "ok"]
    kinds = [_kind(r[2], r[6], r[7]) for r in rows]
    durs = [r[5] for r in ok if r[5] > 0]
    waits = [r[4] for r in ok if r[5] > 0]
    total = len(rows)
    p50 = _pct(durs, 0.5)
    out = {
        "requests": total, "ok": len(ok), "images": sum(r[3] for r in ok),
        "rate_limited": kinds.count("429"), "auth": kinds.count("auth"),
        "server": kinds.count("5xx"), "other": kinds.count("other"),
        "success_rate": round(len(ok) / total, 3) if total else None,
        "samples": len(durs),
        "p50_ms": p50, "p90_ms": _pct(durs, 0.9),
        "avg_wait_ms": int(sum(waits) / len(waits)) if waits else None,
        "p90_wait_ms": _pct(waits, 0.9),
    }
    if p50:
        # 每把上游 Token 一次一张，相邻两次至少间隔 site_interval；成员自己的间隔是 key_interval。
        per_slot = HOUR / max(p50 / 1000, site_interval)
        out["capacity_per_hour"] = int(per_slot * max(1, slots))
        out["member_capacity_per_hour"] = int(HOUR / max(p50 / 1000, key_interval))
    else:
        out["capacity_per_hour"] = out["member_capacity_per_hour"] = None
    return out


def _hourly(rows: list[tuple], now: float) -> list[dict]:
    start = (int(now) // HOUR) * HOUR - 23 * HOUR
    buckets = [{"t": start + i * HOUR, "ok": 0, "err": 0, "r429": 0, "images": 0, "durs": []} for i in range(24)]
    for r in rows:
        i = int((r[0] - start) // HOUR)
        if not 0 <= i < 24:
            continue
        b = buckets[i]
        if r[2] == "ok":
            b["ok"] += 1
            b["images"] += r[3]
            if r[5] > 0:
                b["durs"].append(r[5])
        else:
            b["err"] += 1
            if _kind(r[2], r[6], r[7]) == "429":
                b["r429"] += 1
    for b in buckets:
        b["p50_ms"] = _pct(b.pop("durs"), 0.5)
    return buckets


def _flags(fam: str, h1: dict, d1: dict, base: dict) -> list[dict]:
    out = []

    def add(code, level, text):
        out.append({"code": code, "family": fam, "level": level, "text": f"{fam}：{text}"})

    if d1["auth"]:
        add("account", "bad", f"24 小时内上游返回 401/402/403 共 {d1['auth']} 次，账号可能被限制，请登录 NovelAI 检查。")
    r429_share = h1["rate_limited"] / h1["requests"] if h1["requests"] else 0
    if h1["rate_limited"] >= THROTTLE_COUNT_1H or (h1["requests"] >= MIN_SAMPLES and r429_share >= THROTTLE_SHARE):
        add("throttle", "warn", f"最近 1 小时被上游限流（429）{h1['rate_limited']} 次，占 {round(r429_share * 100)}%。")
    recent = h1 if h1["samples"] >= MIN_SAMPLES else d1
    label = "最近 1 小时" if recent is h1 else "最近 24 小时"
    if recent["samples"] >= MIN_SAMPLES and base["samples"] >= MIN_SAMPLES and base["p50_ms"]:
        ratio = recent["p50_ms"] / base["p50_ms"]
        if ratio >= SLOW_RATIO:
            add("slow", "warn", f"{label}中位生成耗时 {recent['p50_ms'] / 1000:.1f}s，"
                                f"是过去 7 天的 {ratio:.1f} 倍（{base['p50_ms'] / 1000:.1f}s）。")
    if h1["requests"] >= MIN_SAMPLES and h1["success_rate"] is not None and h1["success_rate"] < FAIL_FLOOR_1H:
        add("fail", "bad", f"最近 1 小时成功率 {round(h1['success_rate'] * 100)}%。")
    elif (d1["requests"] >= 10 and base["requests"] >= 10 and d1["success_rate"] is not None
          and base["success_rate"] is not None and d1["success_rate"] < base["success_rate"] - FAIL_DROP):
        add("fail", "warn", f"24 小时成功率 {round(d1['success_rate'] * 100)}%，"
                            f"低于过去 7 天的 {round(base['success_rate'] * 100)}%。")
    return out


def analyze(rows: Iterable[tuple], now: float, *, slots: int = 1, site_interval: float = 15,
            key_interval: float = 15) -> dict:
    """rows: (ts, model, status, images, wait_ms, dur_ms, up_status, detail)，应覆盖最近 BASELINE_DAYS 天。"""
    grouped: dict[str, list[tuple]] = {f: [] for f in FAMILIES}
    for r in rows:
        grouped[family(r[1])].append(r)
    families, flags = {}, []
    for fam, items in grouped.items():
        if not items:
            continue
        h1 = _stats([r for r in items if r[0] >= now - HOUR], slots, site_interval, key_interval)
        d1 = _stats([r for r in items if r[0] >= now - DAY], slots, site_interval, key_interval)
        base = _stats([r for r in items if now - BASELINE_DAYS * DAY <= r[0] < now - DAY],
                      slots, site_interval, key_interval)
        families[fam] = {"h1": h1, "d1": d1, "baseline": base,
                         "hourly": _hourly([r for r in items if r[0] >= now - DAY], now),
                         "baseline_ready": base["samples"] >= MIN_SAMPLES}
        flags += _flags(fam, h1, d1, base)
    return {"families": families, "flags": flags, "slots": slots,
            "site_interval": site_interval, "key_interval": key_interval, "generated_at": now}


async def collect(state, now: float) -> dict:
    """从网关状态读取日志和当前并发 / 间隔设置，返回 analyze() 结果。"""
    rows = await state.db.image_perf_rows(now - BASELINE_DAYS * DAY)
    pool = getattr(getattr(state, "nai", None), "pool", []) or []
    slots = sum(t.image_slots.limit for t in pool if t.usable) or 1
    settings = state.settings
    return analyze(rows, now, slots=slots, site_interval=float(settings.image_min_interval),
                   key_interval=float(settings.key_image_min_interval))
