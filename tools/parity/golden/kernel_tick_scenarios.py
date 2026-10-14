"""内核整轮回放的场景库（用法见 kernel_tick.py）。全部是合成数据：假成员名、假 Token、2030 年的时间。

覆盖：动态额度每日微调的各个分支（数据不足 / 不变 / 放宽 / 按需求平分 / 节约折算 / 账号数 / 全站 V5）、
V5 方案（电池充沛 / 收紧 / 放宽 / 实测恢复速度 / 安静时段测量 / 对不上 / 超卖）、私密参数、异常路径（写了一半抛异常）、
自动驾驶（名额 / 熔断 / 单 Key 限流 的观察与执行）、模块内核（开关 / 运行条 / 交叉校验 → Bug / 容量快照与每小时上限加窗 /
领 Key 就绪度 / 风险检查）以及几组随机规模的整轮混合场景。
"""
from __future__ import annotations

import json

from _kernel_replay import (DAYS, HOUR, NOW, RESET_HOUR, TODAY, TOKENS, YDAY, Y0, Sc, T, token_id)

NEXT = NOW + 600                       # 下一个 10 分钟 tick
HOURLY_CAP = "hourly_cap"
Y1 = Y0 + DAYS                         # 昨天（服务日）的终点 = 今天 7 点
LOCAL_Y0 = T(2030, 3, 14)              # 昨天的本地 0 点（自动驾驶的 queue_full 窗口从这里起算）


# ---------------------------------------------------------------- 造数据的小工具
def members(sc: Sc, n: int, *, a: int = 150, v5: int = 10, first: int = 1, scope: str = "all") -> list[int]:
    ids = []
    for i in range(n):
        kid = first + i
        sc.seed.key(kid, f"成员{kid:02d}", daily_images=a, daily_v5=v5, scope=scope,
                    created_at=NOW - 20 * DAYS, last_image_at=NOW - sc.rng.randint(60, 3 * 86400))
        ids.append(kid)
    return ids


def extras(sc: Sc, first: int = 900) -> dict:
    """站长 Key / 测试 Key / 手动额度 Key：都不参与算法，手动 Key 也不被覆盖。"""
    s = sc.seed
    return {"admin": s.key(first, "站长", is_admin=1, quota_auto=-1, daily_images=999, daily_v5=99),
            "test": s.key(first + 1, "巡检", is_test=1, quota_auto=1, daily_images=20, daily_v5=0),
            "manual": s.key(first + 2, "手动成员", quota_auto=-1, daily_images=222, daily_v5=33, scope="legacy"),
            "off": s.key(first + 3, "已停用", enabled=0, daily_images=111, daily_v5=1)}


def coverage(sc: Sc, ago: float = 3 * DAYS) -> None:
    """日志从很早以前就有：昨天算「数据完整」。"""
    sc.seed.log(Y0 - ago, None, kind="text", model="", images=0)


def usage(sc: Sc, ids: list[int], profile: str, a: int, *, day: str = YDAY, v5_each: int = 0) -> list[int]:
    """昨天每人的 V4.5 用量（counters.legacy_free_images）。返回各人用量。"""
    r = sc.rng
    out = []
    for i, kid in enumerate(ids):
        if profile == "light":
            u = r.randint(0, int(0.4 * a))
        elif profile == "one_full":
            u = a if i < 2 else r.randint(0, int(0.4 * a))
        elif profile == "heavy":
            u = r.randint(int(0.5 * a), int(1.4 * a))
        elif profile == "mixed":
            u = r.choice([0, 0, r.randint(1, 30), r.randint(20, int(0.9 * a)), a])
        else:
            raise AssertionError(profile)
        v5 = v5_each if (v5_each and i % 2 == 0) else 0
        sc.seed.counter(kid, day, u + v5, u, v5)
        out.append(u)
    return out


def yday_logs(sc: Sc, ids: list[int], per: int = 2, *, v5_rows: int = 0, blocked_hours: int = 0, base_block: int = 0,
              paced: int = 0, start: float = Y0) -> None:
    """昨天服务日里的日志（都不早于 `start`，这样 MIN(ts) 由场景自己定）：成功出图 + 几类被拒（每小时上限 / 保底 / 平滑放行）。"""
    r = sc.rng
    span = Y1 - start - 1
    for kid in ids:
        for _ in range(per):
            sc.seed.log(start + r.uniform(0, span), kid, images=r.randint(1, 3))
    for i in range(v5_rows):
        kid = ids[i % len(ids)]
        steps = r.choice([28, 28, 14, 20, 40])
        sc.seed.log(start + r.uniform(0, span), kid, model="nai-diffusion-5-full", images=1,
                    detail=f"1024x1024/{steps}step")
    for h in range(blocked_hours):
        sc.seed.log(start + (h + 1) * HOUR + 5, ids[h % len(ids)], status="rejected", images=0,
                    detail="本小时出图量已达上限，请稍后再试", reason=HOURLY_CAP, up_status=0)
    for _ in range(base_block):
        sc.seed.log(start + r.uniform(0, span), ids[0], status="rejected", images=0,
                    detail="已超出保底，其他人在排队", reason="over_base", up_status=0)
    for _ in range(paced):
        sc.seed.log(start + r.uniform(0, span), ids[0], status="rejected", images=0, detail="平滑放行", reason="paced",
                    up_status=0)


def hist_entry(day: str, *, used=300, budget=850, cap=1000, hourly_blocks=0, coverage_hours=24.0, base_blocks=0,
               ceiling_hits=0, paced_blocks=0, v5_pct=None, v5_rate=None, v5_rate_measured=None, a=150) -> dict:
    e = {"day": day, "used": used, "ceiling_hits": ceiling_hits, "hourly_blocks": hourly_blocks,
         "base_blocks": base_blocks, "paced_blocks": paced_blocks, "coverage_hours": coverage_hours, "cap": cap,
         "budget": budget, "from": [a, a], "to": [a, a], "reasons": ["synthetic"]}
    if v5_pct is not None:
        e["v5_pct"] = v5_pct
    if v5_rate is not None:
        e["v5_rate"] = v5_rate
    if v5_rate_measured is not None:
        e["v5_rate_measured"] = v5_rate_measured
    return e


def cached(*accounts) -> dict:
    return {"accounts": list(accounts), "cached": True, "client": True, "resolve_fails": False}


def base_quota(name: str, desc: str, *, n: int = 14, a: int = 150, profile: str = "light", tokens: int = 1, full=True,
               allowance=None, ext: bool = True, **kw) -> Sc:
    """一个有成员、有昨天用量、数据完整的场景（还没有步骤）。"""
    sc = Sc(name, desc, tokens=tokens, allowance=allowance, **kw)
    ids = members(sc, n, a=a)
    if ext:
        extras(sc)
    if full:
        coverage(sc)
    usage(sc, ids, profile, a)
    yday_logs(sc, ids, start=Y0 if full else Y0 + 13 * HOUR)
    sc.ids = ids
    return sc


