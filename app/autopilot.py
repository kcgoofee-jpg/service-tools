"""自动驾驶：把站长原来手动做的站务设置交给算法。每 10 分钟判断一次，结果写进 autopilot_last（后台可看），
执行了动作的写操作日志。

━━ 模式 ━━
每条规则独立设置 autopilot_<规则> = observe（只记录「如果是我会怎么做」）/ enforce（真的执行）/ off。
没设过的默认 observe。下面每条都写明「能不能执行」——有的规则只给建议，设成 enforce 也不会动。
（2026-10-10 线上：slots、breaker、key_guard 为 enforce；economy、idle_days 为 observe。）

━━ 规则 ━━
1. idle_days 闲置回收天数 —— 仅建议，没有执行路径
   名额满了或有人在候补 → 2 天；空位超过 30% → 5 天；其余 3 天。
   真正的回收天数仍是固定的 KEY_INACTIVITY_DELETE_DAYS（state.py），这里算出的值只供站长参考。
2. slots 名额上限 —— 可执行
   名额快满（空位 ≤ 2）或有人在候补，且昨天日用量 < 60%、被每小时上限拦的小时 < 3 → +5（最多 100），
   每天最多一次，且昨天数据要覆盖 ≥ 20 小时（数据不足时不动）。只加不减。
3. reset_hour 每日重置时间 —— 仅建议，没有执行路径
   取最近 7 天出图最少的整点。改日期边界会影响当天计数，要单独做迁移，所以 mode 固定显示 observe。
4. breaker 全站临时暂停 —— 可执行
   最近 15 分钟上游失败（5xx / 超时）≥ 5 次且占比 ≥ 30% → guard.trip_breaker 暂停出图 5 分钟，到点自动恢复。
5. key_guard 单个 Key —— 只有「1 小时内被拒 ≥ 60 次 → 暂停 1 小时」这一条可执行
   （客户端死循环重试；经 share_guard.pause_key，不私信）。
   另两条只观察、不执行：24 小时内「10 分钟内网段来回切换」≥ 2 次（建议暂停 24 小时）；
   「≥ 3 种客户端」且「近 24 小时 ≥ 20 个小时在用」（建议重置 Key）。重置会私信本人，申诉期内不接。
   没有逐级加重，也不私信——暂停原因在成员请求被拒时直接显示。
6. economy 节约模式 —— 可执行
   最近 15 分钟「排队的人太多」≥ 8 次 → 开（全站免费档 14 步 + Euler-a）；30 分钟 ≤ 1 次 → 关。
   两次切换至少隔 1 小时；开关都经 ops.set_economy 在公告频道通知成员。
"""
from __future__ import annotations

import json
import time
from datetime import datetime
from typing import Any, Optional

from . import site_flags
from .action_log import log_action
from . import reasons

STATE_KEY = "autopilot_last"
HISTORY_KEY = "autopilot_history"


async def _mode(db, rule: str) -> str:
    v = await db.get_setting(f"autopilot_{rule}", "observe")
    return v if v in ("observe", "enforce", "off") else "observe"


async def _q(db, sql: str, *args):
    return await db._db.execute_fetchall(sql, args)


def idle_days_rule(active: int, cap: int, waitlist: int) -> tuple[int, str]:
    if cap and (active >= cap or waitlist > 0):
        return 2, f"名额 {active}/{cap}、候补 {waitlist} 人：缩短到 2 天，让名额流转"
    if cap and active < cap * 0.7:
        return 5, f"名额 {active}/{cap}，空位较多：放宽到 5 天"
    return 3, f"名额 {active}/{cap}：保持 3 天"


SLOTS_MAX = 100
SLOTS_STEP = 5
SLOTS_COOLDOWN = 24 * 3600    # 每天最多加一次：判断用的是「昨天」的复盘，一天内重复判断是同一份数据（统计审查：会自我加速）


