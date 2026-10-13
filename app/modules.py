"""模块登记：容量 / 分配 / 自动驾驶 / 风险检查 / 领 Key / 观测。

每个模块写明回答的问题、关键参数的当前值和交叉校验。框架见 kernel.py；新增模块写一个 Module(...) 并在 build() 里 register。
"""
from __future__ import annotations

import json
import time
from typing import Any

from . import site_flags
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
        now = time.time()
        last = await _q1(state.db, "SELECT MAX(ts) FROM upstream_snapshots")
        if not last or now - float(last) >= 3600:          # 每小时记一次上游真实状态
            fresh = await quota_algo.refresh_allowance(state)    # 主动读一次，夜里没人用 V5 时也是真实值
            pct, rate = await quota_algo._allowance(state)
            await state.db._db.execute("INSERT INTO upstream_snapshots(ts, v5_percent, v5_rate, anlas, fresh) VALUES (?,?,?,?,?)",
                                       (now, pct, rate, 0, 1 if fresh else 0))
            await state.db._db.commit()
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
        # 用「窗口内出现过的最高上限」比：上限刚被 429 减半 / 刚进安静时段时，
        # 窗口里那些按当时更高上限放行的图不该算超限（见 guard.hourly_cap_ceiling）
        cap = g.hourly_cap_ceiling(now) * len(tokens)
        # 独立来源：上游报告的 V5 剩余变化 ↔ 我们日志里数到的 V5 张数（同时校准「每 1% ≈ 14.2 张」）
        snaps = await state.db._db.execute_fetchall(
            "SELECT ts, v5_percent, v5_rate FROM upstream_snapshots WHERE ts>? AND v5_percent IS NOT NULL ORDER BY ts",
            (now - 86400,))
        v5_check = Check("上游 V5 消耗 ↔ 日志 V5 张数", True, "数据不足（需要 ≥ 6 小时、账号剩余 < 95%）")
        if len(snaps) >= 2 and snaps[-1][0] - snaps[0][0] >= 6 * 3600 and snaps[-1][1] < 95:
            hours = (snaps[-1][0] - snaps[0][0]) / 3600
            rate = snaps[-1][2] or quota_algo.V5_FALLBACK_RATE
            used_pct = snaps[0][1] - snaps[-1][1] + rate * hours / 24
            counted = int(await _q1(state.db, "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>? AND ts<=? "
                                              "AND status='ok' AND model LIKE '%diffusion-5%'", snaps[0][0], snaps[-1][0]) or 0)
            if used_pct > 0.5:
                per = counted / used_pct
                v5_check = Check("上游 V5 消耗 ↔ 日志 V5 张数", 7 <= per <= 28,
                                 f"{hours:.0f} 小时内上游用掉 {used_pct:.1f}%，日志 {counted} 张 → 每 1% ≈ {per:.1f} 张（参数 14.2）")
        return [
            v5_check,
            Check("内存每小时计数 ↔ 用量日志", mem >= logged - 2,
                  f"内存 {mem} 张，日志 {logged} 张" + ("" if mem >= logged - 2 else "：内存计数偏少，上限可能被绕过（重启清零？）")),
            Check("实际每小时出图 ≤ 上限", logged <= cap * 1.05 + 2, f"最近 60 分钟 {logged} 张，上限 {cap}"),
        ]

    return Module(
        name="capacity", title="容量", question="每个上游账号一小时、一天能出多少张",
        tick=tick, checks=checks,
        hard_note="关闭后不再自动调每小时上限；各项上限照常生效",
        params=[
            Param("每天", lambda: g.values["account_daily_cap"]),
            Param("按时段放行", lambda: f"服务日起点先放 {g.values['daily_burst']} 张，之后每小时补 "
                  f"{round(g.values['account_daily_cap'] / 24)} 张" if g.values.get("daily_burst") else "关"),
            Param("每小时", lambda: g.values["account_hourly_cap"], "连续 24 小时没有 429 且顶到过上限时 +10"),
            Param("出图间隔（秒）", lambda: state.settings.image_min_interval, "另加随机 0～5 秒"),
            Param("V5 每 1% ≈ 张", lambda: quota_algo.V5_IMAGES_PER_PERCENT),
        ])


