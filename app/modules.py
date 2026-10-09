"""模块登记：把现有算法包成模块（容量 / 分配 / Anlas / 自动驾驶 / 诚信 / 领 Key / 观测）。

每个模块写明它回答的问题、依据的现实数据、数学原理、关键参数（依据等级）和交叉校验。
框架见 kernel.py。新增模块：写一个 Module(...) 并在 build() 里 register。
"""
from __future__ import annotations

import json
import time

from .kernel import Check, Kernel, Module, Param
from .params import P


async def _q1(db, sql: str, *args):
    rows = await db._db.execute_fetchall(sql, args)
    return rows[0][0] if rows else None


async def _setting_json(db, key: str) -> dict:
    try:
        return json.loads(await db.get_setting(key, "{}") or "{}")
    except (TypeError, ValueError):
        return {}


# ---------------- ① 容量 ----------------
def capacity(state) -> Module:
    from . import quota_algo
    g = state.guard

    async def tick(k: Kernel):
        changed = await g.adapt_daily()
        if changed:
            from .action_log import log_action
            await log_action(state.db, "系统", "自动调整：每小时上限", "", f"一天没有上游限流：{changed[0]} → {changed[1]}")
        return f"每小时上限 {g.values['account_hourly_cap']}" + (f"（{changed[0]} → {changed[1]}）" if changed else "")

    async def checks(k: Kernel):
        now = time.time()
        tokens = [t.token_id for t in getattr(state.nai, "pool", []) if getattr(t, "usable", True)]
        logged = int(await _q1(state.db, "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND status='ok' "
                                         "AND kind LIKE 'image%'", now - 3600) or 0)
        if not tokens:
            return [Check("有可用的上游账号", False, "没有可用的上游账号，无法核对每小时计数")]
        mem = max(g.hour_count(t, now) for t in tokens)
        cap = g.hourly_cap(now) * len(tokens)
        return [
            Check("内存每小时计数 ↔ 用量日志", mem >= logged - 2,
                  f"内存 {mem} 张，日志 {logged} 张" + ("" if mem >= logged - 2 else "：内存计数偏少，上限可能被绕过（重启清零？）")),
            Check("实际每小时出图 ≤ 上限", logged <= cap * 1.05 + 2, f"最近 60 分钟 {logged} 张，上限 {cap}"),
        ]

    return Module(
        name="capacity", title="① 容量", question="账号今天、这小时能承受多少？",
        reality="账号 V5 剩余与恢复速度（订阅接口）、上游 429、生成耗时、实际出图数",
        principle="电池模型（V5 剩余 + 每日恢复）；每小时上限用 AIMD：上游 429 减半，平稳一天 +10",
        tick=tick, checks=checks,
        hard_note="关闭后停止自动调整每小时上限；每小时 / 3 小时 / 每天上限仍然生效",
        params=[
            Param("每小时上限", lambda: g.values["account_hourly_cap"], "A", "AIMD 自动调整（100～200）",
                  "起点 150 没有依据；靠上游 429 反馈逐步变成实测值"),
            Param("3 小时上限", lambda: g.values["account_3h_cap"], "A", "", "防止连续几小时都在冲"),
            Param("每日上限", lambda: g.values["account_daily_cap"], "B", "fccc 账号日均 ~1370 张被限制（他人一次经历，单个案例）"),
            Param("每 1% V5 ≈ 张", lambda: quota_algo.V5_IMAGES_PER_PERCENT, "B", "官方 17.3 张 / 1%（23 步）× 23/28",
                  "待用账号实际扣减校准"),
            Param("V5 默认恢复 %/天", lambda: quota_algo.V5_FALLBACK_RATE, "B", "官方：订阅首月约 11%/天"),
            Param("出图间隔（秒）", lambda: state.settings.image_min_interval, "A", "", "加随机 0～5 秒"),
        ])