def run_quota(sc: Sc, *ats: float) -> Sc:
    for at in ats or (NOW,):
        sc.step("quota_run", at)
    return sc


# ================================================================ 动态额度
def quota_scenarios() -> list[Sc]:
    out: list[Sc] = []

    sc = base_quota("q01_low_coverage", "昨天只有 11 小时数据：每人额度不变，只复盘不调整", full=False)
    sc.seed.log(Y0 + 13 * HOUR, None, kind="text", model="", images=0)
    out.append(run_quota(sc))

    sc = base_quota("q02_nobody_full", "有余量且没人用满：不变（但仍写复盘、同步 Key）", profile="light")
    out.append(run_quota(sc))

    sc = base_quota("q03_someone_full_plus25", "有余量，两个人用满 150：放宽 +25", profile="one_full")
    out.append(run_quota(sc))

    sc = base_quota("q04_heavy_waterfill", "总需求超过可分量 95%：按需求平分（可降可升）", n=20, profile="heavy")
    out.append(run_quota(sc))

    sc = base_quota("q05_two_accounts", "两个上游账号：日上限 ×2，可分量翻倍", n=22, profile="heavy", tokens=2)
    out.append(run_quota(sc))

    sc = base_quota("q06_guard_cap_800", "站长把账号日上限调成 800", n=18, profile="heavy")
    sc.seed.setting("guard_account_daily_cap", 800)
    out.append(run_quota(sc))

    sc = base_quota("q07_global_v5_400", "全站 V5 上限设成 400：可分量 = 1000 − 400", n=12, profile="heavy")
    sc.seed.setting("global_daily_v5", 400)
    out.append(run_quota(sc))

    sc = base_quota("q08_economy_units", "节约模式（14 步算半张）：用量按算力折算后再比", n=16, profile="heavy", tokens=1)
    sc.seed.upstream_counter(TOKENS[0], YDAY, 1200, 0, units=600.0)
    out.append(run_quota(sc))

    sc = base_quota("q09_manual_and_test_excluded", "站长 / 测试 / 手动 / 停用 Key 不参与也不被覆盖", n=10, profile="one_full")
    for kid in (5, 6):                                   # 两个成员手动定额
        sc.seed.sql.append(f"UPDATE api_keys SET quota_auto=-1, daily_images=77 WHERE id={kid}")
    out.append(run_quota(sc))

    sc = base_quota("q10_algo_disabled", "总开关关闭：只返回 enabled=false，不读不写")
    sc.seed.setting("quota_auto_enabled", 0)
    out.append(run_quota(sc))

    sc = base_quota("q11_same_day_again", "今天已经微调过：不再复盘，只同步 Key 与 V5", profile="one_full",
                    allowance=cached({"percent": 66, "recharge_per_day": 5.5}))
    sc.seed.setting("quota_algo_day", TODAY)
    sc.seed.setting("quota_ceiling", 175)
    sc.seed.setting("quota_base_now", 175)
    sc.seed.setting_json("quota_v5_day_plan", {"day": TODAY, "plan": {"rate": 5.5, "percent": 66, "k": 1.1, "global": 85,
                                                                       "people": 10, "each": 8}})
    out.append(run_quota(sc, NOW, NEXT))

    sc = base_quota("q12_ceiling_max", "已经 300（上限）还有人用满：放宽被钳到 300", profile="one_full")
    sc.seed.setting("quota_ceiling", 300)
    sc.seed.setting("quota_base_now", 300)
    out.append(run_quota(sc))

    sc = base_quota("q13_floor_min_fair", "可分量只剩 10：平分结果被钳到最低 10", n=15, profile="heavy")
    sc.seed.setting("global_daily_v5", 990)
    out.append(run_quota(sc))

    sc = base_quota("q14_string_settings", "设置值是 '150.0' / '175.5' 这种字符串：按 int(float()) 读", profile="one_full")
    sc.seed.setting("quota_ceiling", "150.0")
    sc.seed.setting("quota_base_now", "150")
    sc.seed.setting("quota_target_avg", "175.5")
    sc.seed.setting("quota_step", "30.9")
    out.append(run_quota(sc))

    sc = base_quota("q15_bad_history_json", "额度历史不是合法 JSON：抛 JSONDecodeError，之前没有任何写入", profile="one_full")
    sc.seed.setting("quota_algo_history", "{not json")
    out.append(run_quota(sc))

    sc = base_quota("q16_bad_v5_plan_after_writes", "V5 方案坏了：复盘已经写进库之后才抛异常，已写入的保留",
                    profile="one_full", allowance=cached({"percent": 80, "recharge_per_day": 5.0}))
    sc.seed.setting("quota_v5_day_plan", "[1, 2")
    out.append(run_quota(sc))

    sc = base_quota("q17_bad_ceiling", "quota_ceiling 读不懂：ValueError")
    sc.seed.setting("quota_ceiling", "abc")
    out.append(run_quota(sc))

    sc = base_quota("q18_inf_setting", "quota_step 是 inf：OverflowError（_cfg 不捕获）")
    sc.seed.setting("quota_step", "inf")
    out.append(run_quota(sc))

    sc = base_quota("q19_history_trim", "复盘历史已经 35 条：只保留最近 29 条再追加", profile="one_full")
    sc.seed.setting_json("quota_algo_history", [hist_entry(f"2030-02-{d:02d}", v5_pct=60.0) for d in range(1, 36)])
    out.append(run_quota(sc))

    sc = base_quota("q20_empty_members", "没有任何成员 Key（只有站长 Key）", n=0, ext=True)
    out.append(run_quota(sc))

    sc = base_quota("q21_window_boundaries", "昨天窗口的边界：恰好等于起点的算、等于终点的不算（被拦小时数 / 保底拦截 / 平滑拦截）",
                    profile="one_full")
    for ts in (Y0 - 1, Y0, Y0 + 3 * HOUR, Y1 - 1, Y1, Y1 + 1):
        sc.seed.log(ts, 3, status="rejected", images=0, detail="本小时出图量已达上限", reason=HOURLY_CAP, up_status=0)
        sc.seed.log(ts, 4, status="rejected", images=0, detail="保底：其他人在排队", reason="over_base", up_status=0)
        sc.seed.log(ts, 4, status="rejected", images=0, detail="x", reason="paced", up_status=0)
    out.append(run_quota(sc))

    sc = base_quota("q22_v5_rows_boundary", "昨天 V5 日志的窗口边界（用来反推实测恢复速度）：起点算、终点不算",
                    profile="one_full", allowance=cached({"percent": 70, "recharge_per_day": 5.0}))
    for ts, steps in ((Y0 - 1, 28), (Y0, 14), (Y1 - 1, 28), (Y1, 28), (Y0 + HOUR, 40)):
        sc.seed.log(ts, 2, model="nai-diffusion-5-full", images=2, detail=f"1024x1024/{steps}step")
    sc.seed.log(Y0 + 2 * HOUR, 2, model="nai-diffusion-5-full", images=0, detail="")          # images 为 0 当 1 张、没有步数当 28
    sc.seed.log(Y0 + 3 * HOUR, 2, model="nai-diffusion-5-full", images=1, detail="/0step")    # 步数 0 钳到 1
    sc.seed.log(Y0 + 4 * HOUR, 2, model="nai-diffusion-5-full", status="error", images=1, detail="/28step")  # 失败的不算
    sc.seed.setting_json("quota_algo_history", [hist_entry("2030-03-13", v5_pct=60.0)])
    out.append(run_quota(sc))

    sc = base_quota("q23_quiet_boundaries", "安静时段测量的边界：72 小时前恰好的快照不算；相邻快照间 V5 日志 (t0, t1] 左开右闭；间隔恰好 3 小时算",
                    allowance=cached({"percent": 64, "recharge_per_day": 5.0}))
    t = NOW - 72 * HOUR
    for i in range(0, 10):
        sc.seed.snapshot(t + i * 3 * HOUR if i else t, 50.0 + i, 5.0, 1)         # 间隔恰好 3 小时
    sc.seed.log(t + 3 * HOUR, 1, model="nai-diffusion-5-full")                       # 恰好在第二条快照上：算在第一对里
    sc.seed.log(t + 9 * HOUR + 1, 1, model="nai-diffusion-5-full")
    out.append(run_quota(sc))
    return out