# ---------------- ② 分配 ----------------
def allocation(state) -> Module:
    from . import quota_algo

    async def tick(k: Kernel):
        r = await quota_algo.run(state)
        v5 = (r or {}).get("v5") or {}
        return f"每人 V4.5 {r.get('ceiling')} 张 · V5 {v5.get('each')} 张（全站 V5 {v5.get('global')}）" if r else ""

    async def checks(k: Kernel):
        db = state.db
        last = await _setting_json(db, quota_algo.STATE_KEY)
        if not last or last.get("enabled") is False:
            return []
        v5, a = last.get("v5") or {}, last.get("ceiling")
        each = v5.get("each") or 0
        global_v5 = v5.get("global") or 0
        keys = int(await _q1(db, "SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 "
                                 "AND enabled=1") or 0)
        drift = int(await _q1(db, "SELECT COUNT(*) FROM api_keys WHERE quota_auto=1 AND is_admin=0 AND is_test=0 "
                                  "AND enabled=1 AND (daily_images<>? OR daily_v5<>?)", a, each) or 0)
        # 每人额度 × 人数 与全站上限的关系：超卖时「每人 M 张」只是个人上限，全站仍是先到先得。
        # 不改分配结果（要站长确认），但必须在后台看得见，否则首页承诺与成员实际体验长期不一致。
        sold = each * keys
        if await db.get_setting("ack_v5_oversell", ""):
            # 超卖是「先到先得」的有意设计（站长已在后台确认）：转绿，数字仍显示，不再每 10 分钟记一条 warn
            oversell = Check("V5：每人额度 × 人数 ≤ 全站上限", True,
                             f"站长已确认先到先得：每人 {each} × {keys} 人 = {sold} 张，全站上限 {global_v5} 张")
        else:
            oversell = Check("V5：每人额度 × 人数 ≤ 全站上限", not (global_v5 and sold > global_v5),
                             f"每人 {each} × {keys} 人 = {sold} 张，全站上限 {global_v5} 张"
                             + (f"：超卖 {sold - global_v5} 张，先到先得，后到的成员拿不到「每人 {each} 张」"
                                if global_v5 and sold > global_v5 else ""))
        # V4.5：保底层（超过保底只在空闲时放行）只在 保底 < 每日上限 时存在；算法把两者设成同一个数后这一层就没了，
        # SYSTEM-FACTS 里「保底 ≤ 上限、保底 = 算法当前值」两句话需要有代码来核对。
        g = getattr(state, "guard", None)
        base = g.values.get("base_daily_images") if g is not None else 0
        base_check = Check("V4.5：保底 ≤ 每日上限", not base or not a or base <= a,
                           f"保底 {base} · 每人上限 {a}（两者相等 = 没有保底/空闲借用两层，只有每日上限）")
        return [Check("算法管理的 Key 都已同步", drift == 0, f"{drift} 把 Key 的额度和算法结果不一致"),
                oversell, base_check]

    cfg = quota_algo.DEFAULTS
    async def current(key: str):
        return (await _setting_json(state.db, quota_algo.STATE_KEY)).get(key)

    return Module(
        name="allocation", title="分配", question="每人每天能出多少张（所有人一样）",
        tick=tick, checks=checks, hard_note="关闭后额度冻结在当前值",
        params=[
            Param("今天每人 V4.5", lambda: current("ceiling"), "每天服务日起点后按昨天的用量重算"),
            Param("规则", lambda: "需求接近总量 → 按需求平分；有余量且有人用满 → +"
                  f"{cfg['quota_step']}；否则不变"),
            Param("范围", lambda: f"{cfg['quota_base_min_fair']}～{cfg['quota_ceiling_max']} 张"),
            Param("V5 每人范围", lambda: f"{cfg['quota_v5_min']}～{cfg['quota_v5_max']} 张", "按账号 V5 剩余和恢复速度分"),
        ])


# ---------------- 自动驾驶 ----------------
def autopilot_module(state) -> Module:
    from . import autopilot

    async def tick(k: Kernel):
        r = await autopilot.run(state, k.extra.get("registrar"))
        rules = r.get("rules") or {}
        guarded = len((rules.get("key_guard") or {}).get("value") or [])
        return f"全站熔断：{'触发' if (rules.get('breaker') or {}).get('value') else '没有'} · 被限流的 Key：{guarded} 把"

    return Module(
        name="autopilot", title="自动驾驶", question="名额、全站熔断、单个 Key 限流",
        tick=tick, hard_note="关闭后这些设置保持当前值",
        params=[Param("全站熔断", lambda: "15 分钟上游失败 ≥ 5 次且 ≥ 30% → 暂停出图 5 分钟"),
                Param("单个 Key 限流", lambda: "1 小时被拒 ≥ 60 次 → 暂停 1 小时"),
                Param("名额", lambda: f"快满且有余量时 +{autopilot.SLOTS_STEP}，最多 {autopilot.SLOTS_MAX}（名额不限时不动）")])