def slots_rule(cap: int, active: int, waitlist: int, day_util: float, blocked_hours: int) -> tuple[int, str]:
    """名额快满（空位 ≤ 2）或有人在候补，且账号昨天还有余量（日用量 < 60%、被每小时上限拦的小时 < 3）→ +5。
    只加不减：人多了由动态额度把每人份额调小，不踢人。"""
    if blocked_hours >= 3:
        return cap, f"昨天 {blocked_hours} 个小时被每小时上限拦过：名额不再增加"
    if day_util >= 0.6:
        return cap, f"昨天用量 {day_util:.0%}：名额不再增加"
    if cap and cap < SLOTS_MAX and (waitlist > 0 or cap - active <= 2):
        return min(SLOTS_MAX, cap + SLOTS_STEP), (f"名额 {active}/{cap}、候补 {waitlist} 人，"
                                                  f"昨天用量 {day_util:.0%}：名额 +{SLOTS_STEP}")
    return cap, f"名额 {active}/{cap}、候补 {waitlist} 人、昨天用量 {day_util:.0%}：名额不变"


def breaker_rule(fails: int, total: int) -> tuple[bool, str]:
    if fails >= 5 and total and fails / total >= 0.3:
        return True, f"最近 15 分钟上游失败 {fails}/{total}：暂停出图 5 分钟"
    return False, f"最近 15 分钟上游失败 {fails}/{total}：正常"


def key_guard_rule(alternate: int, clients: int, allday: int, rejects_1h: int) -> Optional[tuple[str, int, str]]:
    """返回 (动作, 时长秒, 原因) 或 None。动作：pause / reset。"""
    if clients and allday:
        return "reset", 0, "24 小时内用了 3 种以上客户端、且几乎全天在用：重置 Key（新 Key 私信本人）"
    if alternate >= 2:
        return "pause", 86400, f"24 小时内 {alternate} 次在 10 分钟内来回切换网段：暂停 24 小时"
    if rejects_1h >= 60:
        return "pause", 3600, f"1 小时内被拒绝 {rejects_1h} 次（多半是客户端在死循环重试）：暂停 1 小时"
    return None


ECON_ON_KEYS = 3           # 且来自至少这么多把不同的 Key（审查 P2：防止一个客户端重试就把全站切到节约模式）
ECON_ON_REJECTS = 8        # 最近 15 分钟「排队的人太多」≥ 这么多次 → 拥挤，开节约模式
ECON_OFF_REJECTS = 1       # 最近 30 分钟 ≤ 这么多次 → 不拥挤，关节约模式
ECON_DWELL = 3600          # 两次切换至少间隔 1 小时，避免公告刷屏


def economy_rule(on_now: bool, queue_rejects_15m: int, queue_rejects_30m: int,
                 keys_15m: int = ECON_ON_KEYS) -> tuple[bool, str]:
    """拥挤（排队被拒多）时建议开节约模式让更多人出到图；持续空闲时建议关掉恢复高质量。带滞回。
    开启还要求被拒的来自至少 ECON_ON_KEYS 把不同的 Key：一个客户端连续重试刷出来的拒绝不算全站拥挤。"""
    if not on_now and queue_rejects_15m >= ECON_ON_REJECTS and keys_15m < ECON_ON_KEYS:
        return False, f"最近 15 分钟排队被拒 {queue_rejects_15m} 次，但只来自 {keys_15m} 把 Key：不算全站拥挤"
    if not on_now and queue_rejects_15m >= ECON_ON_REJECTS:
        return True, f"最近 15 分钟 {queue_rejects_15m} 次因排队拥挤被拒：建议开节约模式（14 步）多服务些人"
    if on_now and queue_rejects_30m <= ECON_OFF_REJECTS:
        return False, f"最近 30 分钟排队拥挤仅 {queue_rejects_30m} 次：建议关节约模式，恢复高质量"
    return on_now, f"排队拥挤 15m={queue_rejects_15m}：保持{'节约' if on_now else '正常'}模式"