# ================================================================ V5 方案
def v5_scenarios() -> list[Sc]:
    out: list[Sc] = []

    def mk(name, desc, allowance, *, profile="light", n=14, tokens=1, **kw) -> Sc:
        sc = base_quota(name, desc, n=n, profile=profile, allowance=allowance, tokens=tokens, **kw)
        return sc

    sc = mk("v01_flood_92", "电池 92%：充沛，每人 ≥ 10、全站 ≥ 260", cached({"percent": 92, "recharge_per_day": 6.4}))
    sc.seed.setting("quota_algo_day", TODAY)
    out.append(run_quota(sc))

    sc = mk("v02_mid_75", "电池 75%：系数 1.1", cached({"percent": 75, "recharge_per_day": 6.4}))
    out.append(run_quota(sc))

    sc = mk("v03_tighten_below_40", "当天已定每人 20，电池跌到 30%：当天收紧", cached({"percent": 30, "recharge_per_day": 6.0}))
    sc.seed.setting("quota_algo_day", TODAY)
    sc.seed.setting_json("quota_v5_day_plan", {"day": TODAY, "plan": {"rate": 6.0, "percent": 70, "k": 1.1, "global": 400,
                                                                       "people": 10, "each": 20}})
    out.append(run_quota(sc))

    sc = mk("v04_loosen_same_day", "当天已定每人 3，现在电池 95%：当天放宽", cached({"percent": 95, "recharge_per_day": 7.0}))
    sc.seed.setting("quota_algo_day", TODAY)
    sc.seed.setting_json("quota_v5_day_plan", {"day": TODAY, "plan": {"rate": 5.0, "percent": 50, "k": 0.9, "global": 60,
                                                                       "people": 20, "each": 3}})
    out.append(run_quota(sc))

    sc = mk("v05_stored_plan_kept", "当天方案更大、电池也没跌破 40%：沿用早上的方案（只刷新电池读数）",
            cached({"percent": 60, "recharge_per_day": 5.0}))
    sc.seed.setting("quota_algo_day", TODAY)
    sc.seed.setting_json("quota_v5_day_plan", {"day": TODAY, "plan": {"rate": 5.0, "percent": 80, "k": 1.1, "global": 300,
                                                                       "people": 10, "each": 25}})
    out.append(run_quota(sc))

    sc = mk("v06_no_reading", "上游一个读数都没有（也没有客户端）：剩余 None，按保守速度", None)
    out.append(run_quota(sc))

    sc = mk("v07_refresh_after_restart", "刚重启缓存里没有读数：主动读一次订阅信息再取",
            {"accounts": [{"percent": 55, "recharge_per_day": 5.0}], "cached": False, "client": True,
             "resolve_fails": False})
    out.append(run_quota(sc))

    sc = mk("v08_refresh_fails", "主动读失败：仍然没有读数",
            {"accounts": [{"percent": 55, "recharge_per_day": 5.0}], "cached": False, "client": True,
             "resolve_fails": True})
    out.append(run_quota(sc))

    sc = mk("v09_three_accounts_average", "三个账号：剩余 / 速度取平均，没读数的账号不算",
            cached({"percent": 80, "recharge_per_day": 6.5}, {"percent": 60, "recharge_per_day": None}, None),
            tokens=3)
    out.append(run_quota(sc))

    sc = mk("v10_float_percent", "剩余是小数（77.5）与整数混合求平均", cached({"percent": 77.5, "recharge_per_day": 5.25},
                                                                  {"percent": 70, "recharge_per_day": 6}), tokens=2)
    out.append(run_quota(sc))

    sc = mk("v11_measured_from_history", "每日复盘用「余量变化 + 昨天 V5 用量（按步数折算）」反推实测恢复速度",
            cached({"percent": 71, "recharge_per_day": 5.4}), profile="one_full")
    yday_logs(sc, sc.ids, v5_rows=40)
    sc.seed.setting_json("quota_algo_history", [hist_entry("2030-03-11", v5_pct=58.0, v5_rate=5.0),
                                                hist_entry("2030-03-12", v5_pct=63.4, v5_rate=5.2, v5_rate_measured=5.1),
                                                hist_entry("2030-03-13", v5_pct=66.0, v5_rate=5.3)])
    out.append(run_quota(sc, NOW, NEXT))

    sc = mk("v12_quiet_recovery", "安静时段（相邻快照之间没人用 V5）直接量到恢复速度，优先采用",
            cached({"percent": 64, "recharge_per_day": 5.0}))
    for i in range(7):                                    # 每小时一条主动读的快照，稳步上涨
        sc.seed.snapshot(NOW - 30 * 60 - (6 - i) * HOUR, 50.0 + i * 0.5, 5.0, 1)
    sc.seed.snapshot(NOW - 40 * HOUR, 40.0, 5.0, 1)         # 间隔 > 3 小时：这一对不算
    sc.seed.snapshot(NOW - 20 * HOUR, 55.0, 5.0, 0)         # 不是主动读的：不用
    out.append(run_quota(sc))

    sc = mk("v13_quiet_with_usage_between", "快照之间有 V5 出图的小时不算安静",
            cached({"percent": 64, "recharge_per_day": 5.0}))
    for i in range(8):
        sc.seed.snapshot(NOW - 30 * 60 - (7 - i) * HOUR, 50.0 + i * 0.4, 5.0, 1)
    sc.seed.log(NOW - 30 * 60 - 5 * HOUR - 600, 1, model="nai-diffusion-5-full", detail="1024x1024/28step")
    sc.seed.log(NOW - 30 * 60 - 2 * HOUR - 600, 2, model="nai-diffusion-5-full", detail="1024x1024/28step")
    out.append(run_quota(sc))

    sc = mk("v14_rate_mismatch", "接口速度 9.0 与实测 5.0 对不上：取较小的", cached({"percent": 72, "recharge_per_day": 9.0}))
    sc.seed.setting("quota_algo_day", TODAY)
    sc.seed.setting_json("quota_algo_history", [hist_entry("2030-03-14", v5_pct=60.0, v5_rate_measured=5.0)])
    out.append(run_quota(sc))

    sc = mk("v15_rate_out_of_range", "接口读数 20%/天、实测 0.5%/天都超出合理范围：都不用",
            cached({"percent": 72, "recharge_per_day": 20.0}))
    sc.seed.setting("quota_algo_day", TODAY)
    sc.seed.setting_json("quota_algo_history", [hist_entry("2030-03-14", v5_pct=60.0, v5_rate_measured=0.5)])
    out.append(run_quota(sc))

    sc = mk("v16_api_rate_capped_unverified", "没有实测值核对：接口速度 12 被压到保守值 5", cached({"percent": 88, "recharge_per_day": 12.0}))
    out.append(run_quota(sc))

    sc = mk("v17_active_people", "最近 3 天 14 人用过 V5：分母按 14 人而不是最少 10 人",
            cached({"percent": 85, "recharge_per_day": 8.0}), n=20)
    v5_people(sc, 14)
    sc.seed.setting("quota_algo_day", TODAY)
    out.append(run_quota(sc))

    sc = mk("v18_oversell_notice", "每人额度 × 人数 > 全站上限：首页提示「先到先得」", cached({"percent": 50, "recharge_per_day": 5.0}),
            n=40)
    sc.seed.setting("quota_algo_day", TODAY)
    out.append(run_quota(sc))

    sc = mk("v19_private_params", "私密参数覆盖：系数表 / 保底系数 / 最少人数 / 覆盖小时 / 充沛线 / 收紧线",
            cached({"percent": 62, "recharge_per_day": 6.0}),
            params={"allocation.v5_k_table": [[80, 2.0], [50, 1.0]], "allocation.v5_k_floor": "0.5",
                    "allocation.min_people": 5, "allocation.min_coverage_hours": 10, "allocation.v5_flood_pct": 60,
                    "allocation.v5_flood_min_each": 8, "allocation.v5_flood_global": 100,
                    "allocation.v5_tighten_below": 55}, full=False)
    sc.seed.log(Y0 + 13 * HOUR, None, kind="text", model="", images=0)        # 覆盖 11 小时，但私密参数要求 ≥ 10
    out.append(run_quota(sc))

    sc = mk("v20_anlas_notice", "站长设了每人每日 Anlas：首页提示带上", cached({"percent": 86, "recharge_per_day": 6.0}))
    sc.seed.setting("member_daily_anlas", 25)
    out.append(run_quota(sc))

    sc = mk("v21_paced_note_100", "每人 100（≤ 注水桶上限 126）：首页提示不带「每小时自动恢复」",
            cached({"percent": 66, "recharge_per_day": 6.0}))
    sc.seed.setting("quota_ceiling", 100)
    sc.seed.setting("quota_base_now", 100)
    sc.seed.setting("quota_algo_day", TODAY)
    out.append(run_quota(sc))

    sc = mk("v22_guard_base_saved", "保底张数（guard）与算法结果不一致：同步写 guard_base_daily_images",
            cached({"percent": 66, "recharge_per_day": 6.0}), profile="one_full")
    sc.seed.setting("guard_base_daily_images", 90)
    out.append(run_quota(sc, NOW, NEXT))

    sc = mk("v23_review_without_reading", "复盘当天上游没有读数：复盘里不记 v5_pct", None, profile="one_full")
    out.append(run_quota(sc))

    sc = mk("v24_zero_min_people", "私密参数 min_people=0 且没有活跃 V5 成员：除零 ZeroDivisionError",
            cached({"percent": 66, "recharge_per_day": 6.0}), params={"allocation.min_people": 0})
    out.append(run_quota(sc))
    return out