# ---------------- ② 分配 ----------------
def allocation(state) -> Module:
    from . import quota_algo

    async def tick(k: Kernel):
        r = await quota_algo.run(state)
        v5 = (r or {}).get("v5") or {}
        return f"V4.5 {r.get('ceiling')} / 保底 {r.get('base')} · V5 每人 {v5.get('each')} / 全站 {v5.get('global')}" if r else ""

    async def checks(k: Kernel):
        db = state.db
        last = await _setting_json(db, quota_algo.STATE_KEY)
        if not last or last.get("enabled") is False:
            return []
        v5, a, b = last.get("v5") or {}, last.get("ceiling"), last.get("base")
        each, people, glob = v5.get("each") or 0, v5.get("people") or 0, v5.get("global") or 0
        reg_img = int(float(await db.get_setting("register_daily_images", 0) or 0))
        reg_v5 = int(float(await db.get_setting("register_daily_v5", 0) or 0))
        drift = int(await _q1(db, "SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 "
                                  "AND enabled=1 AND (daily_images<>? OR daily_v5<>?)", a, each) or 0)
        base_now = state.guard.values.get("base_daily_images", 0)
        return [
            Check("人均 V5 × 人数 ≤ 全站 V5", each * people <= glob, f"{each} × {people} = {each * people}，全站 {glob}"),
            Check("领 Key 默认额度 = 算法当前值", reg_img == a and reg_v5 == each,
                  f"领 Key 默认 V4.5 {reg_img} / V5 {reg_v5}，算法 {a} / {each}"),
            Check("算法管理的 Key 都已同步", drift == 0, f"{drift} 把 Key 的额度和算法结果不一致"),
            Check("保底 ≤ 上限", base_now <= (a or 0), f"保底 {base_now}，上限 {a}"),
            Check("保底 = 算法当前值", base_now == b, f"实际保底 {base_now}，算法 {b}"),
        ]

    cfg = quota_algo.DEFAULTS
    return Module(
        name="allocation", title="② 分配", question="容量怎么分给每个人？",
        reality="最近 3 天活跃人数、昨天的总用量、被每小时上限拦的小时数、顶格人数",
        principle="全站 V5 按人均截断分配（max-min 公平）；V4.5 上限 / 保底按昨天拥挤程度加减步长；保底 + 空闲借用",
        tick=tick, checks=checks, hard_note="关闭后额度冻结在当前值，不再随人数和拥挤程度变化",
        params=[
            Param("V4.5 上限初始值", lambda: cfg["quota_target_avg"], "A", "站长定：平均 150"),
            Param("保底初始值", lambda: cfg["quota_base"], "A", "站长定：100"),
            Param("每日步长 上限 / 保底", lambda: f"{cfg['quota_step']} / {cfg['quota_base_step']}", "A"),
            Param("拥挤：用量 ≥", lambda: P("allocation.congested_util", 0.85), "A", "", "与「拦截小时」是「或」关系 → 待改为两者都满足 + 3 天平滑", key="allocation.congested_util"),
            Param("拥挤：拦截小时 ≥", lambda: P("allocation.congested_hours", 3), "A", "", "只基于一次 10 连拦（样本 1）", key="allocation.congested_hours"),
            Param("余量：用量 <", lambda: P("allocation.slack_util", 0.6), "D", "与「有人顶格」同时满足才放宽", key="allocation.slack_util"),
            Param("V5 系数 k（按剩余）", lambda: "≥90:1.3 · ≥70:1.1 · ≥40:0.9 · ≥20:0.6 · 其余 0.3", "A",
                  "目标让剩余稳定在 60～80%", "待换连续 P 控制"),
            Param("V5 每人范围", lambda: f"{cfg['quota_v5_min']}～{cfg['quota_v5_max']}", "A"),
            Param("至少按几人分", lambda: P("allocation.min_people", 10), "A", "", "给新来的人留位置", key="allocation.min_people"),
        ])