async def run(state, registrar=None, now: Optional[float] = None) -> dict[str, Any]:
    db = state.db
    now = time.time() if now is None else now
    out: dict[str, Any] = {"at": now, "rules": {}}

    # 1/2 名额与回收
    cfg = await registrar.settings() if registrar is not None else {"max_users": 0}
    active = await registrar.count_active() if registrar is not None else 0
    # 候补只算还没被邀请的人（已邀请的有 24 小时保留名额，不算在等）
    waitlist = sum(1 for w in await registrar.waitlist() if not w.get("invited_at")) if registrar is not None else 0
    days, why = idle_days_rule(active, cfg.get("max_users") or 0, waitlist)
    out["rules"]["idle_days"] = {"mode": await _mode(db, "idle_days"), "value": days, "why": why}

    # 用动态额度每天 0 点的复盘（昨天的用量 / 被拦小时数，已排除测试号和站长号）
    from . import quota_algo
    hist = json.loads(await db.get_setting(quota_algo.HISTORY_KEY, "[]") or "[]")
    last = hist[-1] if hist else {}
    day_util = (last.get("used", 0) / last["cap"]) if last.get("cap") else 0.0
    cap_now = cfg.get("max_users") or 0
    slots, why = slots_rule(cap_now, active, waitlist, day_util, int(last.get("hourly_blocks", 0)))
    if last.get("coverage_hours", 0) < 20:                 # 昨天数据不完整：不据此加名额
        slots, why = cap_now, f"昨天只有 {last.get('coverage_hours', 0)} 小时数据，名额不变"
    mode = await _mode(db, "slots")
    out["rules"]["slots"] = {"mode": mode, "value": slots, "why": why}
    if mode == "enforce" and slots > cap_now and registrar is not None:
        last_at = float(await db.get_setting("autopilot_slots_at", 0) or 0)
        if now - last_at >= SLOTS_COOLDOWN:
            from . import ops
            # 算法调整只在首页提示，不发 Discord（notify=False）
            await ops.set_registration(db, {"max_users": slots, "notify": False}, state, registrar)
            await db.set_setting("autopilot_slots_at", now)
            await log_action(db, "系统", "自动驾驶：名额", "", f"{cap_now} → {slots}：{why}")
            out["rules"]["slots"]["applied"] = True

    # 3 重置时间：最近 7 天每个北京时间整点的出图量，取最少的
    by_hour = {h: 0 for h in range(24)}
    for (h, n) in await _q(db, "SELECT CAST(strftime('%H', ts, 'unixepoch', '+8 hours') AS INT), COUNT(*) FROM usage_log "
                                "WHERE ts>=? AND status='ok' AND kind LIKE 'image%' GROUP BY 1", now - 7 * 86400):
        by_hour[int(h)] = n
    quiet = min(by_hour, key=lambda h: (by_hour[h], abs(h - 6)))
    out["rules"]["reset_hour"] = {"mode": "observe", "value": quiet,
                                  "why": f"最近 7 天 {quiet}:00 出图最少（{by_hour[quiet]} 张）；现在是 0 点重置（0 点 {by_hour[0]} 张）"}

    # 4 熔断：只算真正打到上游的失败——5xx，或 200 开头后流中途断开（up_status 2xx 但 status=error）。
    # 本地拦截（冷却 / 上限 / 排队超时，up_status=0）、上游 429（另有冷却和 AIMD 减半）、成员参数错误（4xx）都不算。
    # 只看上次熔断结束之后的数据，避免暂停期间没有新样本、下一轮拿同一批旧数据再熔断一次。
    since = max(now - 900, float(await db.get_setting("autopilot_breaker_until", 0) or 0))
    rows = await _q(db, "SELECT SUM(CASE WHEN status='error' AND (up_status>=500 OR up_status BETWEEN 200 AND 299) "
                        "THEN 1 ELSE 0 END), COUNT(*) FROM usage_log "
                        "WHERE ts>=? AND kind LIKE 'image%' AND status IN ('ok','error')", since)
    fails, total = int(rows[0][0] or 0), int(rows[0][1] or 0)
    trip, why = breaker_rule(fails, total)
    out["rules"]["breaker"] = {"mode": await _mode(db, "breaker"), "value": trip, "why": why}
    if trip and out["rules"]["breaker"]["mode"] == "enforce":
        guard = getattr(state, "guard", None)
        if guard is not None and getattr(guard, "breaker_until", 0.0) <= now:
            guard.trip_breaker(300, f"上游连续出错，已暂停出图 5 分钟（{why}）", now)
            await db.set_setting("autopilot_breaker_until", now + 300)
            await log_action(db, "系统", "自动驾驶：熔断", "", f"{why}；全站暂停出图 5 分钟")
            out["rules"]["breaker"]["applied"] = True

    # 5 单个 Key
    events = list(getattr(getattr(state, "sources", None), "events", []) or [])
    # 只数成员自己造成的拒绝：「Key 正在暂停」本身的拒绝不算（否则暂停到期立刻再暂停），
    # 全站层面的拒绝（排队满 / 账号上限 / 熔断 / 冷却）也不算——忙时自动重试的正常成员不能被当成死循环暂停
    skip = "','".join((reasons.KEY_PAUSED,) + reasons.SITE_LEVEL)
    rejects = {r[0]: r[1] for r in await _q(db, "SELECT key_id, COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected' "
                                                 f"AND key_id IS NOT NULL AND reason NOT IN ('{skip}') GROUP BY key_id",
                                             now - 3600)}
    keys = {r[0]: r[1] for r in await _q(db, "SELECT id, name FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0")}
    decisions = []
    for kid, name in keys.items():
        mine = [k for t, i, k in events if i == kid and t >= now - 86400]
        d = key_guard_rule(mine.count("alternate"), mine.count("clients"), mine.count("allday"), rejects.get(kid, 0))
        if d:
            decisions.append({"key": kid, "name": name, "action": d[0], "seconds": d[1], "why": d[2]})
    out["rules"]["key_guard"] = {"mode": await _mode(db, "key_guard"), "value": decisions,
                                 "why": f"{len(decisions)} 把 Key 触发" if decisions else "没有 Key 触发"}
    if decisions and out["rules"]["key_guard"]["mode"] == "enforce":
        share = getattr(state, "share", None)
        for d in decisions:
            # 只执行「1 小时内被拒 ≥60 次」这一条（客户端死循环限流，seconds==3600）；
            # 换网段(86400) / 重置 Key 仍只观察——重置会私信本人，申诉期间避免任何群发私信。
            if share is not None and d["action"] == "pause" and d["seconds"] == 3600:
                if await share.pause_key(d["key"], d["seconds"],
                                         "你的 Key 短时间内被大量拒绝，疑似客户端在反复重试，已暂停 1 小时", now):
                    await log_action(db, "系统", "自动驾驶：Key 限流", str(d["name"]), d["why"])
                    d["applied"] = True

    # 6 节约模式：拥挤（排队被拒多）时统一 14 步让更多人出到图，空闲时恢复高质量。切换都会公告。
    econ_now = await site_flags.get(db, site_flags.ECONOMY)
    r15 = (await _q(db, "SELECT COUNT(*), COUNT(DISTINCT key_id) FROM usage_log WHERE ts>=? AND status='rejected' "
                        f"AND reason='{reasons.QUEUE_FULL}'", now - 900))[0]
    qr15, keys15 = int(r15[0] or 0), int(r15[1] or 0)
    qr30 = int((await _q(db, "SELECT COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected' "
                             f"AND reason='{reasons.QUEUE_FULL}'", now - 1800))[0][0] or 0)
    econ_target, econ_why = economy_rule(econ_now, qr15, qr30, keys15)
    out["rules"]["economy"] = {"mode": await _mode(db, "economy"), "value": econ_target, "why": econ_why}
    if econ_target != econ_now and out["rules"]["economy"]["mode"] == "enforce":
        last_flip = float(await db.get_setting("autopilot_economy_at", 0) or 0)
        if now - last_flip >= ECON_DWELL:                 # 间隔 ≥1 小时，避免公告刷屏
            from . import ops
            if await ops.set_economy(db, econ_target, state, by="自动驾驶"):
                await db.set_setting("autopilot_economy_at", now)
                out["rules"]["economy"]["applied"] = True

    # 观察模式：只记录；有「会执行的动作」时写操作日志，方便第二天审核（已执行的规则各自单独记日志，这里不重复）
    notable = [f"{r}：{v['why']}" for r, v in out["rules"].items()
               if v.get("mode") != "enforce" and ((r == "key_guard" and v["value"]) or (r == "breaker" and v["value"]))]
    if notable:
        await log_action(db, "系统", "自动驾驶（观察）", "", "；".join(notable)[:500])
    await db.set_setting(STATE_KEY, json.dumps(out, ensure_ascii=False))
    day = datetime.fromtimestamp(now).strftime("%Y-%m-%d %H:00")
    hist = json.loads(await db.get_setting(HISTORY_KEY, "[]") or "[]")
    if not hist or hist[-1].get("hour") != day:          # 每小时留一条快照，保留 7 天
        hist = (hist + [{"hour": day, **{r: v["value"] if r != "key_guard" else len(v["value"]) for r, v in out["rules"].items()}}])[-168:]
        await db.set_setting(HISTORY_KEY, json.dumps(hist, ensure_ascii=False))
    return out
