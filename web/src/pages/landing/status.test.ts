import { describe, expect, it } from "vitest";
import { createEstimator, dailyUsage, deriveStatus } from "./status";
import type { Live } from "./types";

/**
 * 下面的 API 原文逐字来自 crates/gate-core/src/guard.rs（出图限量的拒绝原因）与 crates/gate-server/src/auth.rs（图片冷却）：
 * 成员收到的 429 原因和 /public/live 的 guard.blocked.text 是同一句。首页的说法必须和它同一口径（站长规定）。
 */
const API = {
  quietHard: "现在是安静时段（3:00–7:00），暂停出图，7 点恢复",
  quietSlow: "现在是安静时段（3:00–7:00），出图放慢到每小时 20 张，约 12 分钟后有空位",
  daily: "本站今天的出图总量已达上限（每个账号 1000 张/天，用来保护上游账号），明天 7 点恢复",
  paced:
    "全站出图按时段放行（每天 1000 张平均分到 24 小时，保证晚来的人也有图出），这个时段的份额已用完，约 5 分钟后有空位",
  hourly:
    "最近 60 分钟的出图量已达上限（滚动窗口，全站每小时 150 张，不是整点重置），约 12 分钟后随最早的图滑出窗口逐张恢复",
  breaker: "上游连续出错，已暂停出图，约 30 秒后自动恢复",
  cooldown: "上游图片服务限流保护中，所有图片生成暂停约 45 秒",
  queueFull: "当前排队的人太多（全站最多同时排 5 张），请稍后再试",
};

const idle = (over: Partial<Live> = {}): Live => ({
  quota: { images_today: 10, images_cap: 1000, v5_today: 2, v5_cap: 63 },
  queue: { waiting: 0, running: 0 },
  guard: { hour: 8, hour_cap: 150, cooldown: 0, blocked: null },
  rules: { queue_cap: 5, hourly_cap: 150, quiet_start: 3, quiet_end: 7, quiet_cap: 0 },
  interval: 2.5,
  me: null,
  ...over,
});
const blocked = (code: string, text: string, over: Partial<Live> = {}): Live =>
  idle({ guard: { hour: 8, hour_cap: 150, blocked: { code, text } }, ...over });

const sentence = (live: Live | null) => deriveStatus(live)?.sentence;

describe("deriveStatus：正常与排队", () => {
  it("没有数据时返回 null（页面显示骨架屏）", () => {
    expect(deriveStatus(null)).toBeNull();
  });

  it("空闲：马上开始 · 通道空闲", () => {
    const v = deriveStatus(idle());
    expect(v?.tone).toBe("ok");
    expect(v?.sentence).toBe("马上开始 · 通道空闲");
  });

  it("有人排队：按 人数 × 放行间隔 估算，并说出前面有几张", () => {
    const v = deriveStatus(idle({ queue: { waiting: 3, running: 1 }, interval: 15 }), { nowMs: 0 });
    expect(v?.tone).toBe("busy");
    expect(v?.headline).toBe("约 1 分钟后开始");
    expect(v?.detail).toBe("前面有 4 张在等");
    // 读屏播报不含倒计时
    expect(v?.announce).toBe("排队中 · 前面有 4 张在等");
  });

  it("没有放行间隔时不编秒数", () => {
    const v = deriveStatus(idle({ queue: { waiting: 2, running: 0 }, interval: 0 }));
    expect(v?.sentence).toBe("排队中 · 前面有 2 张在等");
  });

  it("排队已满", () => {
    const v = deriveStatus(idle({ queue: { waiting: 5, running: 1 } }));
    expect(v?.tone).toBe("warn");
    expect(v?.headline).toBe("排队已满");
    expect(v?.detail).toContain("最多同时排 5 张");
  });
});

