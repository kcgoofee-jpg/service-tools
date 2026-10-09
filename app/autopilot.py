"""自动驾驶：把站长原来手动做的设置交给算法——闲置回收天数、名额、重置时间、全站临时暂停、单个 Key 暂停 / 重置。

━━ 先观察，后执行 ━━
每条规则都有两种模式：observe（只记录「如果是我会怎么做」，不改任何东西）和 enforce（真的执行）。
新规则一律先 observe 至少一天（当前版本所有规则都只观察，执行逻辑在审核通过后再接上），运维审核记录、确认没有误伤后再逐条切到 enforce（设置项 autopilot_<规则>）。
所有判断每 10 分钟做一次，结果写进 autopilot_last（后台可看）和操作日志（只记有动作的）。

━━ 规则 ━━
1. idle_days 闲置回收天数（原来固定 3 天）
   名额满了或有人在候补 → 2 天（让名额流转起来）；空位超过 30% → 5 天（没人等，不必急着回收）；其余 3 天。
2. slots 名额上限（原来手动改；2026-10-10 起执行）
   名额快满（空位 ≤ 2）或有人在候补，且昨天日用量 < 60%、被每小时上限拦的小时 < 3 → +5（最多 100），
   两次之间至少隔 6 小时。只加不减（人多了由动态额度调小每人份额，不踢人）。
3. reset_hour 每日额度重置时间（原来固定北京时间 0 点）
   取最近 7 天出图最少的那个整点作为建议重置时间，避免大家在 0 点一起抢 V5。
   改日期边界会影响当天计数，所以这条只给建议，执行要在运维审核后单独做迁移。
4. breaker 全站临时暂停（原来只有上游 429 才暂停）
   最近 15 分钟上游失败（5xx / 超时）≥ 5 次且占比 ≥ 30% → 暂停出图 5 分钟，首页显示原因，到点自动恢复。
5. key_guard 单个 Key 暂停 / 重置（原来要站长手动封禁）
   · 24 小时内「10 分钟内网段来回切换」信号 ≥ 2 次 → 暂停 24 小时（像是多人同时在用）。
   · 同时出现「≥ 3 种客户端」和「近 24 小时 ≥ 20 个小时在用」→ 重置 Key（旧 Key 立刻失效，新 Key 私信给本人：
     如果是 Key 泄露，本人不受影响，拿到 Key 的人用不了了）。
   · 1 小时内被拒绝 ≥ 60 次 → 暂停 1 小时（多半是客户端死循环重试）。
   重复触发逐级加重：24 小时 → 7 天 → 交给站长决定。暂停到点自动恢复，每次都会私信本人说明原因。
"""
from __future__ import annotations

import json
import time
from datetime import datetime, timedelta
from typing import Any, Optional

from .action_log import log_action

RULES = ("idle_days", "slots", "reset_hour", "breaker", "key_guard")
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
SLOTS_COOLDOWN = 6 * 3600     # 两次自动加名额至少隔 6 小时，先看加进来的人用得怎么样


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

    # 4 熔断
    rows = await _q(db, "SELECT SUM(CASE WHEN status='error' THEN 1 ELSE 0 END), COUNT(*) FROM usage_log "
                        "WHERE ts>=? AND kind LIKE 'image%' AND status IN ('ok','error')", now - 900)
    fails, total = int(rows[0][0] or 0), int(rows[0][1] or 0)
    trip, why = breaker_rule(fails, total)
    out["rules"]["breaker"] = {"mode": await _mode(db, "breaker"), "value": trip, "why": why}

    # 5 单个 Key
    events = list(getattr(getattr(state, "sources", None), "events", []) or [])
    rejects = {r[0]: r[1] for r in await _q(db, "SELECT key_id, COUNT(*) FROM usage_log WHERE ts>=? AND status='rejected' "
                                                 "AND key_id IS NOT NULL GROUP BY key_id", now - 3600)}
    keys = {r[0]: r[1] for r in await _q(db, "SELECT id, name FROM api_keys WHERE enabled=1 AND is_admin=0 AND is_test=0")}
    decisions = []
    for kid, name in keys.items():
        mine = [k for t, i, k in events if i == kid and t >= now - 86400]
        d = key_guard_rule(mine.count("alternate"), mine.count("clients"), mine.count("allday"), rejects.get(kid, 0))
        if d:
            decisions.append({"key": kid, "name": name, "action": d[0], "seconds": d[1], "why": d[2]})
    out["rules"]["key_guard"] = {"mode": await _mode(db, "key_guard"), "value": decisions,
                                 "why": f"{len(decisions)} 把 Key 触发" if decisions else "没有 Key 触发"}

    # 观察模式：只记录；有「会执行的动作」时写操作日志，方便第二天审核
    notable = [f"{r}：{v['why']}" for r, v in out["rules"].items()
               if (r == "key_guard" and v["value"]) or (r == "breaker" and v["value"])]
    if notable:
        await log_action(db, "系统", "自动驾驶（观察）", "", "；".join(notable)[:500])
    await db.set_setting(STATE_KEY, json.dumps(out, ensure_ascii=False))
    day = datetime.fromtimestamp(now).strftime("%Y-%m-%d %H:00")
    hist = json.loads(await db.get_setting(HISTORY_KEY, "[]") or "[]")
    if not hist or hist[-1].get("hour") != day:          # 每小时留一条快照，保留 7 天
        hist = (hist + [{"hour": day, **{r: v["value"] if r != "key_guard" else len(v["value"]) for r, v in out["rules"].items()}}])[-168:]
        await db.set_setting(HISTORY_KEY, json.dumps(hist, ensure_ascii=False))
    return out