def v5_people(sc: Sc, n: int) -> None:
    """n 个成员最近 3 个自然日用过 V5（counters 的 day 是服务日字符串，这里直接给自然日）。"""
    days = ["2030-03-15", "2030-03-14", "2030-03-13"]
    for i in range(n):
        kid = sc.ids[i]
        day = days[i % 3]
        exists = any(s.startswith("INSERT INTO counters") and f"VALUES ({kid}, '{day}'" in s for s in sc.seed.sql)
        if exists:
            sc.seed.sql.append(f"UPDATE counters SET v5=v5+1 WHERE key_id={kid} AND day='{day}'")
        else:
            sc.seed.counter(kid, day, 1, 0, 1)


# ================================================================ 自动驾驶
def ap_base(name: str, desc: str, *, registrar=None, hist=None, last_v5=None, n: int = 10, mode: dict | None = None) -> Sc:
    sc = Sc(name, desc, tokens=1,
            registrar=registrar if registrar is not None else {"max_users_env": 20, "daily_images_env": 100,
                                                                "daily_v5_env": 10, "active": 19})
    sc.ids = members(sc, n)
    coverage(sc)
    sc.seed.setting_json("quota_algo_history", [hist if hist is not None else hist_entry(YDAY)])
    if last_v5 is not None:
        sc.seed.setting_json("quota_algo_last", {"enabled": True, "v5": {"percent": last_v5}})
    for rule, m in (mode or {}).items():
        sc.seed.setting(f"autopilot_{rule}", m)
    return sc


def ap_run(sc: Sc, *ats: float) -> Sc:
    for at in ats or (NOW,):
        sc.step("autopilot_run", at)
    return sc


def keyguard(name, desc, count, mode, reason="param", **kw) -> Sc:
    sc = ap_base(name, desc, mode=mode)
    for i in range(count):
        sc.seed.log(NOW - 3000 + i * 30, 5, status="rejected", images=0, reason=reason, detail="参数不对", up_status=0,
                    name="成员05")
    for i in range(10):                                # 另一把 Key 次数不够
        sc.seed.log(NOW - 2000 + i, 6, status="rejected", images=0, reason=reason, up_status=0)
    sc.seed.log(NOW - 7200, 7, status="rejected", images=0, reason=reason, up_status=0)  # 一小时之外
    return sc