# ---------------- 领 Key ----------------
def registration(state) -> Module:
    async def get_enabled(k: Kernel):
        return await site_flags.get(state.db, site_flags.REGISTER_OPEN)

    async def set_enabled(k: Kernel, on: bool):
        await site_flags.put(state.db, site_flags.REGISTER_OPEN, on)

    async def checks(k: Kernel):
        reg = k.extra.get("registrar")
        if reg is None:
            return []
        cfg = await reg.settings()
        active = await reg.count_active()
        cap = cfg.get("max_users") or 0
        return [Check("已领人数 ≤ 名额", not cap or active <= cap, f"{active} / {cap or '不限'}")]

    return Module(
        name="registration", title="领 Key", question="新成员能不能领 Key",
        get_enabled=get_enabled, set_enabled=set_enabled, checks=checks,
        hard_note="关闭 = 暂停领取；已领的成员和邀请 Key 绑定不受影响",
        params=[Param("Key 有效天数", lambda: state.settings.default_expires_days),
                Param("闲置回收", lambda: f"{state.settings.key_inactivity_delete_days} 天"
                      if state.settings.key_inactivity_delete_days else "关")])


# ---------------- 开注册就绪度（2026-10-13）----------------
def registration_ready(state) -> Module:
    """回答「现在把领 Key 打开会发生什么」。

    独立于领 Key 开关常驻运行（关着也要能看到就绪度，否则打开之前这些数字看不见），
    只读：不改额度、不切开关、不动名额。
    """
    # 注册服务对象只在 kernel.extra 里（不是数据库设置），面板参数要读它，就在校验时记一份下来
    known: dict[str, Any] = {"reg": None}

    def _site_daily() -> int:
        """全站一天的出图容量 = 上游账号数 × 每账号日上限。"""
        tokens = [t for t in getattr(state.nai, "pool", []) if getattr(t, "usable", True)]
        return int(state.guard.values["account_daily_cap"]) * max(1, len(tokens))

    async def checks(k: Kernel):
        reg = k.extra.get("registrar")
        known["reg"] = reg
        open_now = await site_flags.get(state.db, site_flags.REGISTER_OPEN)
        if reg is None:
            # 服务没配起来：开关是关的就没事；开着的话成员会看到「领 Key 暂不可用」
            return [Check("注册链路已配置", not open_now,
                          "领 Key 开关是打开的，但网关没有注册服务配置（DISCORD_CLIENT_ID / "
                          "DISCORD_CLIENT_SECRET / DISCORD_BOT_TOKEN / REGISTRATION_BRIDGE_SECRET / "
                          "DISCORD_GUILD_ID / SITE_URL 缺项）：成员用 /register 只会看到「暂不可用」")]
        cfg = await reg.settings()
        active = await reg.count_active()
        cap = cfg.get("max_users") or 0
        per = cfg.get("daily_images") or 0
        v5_per = cfg.get("daily_v5") or 0
        tokens = [t for t in getattr(state.nai, "pool", []) if getattr(t, "usable", True)]
        site_daily = _site_daily()
        hourly = await site_flags.get(state.db, site_flags.ISSUE_HOURLY_CAP, state.settings)

        out = [Check("注册链路已配置", True,
                     f"{'开放中' if open_now else '当前关闭'}；配置齐备（网关能签发 Key，Key 通过临时消息发给本人）")]
        # ① 每人承诺 × 人数 vs 全站日容量：开注册前先看这一条 —— 它说明「名额能不能兑现」
        if per and site_daily:
            promise = active * per
            if await state.db.get_setting("ack_registration_ready", ""):
                # 先到先得是有意设计（站长已在后台确认）：转绿，数字仍显示
                out.append(Check("每人承诺 ≤ 全站日容量", True,
                                 f"站长已确认先到先得：每人 {per} 张 × {active} 人 = {promise} 张 vs "
                                 f"全站 {site_daily} 张/天（{len(tokens)} 个上游账号）"))
            else:
                out.append(Check("每人承诺 ≤ 全站日容量", promise <= site_daily,
                                 f"每人 {per} 张 × {active} 人 = {promise} 张 vs 全站 {site_daily} 张/天"
                                 f"（{len(tokens)} 个上游账号）：平分下来每人每天约 {site_daily // max(1, active)} 张，"
                                 f"先到先得；只有前约 {site_daily // max(1, per)} 个名额能拿满 {per} 张",
                                 level="warn"))
        # ② 领取速度 × 每人额度 = 每小时新增的边际需求，不能超过全站一天的量
        if hourly and per and site_daily:
            ramp = hourly * per
            out.append(Check("每小时领取上限 ≤ 全站日容量", ramp <= site_daily,
                             f"每小时最多发 {hourly} 个名额 × 每人 {per} 张 = {ramp} 张/小时的边际需求"
                             f"（全站 {site_daily} 张/天，约占 {round(ramp / site_daily * 100)}%）"))
        # ③ 后台额度 vs 环境变量：后台值被清掉时会跳回环境变量（可能是很早前写的大数）
        drift = []
        for label, db_value, env_value in (("每人张数", per, getattr(reg, "key_daily_images", 0)),
                                           ("每人 V5", v5_per, getattr(reg, "key_daily_v5", 0)),
                                           ("名额上限", cap, getattr(reg, "max_users", 0))):
            if env_value and db_value != env_value:
                drift.append(f"{label} 后台 {db_value} / 环境变量 {env_value}")
        out.append(Check("后台额度与环境变量一致", not drift,
                         "；".join(drift) + ("（后台值被清掉会跳到环境变量那个数）" if drift else ""),
                         level="warn"))
        return out

    async def _seats_text() -> str:
        """全站日容量 ÷ 每人额度 = 能拿满额度的人数（注册名额的现实上限）。"""
        reg = known["reg"]
        try:
            cfg = await reg.settings() if reg is not None else {}
            per = cfg.get("daily_images") or getattr(reg, "key_daily_images", 0) or 0
            daily = _site_daily()
            return f"约 {daily // per} 人（{daily} 张/天 ÷ 每人 {per} 张）" if per else "—"
        except Exception:
            return "—"

    async def _hourly_text() -> str:
        cap = await site_flags.get(state.db, site_flags.ISSUE_HOURLY_CAP, state.settings)
        return f"{cap} 个名额/小时" if cap else "不限"

    return Module(
        name="registration_ready", title="开注册就绪度",
        question="把领 Key 打开会发生什么：名额能兑现吗、链路通吗",
        checks=checks, hard_note="只读体检；不影响领 Key 开关、不改任何额度",
        params=[Param("每小时最多发放", _hourly_text),
                Param("容量能兑现的名额", _seats_text, "全站日容量 ÷ 每人额度：开注册前先看这一条")])