describe("deriveStatus：与 API 的 429 原因同一句话", () => {
  it("上游冷却：同 check_image_cooldown", () => {
    const v = deriveStatus(idle({ guard: { cooldown: 45 } }));
    expect(v?.tone).toBe("bad");
    expect(v?.sentence).toBe("上游图片服务限流保护中 · 所有图片生成暂停约 45 秒");
    expect(API.cooldown).toContain("暂停约 45 秒");
  });

  it("安静时段整体暂停：安静时段 · 暂停出图 · 7 点恢复", () => {
    expect(sentence(blocked("quiet_cap", API.quietHard))).toBe("安静时段 · 暂停出图 · 7 点恢复");
  });

  it("安静时段只是放慢：每小时 20 张，约 12 分钟后有空位（不能说成暂停到 7 点）", () => {
    const s = sentence(blocked("quiet_cap", API.quietSlow));
    expect(s).toBe("安静时段 · 出图放慢 · 每小时 20 张，约 12 分钟后有空位");
    expect(s).not.toContain("暂停");
  });

  it("每日上限：明天 7 点恢复", () => {
    expect(sentence(blocked("daily_cap", API.daily))).toBe("今日出图已满 · 明天 7 点恢复");
  });

  it("每日上限的恢复整点取自接口原文，不写死", () => {
    expect(sentence(blocked("daily_cap", API.daily.replace("明天 7 点", "明天 5 点")))).toBe(
      "今日出图已满 · 明天 5 点恢复",
    );
  });

  it("按时段放行：约 5 分钟后有空位", () => {
    const v = deriveStatus(blocked("paced", API.paced));
    expect(v?.tone).toBe("paced");
    expect(v?.sentence).toBe("本时段份额已用完 · 全站出图按时段放行，约 5 分钟后有空位");
  });

  it("最近 60 分钟（滚动窗口）上限：数字与原文一致", () => {
    const v = deriveStatus(blocked("hourly_cap", API.hourly));
    expect(v?.headline).toBe("最近 60 分钟已到上限");
    expect(v?.detail).toBe("滚动窗口，全站每小时 150 张，不是整点重置，约 12 分钟后逐张恢复");
  });

  it("熔断：原文照搬", () => {
    const v = deriveStatus(blocked("breaker", API.breaker));
    expect(v?.tone).toBe("bad");
    expect(v?.sentence).toBe(API.breaker);
  });

  it("不认识的原因码：原文照搬，不改写", () => {
    expect(sentence(blocked("something_new", "新规则：请稍后再试"))).toBe("新规则：请稍后再试");
  });

  it("小时用量到顶但网关没给 blocked：同样说滚动窗口", () => {
    const v = deriveStatus(idle({ guard: { hour: 150, hour_cap: 150 } }));
    expect(v?.headline).toBe("最近 60 分钟已到上限");
    expect(v?.detail).toContain("滚动窗口，全站每小时 150 张，不是整点重置");
  });

  it("优先级：冷却 > 全站拦截 > 排队满 > 小时到顶 > 排队", () => {
    const base = {
      queue: { waiting: 9, running: 0 },
      guard: {
        cooldown: 10,
        hour: 150,
        hour_cap: 150,
        blocked: { code: "paced", text: API.paced },
      },
    };
    expect(deriveStatus(idle(base))?.headline).toBe("上游图片服务限流保护中");
    expect(deriveStatus(idle({ ...base, guard: { ...base.guard, cooldown: 0 } }))?.headline).toBe(
      "本时段份额已用完",
    );
    expect(
      deriveStatus(idle({ ...base, guard: { ...base.guard, cooldown: 0, blocked: null } }))?.headline,
    ).toBe("排队已满");
  });
});

describe("deriveStatus：我的图优先", () => {
  it("我的图正在生成", () => {
    const v = deriveStatus(idle({ me: { mine: [{ state: "running" }], waiting: 0, running: 1 } }), {
      avgDurS: 6.2,
    });
    expect(v?.tone).toBe("mine");
    expect(v?.sentence).toBe("你的图正在生成 · 平均约 6.2 秒");
  });

  it("我的图排第 N 位，估算时间取接口的 eta", () => {
    const v = deriveStatus(
      idle({ me: { mine: [{ state: "waiting", position: 3, eta: 45 }], waiting: 3, running: 1 } }),
    );
    expect(v?.sentence).toBe("你的图排第 3 位 · 约 45 秒后开始画");
    expect(v?.announce).toBe("你的图排第 3 位");
  });

  it("即使全站被拦下，已经排上队的图仍显示自己的位置（与旧版一致）", () => {
    const v = deriveStatus(
      blocked("hourly_cap", API.hourly, {
        me: { mine: [{ state: "waiting", position: 1 }], waiting: 1, running: 0 },
      }),
    );
    expect(v?.headline).toBe("你的图排第 1 位");
  });
});

describe("createEstimator", () => {
  it("人数变了重新估，之后每秒倒数，最少 2 秒", () => {
    const est = createEstimator();
    expect(est(4, 15, 0)).toBe(60);
    expect(est(4, 15, 10_000)).toBe(50);
    expect(est(4, 15, 59_000)).toBe(2);
    expect(est(4, 15, 120_000)).toBe(2);
    expect(est(2, 15, 121_000)).toBe(30);
  });
  it("队伍清空后归零，再来人重新估", () => {
    const est = createEstimator();
    expect(est(3, 10, 0)).toBe(30);
    expect(est(0, 10, 5000)).toBe(0);
    expect(est(3, 10, 9000)).toBe(30);
  });
});

describe("dailyUsage", () => {
  it("按实际张数；上限 0 = 不限", () => {
    const u = dailyUsage(idle({ quota: { images_today: 10, images_cap: 0, v5_today: 3, v5_cap: 0 } }));
    expect(u.total).toEqual({ used: 10, cap: 0 });
    expect(u.v5).toEqual({ used: 3, cap: 0 });
  });
  it("没有数据全是 0", () => {
    expect(dailyUsage(idle({ quota: { images_today: 7, images_cap: 1000 } })).total).toEqual({
      used: 7,
      cap: 1000,
    });
    expect(dailyUsage(null)).toEqual({ total: { used: 0, cap: 0 }, v5: { used: 0, cap: 0 } });
  });
});