def autopilot_scenarios() -> list[Sc]:
    out: list[Sc] = []
    out.append(ap_run(ap_base("ap01_observe_default", "默认观察模式：名额算出 +5 但不执行"), NOW))
    out.append(ap_run(ap_base("ap02_slots_enforce", "名额 enforce：满 + 余量 → 执行 +5，写设置 / 冷却 / 操作日志",
                              mode={"slots": "enforce"}), NOW, NOW + 3600))
    sc = ap_base("ap03_slots_cooldown", "名额 enforce 但 24 小时内刚加过：不执行", mode={"slots": "enforce"})
    sc.seed.setting("autopilot_slots_at", repr(NOW - 3600))
    out.append(ap_run(sc))
    out.append(ap_run(ap_base("ap04_slots_unlimited", "名额不限（max_users=0）：不调整",
                              registrar={"max_users_env": 0, "daily_images_env": 100, "daily_v5_env": 10, "active": 30},
                              mode={"slots": "enforce"})))
    out.append(ap_run(ap_base("ap05_low_coverage", "昨天只有 11 小时数据：名额不变", hist=hist_entry(YDAY, coverage_hours=11.0),
                              mode={"slots": "enforce"})))
    out.append(ap_run(ap_base("ap06_no_registrar", "没有注册服务：按名额不限走",
                              registrar=False, mode={"slots": "enforce"})))
    out.append(ap_run(ap_base("ap07_v5_low", "账号 V5 只剩 40%：名额不再增加", last_v5=40, mode={"slots": "enforce"})))
    out.append(ap_run(ap_base("ap08_hourly_blocks", "昨天 3 个小时被每小时上限拦：名额不再增加",
                              hist=hist_entry(YDAY, hourly_blocks=3), mode={"slots": "enforce"})))
    sc = ap_base("ap09_queue_hours", "昨天 3 个小时排队满被拒 ≥ 5 次：名额不再增加（窗口是本地 0 点起 24 小时）",
                 mode={"slots": "enforce"})
    for h in (3, 9, 15):
        for j in range(6):
            sc.seed.log(LOCAL_Y0 + h * HOUR + j * 10, sc.ids[j], status="rejected", images=0, reason="queue_full",
                        detail="排队已满", up_status=0)
    for j in range(4):                                     # 只有 4 次的小时不算
        sc.seed.log(LOCAL_Y0 + 20 * HOUR + j, sc.ids[j], status="rejected", images=0, reason="queue_full", up_status=0)
    out.append(ap_run(sc))
    out.append(ap_run(ap_base("ap10_util_high", "昨天用量 70%：名额不再增加", hist=hist_entry(YDAY, used=595),
                              mode={"slots": "enforce"})))
    out.append(ap_run(ap_base("ap11_cap_reached", "名额已到 100：不再加",
                              registrar={"max_users_env": 100, "daily_images_env": 100, "daily_v5_env": 10, "active": 99},
                              mode={"slots": "enforce"})))
    out.append(ap_run(ap_base("ap12_budget_missing", "复盘里没有 budget：用 cap − 全站 V5 兜底",
                              hist={k: v for k, v in hist_entry(YDAY).items() if k != "budget"},
                              mode={"slots": "enforce"})))

    def breaker(name, desc, fails, total, mode, **kw) -> Sc:
        sc = ap_base(name, desc, mode=mode)
        for i in range(total):
            bad = i < fails
            sc.seed.log(NOW - 600 + i * 20, sc.ids[i % len(sc.ids)], status="error" if bad else "ok",
                        images=0 if bad else 1, up_status=502 if bad else 200, detail="upstream 502" if bad else "")
        for _ in range(3):                                 # 不计入的：本地拦截 / 429 / 4xx
            sc.seed.log(NOW - 500, sc.ids[0], status="error", images=0, up_status=0)
            sc.seed.log(NOW - 400, sc.ids[0], status="error", images=0, up_status=429)
            sc.seed.log(NOW - 300, sc.ids[0], status="error", images=0, up_status=422)
        sc.seed.log(NOW - 1800, sc.ids[0], status="error", images=0, up_status=500)      # 15 分钟之外
        return sc

    sc = breaker("ap13_breaker_enforce_trip", "最近 15 分钟 6/10 次上游失败：熔断 5 分钟（enforce）", 6, 10,
                 {"breaker": "enforce"})
    out.append(ap_run(sc).step("probe", NOW))
    sc = breaker("ap14_breaker_observe", "同样的失败，观察模式：只记「如果是我会熔断」", 6, 10, {"breaker": "observe"})
    out.append(ap_run(sc).step("probe", NOW))
    out.append(ap_run(breaker("ap15_breaker_below", "失败 4/10：不熔断", 4, 10, {"breaker": "enforce"})))
    out.append(ap_run(breaker("ap16_breaker_ratio_low", "失败 5/40 占比不到 30%：不熔断", 5, 40, {"breaker": "enforce"})))
    sc = breaker("ap17_breaker_since_until", "上次熔断结束于 5 分钟前：只看那之后的数据", 6, 10, {"breaker": "enforce"})
    sc.seed.setting("autopilot_breaker_until", repr(NOW - 300))
    out.append(ap_run(sc))
    sc = breaker("ap18_breaker_already_tripped", "已经在熔断中：不重复执行", 6, 10, {"breaker": "enforce"})
    sc.step("trip_breaker", NOW - 1, seconds=900)
    out.append(ap_run(sc).step("probe", NOW))
    out.append(ap_run(breaker("ap19_breaker_off", "breaker=off（当观察处理）：不执行", 6, 10, {"breaker": "off"})))
    out.append(ap_run(breaker("ap20_bad_mode_value", "模式设成不认识的值：回到 observe", 6, 10, {"breaker": "ENFORCE"})))

    sc = keyguard("ap21_keyguard_enforce_pause", "1 小时被拒 70 次：暂停 1 小时（enforce）", 70, {"key_guard": "enforce"})
    out.append(ap_run(sc).step("probe", NOW))
    sc = keyguard("ap22_keyguard_observe", "同样 70 次，观察模式：只记录", 70, {"key_guard": "observe"})
    out.append(ap_run(sc).step("probe", NOW))
    out.append(ap_run(keyguard("ap23_keyguard_below", "被拒 59 次：不触发", 59, {"key_guard": "enforce"})))
    sc = keyguard("ap24_keyguard_already_paused", "这把 Key 已经在暂停中：不重复暂停", 70, {"key_guard": "enforce"})
    sc.seed.pause(5, NOW + 1800, "上一次的暂停")
    out.append(ap_run(sc).step("probe", NOW))
    out.append(ap_run(keyguard("ap25_keyguard_site_level", "被拒原因都是全站层面（排队满）：不算成员的错", 70,
                               {"key_guard": "enforce"}, reason="queue_full")))
    out.append(ap_run(keyguard("ap26_keyguard_key_paused_reason", "被拒原因是「Key 暂停」本身：不算", 70,
                               {"key_guard": "enforce"}, reason="key_paused")))
    sc = keyguard("ap27_all_enforce", "三条规则同时 enforce、同时触发", 70,
                  {"slots": "enforce", "breaker": "enforce", "key_guard": "enforce"})
    for i in range(10):
        sc.seed.log(NOW - 600 + i * 20, sc.ids[i % 4], status="error" if i < 6 else "ok", images=0 if i < 6 else 1,
                    up_status=504 if i < 6 else 200)
    out.append(ap_run(sc).step("probe", NOW))
    sc = keyguard("ap32_keyguard_boundary_in", "59 次 + 1 次刚好在一小时内：恰好 60 次触发暂停", 59, {"key_guard": "enforce"})
    sc.seed.log(NOW - 3599, 5, status="rejected", images=0, reason="param", up_status=0)
    out.append(ap_run(sc).step("probe", NOW))
    sc = keyguard("ap33_keyguard_boundary_out", "59 次 + 1 次刚好在一小时之外：不触发", 59, {"key_guard": "enforce"})
    sc.seed.log(NOW - 3601, 5, status="rejected", images=0, reason="param", up_status=0)
    out.append(ap_run(sc))
    sc = ap_base("ap34_breaker_boundary", "15 分钟窗口边界：899 秒前算、901 秒前不算（5 次失败刚好触发）", mode={"breaker": "enforce"})
    for i, ts in enumerate((NOW - 899, NOW - 600, NOW - 500, NOW - 400, NOW - 300)):
        sc.seed.log(ts, sc.ids[i], status="error", images=0, up_status=502)
    sc.seed.log(NOW - 901, sc.ids[0], status="error", images=0, up_status=503)
    sc.seed.log(NOW - 100, sc.ids[1], status="ok", images=1)
    out.append(ap_run(sc).step("probe", NOW))
    sc = ap_base("ap35_breaker_midstream_error", "200 开头流中途断开（up_status 2xx 且 status=error）也算上游失败", mode={"breaker": "enforce"})
    for i in range(6):
        sc.seed.log(NOW - 700 + i * 10, sc.ids[i], status="error", images=0, up_status=200 + i * 10 if i < 5 else 299)
    sc.seed.log(NOW - 300, sc.ids[0], status="error", images=0, up_status=300)
    sc.seed.log(NOW - 200, sc.ids[0], kind="text", status="error", images=0, up_status=500)       # 不是出图
    out.append(ap_run(sc).step("probe", NOW))
    sc = ap_base("ap28_hourly_history", "每小时留一条快照：同一小时不重复，下个小时追加，保留 168 条")
    sc.seed.setting_json("autopilot_history", [{"hour": f"2030-02-{1 + i // 24:02d} {i % 24:02d}:00", "slots": 0,
                                                "breaker": False, "key_guard": 0} for i in range(170)])
    out.append(ap_run(sc, NOW, NOW + 600, NOW + 3700))
    sc = ap_base("ap29_bad_autopilot_history", "快照历史不是合法 JSON：写完 autopilot_last 之后才抛异常")
    sc.seed.setting("autopilot_history", "{bad")
    out.append(ap_run(sc))
    sc = ap_base("ap30_bad_quota_history", "额度历史坏了：自动驾驶一开始就抛异常")
    sc.seed.setting("quota_algo_history", "oops")
    out.append(ap_run(sc))
    sc = ap_base("ap31_empty_history", "额度历史是空列表：没有复盘数据", hist=None)
    sc.seed.setting("quota_algo_history", "[]")
    out.append(ap_run(sc))
    return out