# ---------------- Anlas 自动分配 ----------------
def anlas(state) -> Module:
    from . import anlas_pool

    async def tick(k: Kernel):
        r = await anlas_pool.rebalance(state, notify=k.extra.get("dm"))
        return f"每人 {r.get('per_member')} Anlas · {len(r.get('members') or [])} 人" if isinstance(r, dict) else ""

    async def checks(k: Kernel):
        last = await _setting_json(state.db, anlas_pool.STATE_KEY)
        per = last.get("per_member") or 0
        return [Check("分到的 Anlas 至少够一张续杯", per == 0 or per >= anlas_pool.V5_PAID_PRICE,
                      f"每人 {per}，一张 V5 约 {anlas_pool.V5_PAID_PRICE}")] if last else []

    d = anlas_pool.DEFAULTS
    return Module(
        name="anlas", title="Anlas 续杯", question="V5 用完后，账号的 Anlas 怎么分给常用成员续杯？",
        reality="账号剩余 Anlas、离补满还有几天、成员近 7 天出图数",
        principle="（剩余 − 保留）÷ 剩余天数 ÷ 合格人数，低于一张的价格就不分",
        tick=tick, checks=checks, hard_note="关闭后不再重新分配，已分配的当天额度保持",
        params=[
            Param("续杯单价", lambda: anlas_pool.V5_PAID_PRICE, "C", "policy.py 实测扣费系数（20 × V5 1.5 倍）"),
            Param("保留不分", lambda: d["anlas_reserve"], "A"),
            Param("每人每天最多", lambda: d["anlas_member_daily_cap"], "A", "", "约 3 张"),
            Param("近 7 天至少出图", lambda: d["anlas_min_images_7d"], "A"),
        ])