# ---------------- ⑥ 观测 ----------------
def observation(state) -> Module:
    async def checks(k: Kernel):
        # 生成记录（audit.py）和用量日志是两条独立的写入路径；「每日计数 ↔ 日志」在同一个事务里写，是恒等式
        db = state.db
        day = state.day()
        start = time.mktime(time.strptime(day, "%Y-%m-%d"))
        logged = int(await _q1(db, "SELECT COALESCE(SUM(images),0) FROM usage_log WHERE ts>=? AND status='ok' "
                                   "AND kind LIKE 'image%'", start) or 0)
        audited = int(await _q1(db, "SELECT COUNT(*) FROM generation_audit WHERE ts>=? AND status='ok'", start) or 0)
        if not audited:
            return [Check("生成记录 ↔ 用量日志", True, "生成记录未开启或今天还没有记录")]
        tol = max(2, logged * 0.005)
        return [Check("生成记录 ↔ 用量日志", abs(audited - logged) <= tol, f"生成记录 {audited} 条，日志 {logged} 张"),
                await _v5_rate_check()]

    async def _v5_rate_check() -> Check:
        """V5 恢复速度：接口读数和「每天余量变化 + 用掉的量」反推的实测值核对（2026-10-10 单次读数错了一倍）。"""
        try:
            last = json.loads(await state.db.get_setting("quota_algo_last", "{}") or "{}")
        except (TypeError, ValueError):
            last = {}
        rc = ((last.get("v5") or {}).get("rate_check")) or {}
        if not rc:
            return Check("V5 恢复速度：接口 ↔ 实测", True, "还没有数据")
        fmt = lambda v: "—" if v is None else f"{v:.1f}%/天"
        detail = f"接口 {fmt(rc.get('api'))} · 实测 {fmt(rc.get('measured'))} · 采用 {fmt(rc.get('used'))}（{rc.get('status')}）"
        return Check("V5 恢复速度：接口 ↔ 实测", rc.get("status") != "对不上", detail)

    return Module(
        name="observation", title="观测", question="各处记录的数字对不对得上",
        checks=checks, hard_note="关闭后不再核对；日志照常记录")


# ---------------- ③ 公平调度（DRR）----------------
# ---------------- 风险检查 ----------------
def risk(state) -> Module:
    """出站请求的样子和滥用线索（risk_check.py），每 10 分钟体检一次；只展示，不处罚、不记 Bug。"""
    from . import risk_check

    async def checks(k: Kernel):
        data = await risk_check.collect(state, time.time())
        return [Check(it["title"], True, it["detail"], level=it["status"], evidence=it["evidence"]) for it in data["items"]]

    return Module(name="risk", title="风险检查", question="出站请求像不像正常浏览器，有没有滥用迹象",
                  checks=checks, report_bugs=False, hard_note="关闭后不再体检；不影响任何处罚")


def build(state, bug) -> Kernel:
    k = Kernel(state, bug)
    for make in (observation, capacity, allocation, autopilot_module, risk,
                 registration, registration_ready):
        k.register(make(state))
    return k