# ================================================================ 模块内核
def k_base(name: str, desc: str, *, tokens: int = 2, n: int = 12, registrar=None, allowance=None, **kw) -> Sc:
    sc = Sc(name, desc, tokens=tokens, registrar=registrar, allowance=allowance or cached(
        {"percent": 78, "recharge_per_day": 6.0}, {"percent": 74, "recharge_per_day": 6.2}), **kw)
    sc.ids = members(sc, n)
    extras(sc)
    coverage(sc)
    usage(sc, sc.ids, "one_full", 150)
    yday_logs(sc, sc.ids)
    return sc


def tick(sc: Sc, *ats: float) -> Sc:
    for at in ats or (NOW,):
        sc.step("tick_all", at)
    return sc


def kernel_scenarios() -> list[Sc]:
    out: list[Sc] = []
    reg = {"max_users_env": 30, "daily_images_env": 100, "daily_v5_env": 10, "active": 12}

    sc = k_base("k01_three_ticks", "三轮 tick：第一轮记快照 + 复盘，第二轮 10 分钟后，第三轮超过 1 小时再记快照", registrar=reg)
    out.append(tick(sc, NOW, NEXT, NOW + 3700).step("snapshot", NOW + 3700))

    sc = k_base("k02_module_disabled", "分配模块被关：只记 enabled=false，其它照常；运行条只给启用的模块", registrar=reg)
    sc.seed.setting("module_allocation", 0)
    sc.seed.setting("module_autopilot", "0")
    out.append(tick(sc, NOW, NEXT))

    sc = k_base("k03_registration_open", "领 Key 开着：registration 模块启用并跑校验（名额占用 ≤ 上限）",
                registrar={**reg, "active": 31})
    sc.seed.setting("register_open", 1)
    out.append(tick(sc))

    sc = k_base("k04_registration_open_no_registrar", "领 Key 开着但没有注册服务：就绪度校验报「未配置」", registrar=None)
    sc.seed.setting("register_open", "true")
    out.append(tick(sc))

    sc = k_base("k05_ready_overbooked", "开注册就绪度：人数 × 每人张数超过全站日容量；每小时上限；环境变量漂移",
                registrar={"max_users_env": 40, "daily_images_env": 100, "daily_v5_env": 20, "active": 45})
    sc.seed.setting("register_max_users", 50)
    sc.seed.setting("register_daily_images", 150)
    sc.seed.setting("issue_hourly_cap", 30)
    out.append(tick(sc).step("snapshot", NOW))

    sc = k_base("k06_ready_acked", "同样的就绪度，站长已确认「先到先得」：转绿但数字仍显示",
                registrar={"max_users_env": 40, "daily_images_env": 100, "daily_v5_env": 20, "active": 45})
    sc.seed.setting("register_daily_images", 150)
    sc.seed.setting("ack_registration_ready", "1")
    out.append(tick(sc).step("snapshot", NOW))

    sc = k_base("k07_failing_checks_become_bugs", "交叉校验不通过：记 warn 级 Bug（不私信）；同步漂移 / 内存计数偏少 / 观测对不上",
                registrar=reg)
    sc.seed.setting_json("quota_algo_last", {"enabled": True, "ceiling": 150, "base": 150,
                                             "v5": {"each": 7, "global": 30, "percent": 70.0,
                                                    "rate_check": {"api": 5.0, "measured": 9.0, "used": 5.0, "status": "对不上"}}})
    for i in range(30):
        sc.seed.log(NOW - 60 - i * 30, 1 + i % 5, images=2)         # 最近 1 小时 60 张，内存里没有记
    for i in range(8):
        sc.seed.audit(T(2030, 3, 15, 8) + i * 60, 1 + i)
    out.append(tick(sc, NOW, NEXT))

    sc = k_base("k08_capacity_adapt_hourly", "过去 24 小时顶到过每小时上限且没有 429：每小时上限 +10（并记操作日志）", registrar=reg)
    sc.seed.setting("guard_account_hourly_cap", 140)
    sc.seed.setting("guard_hourly_adapted_at", repr(NOW - 30 * HOUR))
    for i in range(3):
        sc.seed.log(NOW - 5 * HOUR - i * 100, 2, status="rejected", images=0, reason=HOURLY_CAP, up_status=0)
    out.append(tick(sc).step("probe", NOW))

    sc = k_base("k09_capacity_adapt_first_run", "第一次运行只开始计时：上线当天不算「平稳了一天」", registrar=reg)
    out.append(tick(sc).step("probe", NOW))

    sc = k_base("k10_capacity_adapt_capped_after_429", "挨过 429 之后加窗的天花板压到当时上限的 80%", registrar=reg)
    sc.seed.setting("guard_account_hourly_cap", 90)
    sc.seed.setting("guard_hourly_adapted_at", repr(NOW - 30 * HOUR))
    sc.seed.setting("guard_429_cap_at", 150)
    sc.seed.setting("guard_last_upstream_429", repr(NOW - 3 * DAYS))
    sc.seed.log(NOW - 4 * HOUR, 2, status="rejected", images=0, reason=HOURLY_CAP, up_status=0)
    out.append(tick(sc).step("probe", NOW))

    sc = k_base("k11_no_usable_tokens", "所有上游账号都停用 / 冷却：容量校验只剩「没有可用的上游账号」；风险检查照常",
                registrar=reg, token_state={"0": {"admin_disabled": True}, "1": {"blocked_for": 1800}})
    out.append(tick(sc))

    sc = k_base("k12_capacity_v5_check", "容量：上游 V5 消耗 ↔ 日志 V5 张数（≥ 6 小时快照、剩余 < 95%）", registrar=reg,
                guard_starts=[[0, NOW - 600, 3], [1, NOW - 1200, 2], [0, NOW - 2400, 5]])
    for i in range(9):
        sc.seed.snapshot(NOW - 8 * HOUR + i * HOUR, 90.0 - i * 2.0, 5.0, 1)
    for i in range(60):
        sc.seed.log(NOW - 8 * HOUR + 60 + i * 450, 1 + i % 6, model="nai-diffusion-5-full", images=1)
    out.append(tick(sc))

    sc = k_base("k13_capacity_v5_check_fails", "同样的数据但日志张数离谱（每 1% 远小于 7 张）", registrar=reg)
    for i in range(9):
        sc.seed.snapshot(NOW - 8 * HOUR + i * HOUR, 90.0 - i * 2.0, 5.0, 1)
    for i in range(10):
        sc.seed.log(NOW - 7 * HOUR + i * 900, 1, model="nai-diffusion-5-full", images=1)
    out.append(tick(sc))

    sc = k_base("k14_observation_ok", "观测：生成记录 ↔ 用量日志对得上；V5 恢复速度「已核对」", registrar=reg)
    sc.seed.setting_json("quota_algo_last", {"enabled": True, "ceiling": 150, "v5": {"each": 10, "global": 150,
                                             "rate_check": {"api": 5.0, "measured": 5.1, "used": 5.0, "status": "已核对"}}})
    for i in range(6):
        sc.seed.log(T(2030, 3, 15, 9) + i * 60, 1 + i, images=1)
        sc.seed.audit(T(2030, 3, 15, 9) + i * 60 + 1, 1 + i)
    out.append(tick(sc))

    sc = k_base("k15_allocation_oversell", "分配校验：超卖（每人 × 人数 > 全站）→ 校验不通过；确认后转绿", registrar=reg)
    sc.seed.setting_json("quota_algo_last", {"enabled": True, "ceiling": 150, "base": 150,
                                             "v5": {"each": 20, "global": 130, "percent": 60}})
    sc.seed.sql.append("UPDATE api_keys SET daily_images=150, daily_v5=20 WHERE quota_auto=1")
    sc.seed.setting("module_allocation", 0)                               # 只看校验，不让 tick 重算额度
    out.append(tick(sc, NOW))
    sc2 = k_base("k16_allocation_oversell_acked", "同上，站长已确认超卖是有意的", registrar=reg)
    sc2.seed.setting_json("quota_algo_last", {"enabled": True, "ceiling": 150, "base": 150,
                                              "v5": {"each": 20, "global": 130, "percent": 60}})
    sc2.seed.sql.append("UPDATE api_keys SET daily_images=150, daily_v5=20 WHERE quota_auto=1")
    sc2.seed.setting("module_allocation", 0)
    sc2.seed.setting("ack_v5_oversell", "1")
    out.append(tick(sc2, NOW))

    sc = k_base("k17_risk_items", "风险检查：用量集中度 ≥ 50%、V5 消耗接近日额度、上游 429 增多", registrar=reg)
    for i in range(30):
        sc.seed.log(NOW - 3600 * 5 - i * 60, 3, images=1, name="成员03")
    for i in range(10):
        sc.seed.log(NOW - 3600 * 6 - i * 60, 1 + i % 4, images=1, name=f"成员{1 + i % 4:02d}")
    for i in range(8):                                                    # 最近 1 小时 429
        sc.seed.log(NOW - 1800 + i * 60, 4, status="error", images=0, up_status=429, detail="429")
    sc.seed.upstream_counter(TOKENS[0], TODAY, 60, 70)
    sc.seed.upstream_counter(TOKENS[1], TODAY, 40, 25)
    sc.seed.setting_json("quota_algo_last", {"enabled": True, "ceiling": 150, "v5": {"each": 10, "global": 100}})
    out.append(tick(sc))

    sc = k_base("k18_risk_no_data", "风险检查：近 24 小时没有出图、令牌池有账号但今日没用", registrar=reg)
    sc.seed.sql.append("DELETE FROM usage_log")
    out.append(tick(sc))

    sc = k_base("k19_preloaded_kernel_last", "启动时读回上次的运行记录：运行条累加到 36 轮，不认识的模块名原样保留", registrar=reg)
    runs = [{"at": NOW - (40 - i) * 600.0, "level": "ok"} for i in range(40)]
    sc.seed.setting_json("kernel_last", {
        "capacity": {"enabled": True, "at": NOW - 600.0, "ok": True, "ms": 12, "summary": "每小时上限 150", "error": "",
                     "runs": runs, "checks": [{"name": "旧校验", "ok": True, "detail": "", "level": "ok", "evidence": []}]},
        "retired_module": {"enabled": True, "runs": runs[:3]},
        "autopilot": {"enabled": False}})
    out.append(tick(sc).step("snapshot", NOW))

    sc = k_base("k20_set_enabled_steps", "后台开关：关闭分配 / 打开领 Key（对接 register_open），每次立即再跑一轮", registrar=reg)
    sc.step("kernel_load", NOW)
    sc.step("set_enabled", NOW + 1, name="allocation", on=False)
    tick(sc, NOW + 2)
    sc.step("set_enabled", NOW + 3, name="registration", on=True)
    tick(sc, NOW + 4)
    sc.step("set_enabled", NOW + 5, name="allocation", on=True)
    sc.step("set_enabled", NOW + 6, name="registration", on=False)
    tick(sc, NOW + 7).step("snapshot", NOW + 8)
    out.append(sc)

    sc = k_base("k21_all_modules_off", "所有模块都关：只记 enabled=false", registrar=reg)
    for m in ("observation", "capacity", "allocation", "autopilot", "risk", "registration_ready"):
        sc.seed.setting(f"module_{m}", 0)
    out.append(tick(sc))

    sc = k_base("k22_module_error_loose", "分配模块的 tick 抛异常（额度历史坏了）：记 ok=false + error，其它模块不受影响",
                registrar=reg)
    sc.seed.setting("quota_algo_history", "{broken")
    sc.loose_errors = True
    out.append(tick(sc))

    sc = k_base("k23_enforce_slots_via_tick", "自动驾驶 enforce 名额：tick 里真的改了 register_max_users，下一轮的领 Key 校验读到新值",
                registrar={**reg, "max_users_env": 12, "active": 11})
    sc.seed.setting("register_open", 1)
    sc.seed.setting("autopilot_slots", "enforce")
    sc.seed.setting_json("quota_algo_history", [hist_entry(YDAY)])
    out.append(tick(sc, NOW, NEXT))

    sc = k_base("k24_snapshot_idle_panels", "没有任何设置 / 运行记录时的模块面板（含运行时改过的出图间隔 → 整数）", registrar=reg)
    sc.seed.setting("runtime_image_min_interval", 20)
    sc.step("snapshot", NOW)
    out.append(sc)

    sc = k_base("k26_log_window_boundaries", "容量 / 观测的统计边界：最近 60 分钟 ts>now-3600（不含）；今天的统计 ts>=本地 0 点（含）",
                registrar=reg)
    t0 = T(2030, 3, 15)
    for ts in (NOW - 3601, NOW - 3600, NOW - 3599):
        sc.seed.log(ts, 1, images=3)
    for ts in (t0 - 1, t0, t0 + 1):
        sc.seed.log(ts, 2, images=5)
        sc.seed.audit(ts, 2)
    sc.seed.audit(t0 + 2, 2, status="error")
    out.append(tick(sc))

    sc = k_base("k25_snapshot_default_interval", "出图间隔没改过：面板显示环境变量的浮点数", registrar=None,
                env={"image_min_interval": 18.5})
    sc.step("snapshot", NOW)
    out.append(sc)
    return out