# ---------------- 自动驾驶 ----------------
def autopilot_module(state) -> Module:
    from . import autopilot

    async def tick(k: Kernel):
        r = await autopilot.run(state, k.extra.get("registrar"))
        slots = (r.get("rules") or {}).get("slots") or {}
        return f"名额判断 {slots.get('value')}（{slots.get('mode')}）"

    return Module(
        name="autopilot", title="自动驾驶", question="名额、回收天数、熔断这些站务设置怎么自动调？",
        reality="已领人数、候补、昨天用量、上游失败率",
        principle="每条规则先观察后执行；名额 +5 需要「快满或有候补」「昨天有余量」「拦截小时 < 3」三者同时满足",
        tick=tick, hard_note="关闭后名额等设置保持当前值",
        params=[Param("名额自动 +", lambda: autopilot.SLOTS_STEP, "D", "三个条件同时满足"),
                Param("名额上限", lambda: autopilot.SLOTS_MAX, "A"),
                Param("两次加名额间隔（小时）", lambda: autopilot.SLOTS_COOLDOWN // 3600, "A")])


# ---------------- ④ 诚信（防分享）----------------
def integrity(state) -> Module:
    from . import share_guard as sg

    async def get_enabled(k: Kernel):
        return await state.share.mode() != "off"

    async def set_enabled(k: Kernel, on: bool):
        await state.db.set_setting(sg.MODE_SETTING, "enforce" if on else "off")

    async def checks(k: Kernel):
        now = time.time()
        db = state.db
        rows = await db._db.execute_fetchall(
            "SELECT key_id FROM share_state WHERE paused_until>? OR strikes>0", (now,))
        unsupported = []
        for (kid,) in rows:
            strong = await _q1(db, f"SELECT COUNT(*) FROM share_evidence WHERE key_id=? AND kind IN "
                                   f"({','.join('?' * len(sg.STRONG))}) AND points>0", kid, *sg.STRONG)
            if not strong:
                unsupported.append(kid)
        return [Check("每个处罚都有强证据", not unsupported,
                      f"{len(rows)} 把 Key 在处罚中" + (f"，其中 {unsupported} 没有强证据" if unsupported else ""))]

    return Module(
        name="integrity", title="④ 诚信（防分享）", question="谁在破坏公平（共用 Key、倒卖）？",
        reality="来源网络（打码）、客户端指纹、出图习惯（参数签名 + 提示词固定部分）、是否同时在途",
        principle="多证据加权、半衰期衰减；三重滤网：辅助证据必须有强证据印证；提醒 → 暂停 → 重置 → 停用",
        get_enabled=get_enabled, set_enabled=set_enabled, checks=checks,
        hard_note="关闭后不再收集证据和处罚；已暂停的 Key 到期自动恢复",
        params=[
            Param("强证据加分", lambda: sg._pts("overlap"), "A", "", "证据之间相关，直接相加会重复计分；待改为校准的似然比", key="share.points.overlap"),
            Param("辅助证据加分", lambda: f"{sg.POINTS['multi_device']} / {sg.POINTS['allday']}", "A", "",
                  "72 小时内有计分的强证据才计分"),
            Param("提醒 / 暂停 / 重置", lambda: f"{P('share.warn', sg.WARN)} / {P('share.pause', sg.PAUSE)} / {P('share.reset', sg.RESET)}", "A", "", "待回放回测", key="share.warn"),
            Param("半衰期（小时）", lambda: P("share.half_life", sg.HALF_LIFE) // 3600, "A", "", "待回放回测", key="share.half_life"),
            Param("停用前违规次数", lambda: sg.STRIKES_BAN, "A"),
        ])


# ---------------- 领 Key ----------------
def registration(state) -> Module:
    async def get_enabled(k: Kernel):
        return str(await state.db.get_setting("register_open", "0")) in ("1", "true", "True")

    async def set_enabled(k: Kernel, on: bool):
        await state.db.set_setting("register_open", "1" if on else "0")

    async def checks(k: Kernel):
        reg = k.extra.get("registrar")
        if reg is None:
            return []
        cfg = await reg.settings()
        active = await reg.count_active()
        cap = cfg.get("max_users") or 0
        return [Check("已领人数 ≤ 名额", not cap or active <= cap, f"{active} / {cap}")]

    return Module(
        name="registration", title="领 Key", question="谁能领 Key、名额多少？",
        reality="已领人数、候补、Discord 账号年龄、身份组",
        principle="名额由自动驾驶按负载开放；闲置回收让名额流转",
        get_enabled=get_enabled, set_enabled=set_enabled, checks=checks,
        hard_note="关闭 = 暂停领取，已领的成员不受影响",
        params=[Param("Key 有效天数", lambda: getattr(state.settings, "default_key_expires_days", 30), "A"),
                Param("闲置回收天数", lambda: state.settings.key_inactivity_delete_days, "A")])


# ---------------- ⑥ 观测 ----------------
def observation(state) -> Module:
    async def checks(k: Kernel):
        db = state.db
        day = state.day()
        counted = int(await _q1(db, "SELECT COALESCE(SUM(images),0) FROM counters WHERE day=?", day) or 0)
        start = time.mktime(time.strptime(day, "%Y-%m-%d"))
        logged = int(await _q1(db, "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>=? AND status='ok'", start) or 0)
        return [Check("今日计数 ↔ 用量日志", abs(counted - logged) <= 2, f"计数 {counted} 张，日志 {logged} 张")]

    return Module(
        name="observation", title="⑥ 观测", question="实际发生了什么？各处的数字对得上吗？",
        reality="用量日志、每日计数、上游状态",
        principle="同一件事用两个独立来源记录，互相核对",
        checks=checks, hard_note="关闭后不再做数据一致性核对；日志照常记录")


def build(state, bug) -> Kernel:
    k = Kernel(state, bug)
    for make in (observation, capacity, allocation, anlas, autopilot_module, integrity, registration):
        k.register(make(state))
    return k