# ================================================================ 随机规模的整轮混合场景
def mixed_scenarios() -> list[Sc]:
    out: list[Sc] = []
    for i in range(10):
        sc = Sc(f"m{i + 1:02d}_mixed", f"随机规模整轮 #{i + 1}", tokens=1 + i % 3, seed=i,
                registrar={"max_users_env": 10 + 5 * (i % 4), "daily_images_env": 100, "daily_v5_env": 10,
                           "active": 8 + 4 * (i % 4)})
        r = sc.rng
        n = r.randint(6, 48)
        a = r.choice([100, 125, 150, 175, 200])
        sc.ids = members(sc, n, a=a, v5=r.choice([5, 8, 10, 15]))
        extras(sc)
        cut = Y0
        if r.random() < 0.85:
            coverage(sc)
        else:
            cut = Y0 + r.randint(6, 18) * HOUR
            sc.seed.log(cut, None, kind="text", model="", images=0)
        profile = ["light", "one_full", "heavy", "mixed", "mixed"][i % 5]
        usage(sc, sc.ids, profile, a, v5_each=r.choice([0, 1, 2]))
        yday_logs(sc, sc.ids, per=r.randint(1, 3), v5_rows=r.randint(0, 25), blocked_hours=r.randint(0, 4),
                  base_block=r.randint(0, 3), paced=r.randint(0, 3), start=cut)
        if r.random() < 0.6:
            v5_people(sc, min(n, r.randint(3, 16)))
        sc.seed.setting("quota_ceiling", a)
        sc.seed.setting("quota_base_now", a)
        if r.random() < 0.5:
            sc.seed.setting("guard_account_daily_cap", r.choice([600, 800, 1000, 1200]))
        if r.random() < 0.5:
            sc.seed.setting("global_daily_v5", r.choice([100, 150, 200, 300]))
        if r.random() < 0.4:
            sc.seed.setting_json("quota_algo_history", [hist_entry(f"2030-03-{d:02d}", v5_pct=round(r.uniform(40, 90), 1),
                                                                   v5_rate_measured=round(r.uniform(3, 8), 2), a=a)
                                                         for d in range(9, 14)])
        pct = [round(r.uniform(15, 98)) for _ in range(sc.tokens.__len__())]
        sc.allowance = cached(*[{"percent": p, "recharge_per_day": round(r.uniform(3, 9), 1)} for p in pct])
        if r.random() < 0.5:
            for h in range(r.randint(2, 9)):
                sc.seed.snapshot(NOW - (h + 1) * HOUR, round(r.uniform(50, 80), 1), 5.0, 1)
        for rule in ("slots", "breaker", "key_guard"):
            if r.random() < 0.5:
                sc.seed.setting(f"autopilot_{rule}", "enforce")
        for _ in range(r.randint(0, 80)):
            sc.seed.log(NOW - r.uniform(0, 900), r.choice(sc.ids), status=r.choice(["ok", "ok", "error", "rejected"]),
                        images=1, up_status=r.choice([200, 200, 502, 504, 0, 429]), reason="param")
        tick(sc, NOW, NEXT, NOW + 1800, NOW + 3700)
        out.append(sc)
    # 跨过服务日切换点（7 点）连续运行：06:55 → 07:05
    sc = k_base("m11_day_rollover", "服务日切换前后连续 tick：切换后才复盘新的一天", registrar={"max_users_env": 20, "daily_images_env": 100,
                                                                           "daily_v5_env": 10, "active": 12})
    sc.seed.setting("quota_algo_day", YDAY)
    out.append(tick(sc, T(2030, 3, 15, 6, 55), T(2030, 3, 15, 7, 5), T(2030, 3, 15, 7, 15)))
    return out


def build_scenarios() -> list[Sc]:
    out = quota_scenarios() + v5_scenarios() + autopilot_scenarios() + kernel_scenarios() + mixed_scenarios()
    for sc in out:
        if sc.registrar is False:
            sc.registrar = None
    return out
