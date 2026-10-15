import { describe, expect, it } from "vitest";
import {
  buildReminders,
  computeVerdict,
  HIDDEN_KEY,
  hideReminder,
  type Reminder,
  readHidden,
  restoreReminders,
} from "./reminders";
import type { Overview, StatusResp } from "./types";

const NOW_MS = 1_800_000_000_000;
const H = 3600 * 1000;

function memStore(init: Record<string, string> = {}) {
  const m = new Map(Object.entries(init));
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  };
}

const ov = (p: Partial<Overview> = {}): Overview => ({
  today: {
    images: 0,
    member_images: 0,
    anlas: 0,
    text_tokens: 0,
    requests: 0,
    v5: 0,
    unconfirmed_requests: 0,
    unconfirmed_anlas: 0,
  },
  keys_total: 1,
  keys_active: 1,
  week: [],
  month: { anlas: 0, unconfirmed_requests: 0, unconfirmed_anlas: 0 },
  pool: [{ token_id: "t1", position: 1, admin_enabled: true, disabled: false, blocked_for: 0 }],
  pool_configured: true,
  anlas_budget: 0,
  v5_limit: 0,
  feature_usage: [],
  log_since: null,
  ...p,
});
const status = (p: Partial<StatusResp> = {}): StatusResp => ({
  alerts: { configured: true, sent: 0 },
  audit: {
    prompts: false,
    thumbs: false,
    retention_days: 7,
    image_cap_mb: 0,
    image_bytes: 0,
    image_keep_hours: 0,
  },
  upstream: { status: "ok", recent: 10, failed: 0, image_cooldown_seconds: 0 },
  protection: { auth_fail_max: 20, auth_fail_window: 600, auth_block_seconds: 900, login_max_attempts: 5 },
  disk: { free_mb: 50, total_mb: 100 },
  ...p,
});
const base = { bot: null, bugs: null, modules: null, members: null, now: NOW_MS / 1000 };

describe("buildReminders", () => {
  it("一切正常时没有提醒", () => {
    expect(buildReminders({ ...base, overview: ov(), status: status() })).toEqual([]);
  });

  it("上游限流 / 失效 / 没配置 都是紧急，key 稳定", () => {
    const r = buildReminders({
      ...base,
      overview: ov({
        pool: [{ token_id: "t1", position: 2, admin_enabled: true, disabled: true, blocked_for: 0 }],
        pool_configured: true,
      }),
      status: status({ upstream: { status: "degraded", recent: 20, failed: 9, image_cooldown_seconds: 0 } }),
    });
    expect(r.map((x) => x.key)).toEqual(["up-degraded", "up-acct-2-dead"]);
    expect(r.every((x) => x.level === "bad")).toBe(true);
    const cool = buildReminders({
      ...base,
      overview: ov(),
      status: status({ upstream: { status: "ok", recent: 1, failed: 0, image_cooldown_seconds: 40 } }),
    });
    expect(cool[0]?.key).toBe("up-cooldown");
    expect(cool[0]?.title).toContain("40 秒");
  });

  it("停用的上游账号不提醒；限流中是注意级别", () => {
    const r = buildReminders({
      ...base,
      overview: ov({
        pool: [
          { token_id: "a", position: 1, admin_enabled: false, disabled: true, blocked_for: 0 },
          { token_id: "b", position: 2, admin_enabled: true, disabled: false, blocked_for: 30 },
        ],
      }),
      status: status(),
    });
    expect(r.map((x) => [x.key, x.level])).toEqual([["up-acct-2-blocked", "warn"]]);
  });

  it("Bug：有错误级别 → 紧急，只有注意级别 → 注意", () => {
    const errs = buildReminders({
      ...base,
      overview: ov(),
      status: status(),
      bugs: {
        open: 2,
        errors: [
          { level: "error", resolved_at: null },
          { level: "warn", resolved_at: null },
        ],
      },
    });
    expect(errs[0]).toMatchObject({ key: "bugs", level: "bad", to: "/admin/logs/bugs" });
    const warns = buildReminders({
      ...base,
      overview: ov(),
      status: status(),
      bugs: { open: 1, errors: [{ level: "warn", resolved_at: null }] },
    });
    expect(warns[0]?.level).toBe("warn");
  });

  it("磁盘低于 20% 注意、低于 10% 紧急；Anlas 预算 80% 起提醒", () => {
    const d1 = buildReminders({
      ...base,
      overview: ov(),
      status: status({ disk: { free_mb: 15, total_mb: 100 } }),
    });
    expect(d1[0]).toMatchObject({ key: "disk", level: "warn", title: "磁盘只剩 15%" });
    const d2 = buildReminders({
      ...base,
      overview: ov(),
      status: status({ disk: { free_mb: 5, total_mb: 100 } }),
    });
    expect(d2[0]?.level).toBe("bad");
    const b = buildReminders({
      ...base,
      overview: ov({
        anlas_budget: 1000,
        month: { anlas: 850, unconfirmed_requests: 0, unconfirmed_anlas: 0 },
      }),
      status: status(),
    });
    expect(b[0]).toMatchObject({ key: "anlas-budget", title: "本月 Anlas 用了 85%" });
  });

  it("机器人离线、模块自检失败", () => {
    const r = buildReminders({
      ...base,
      overview: ov(),
      status: status(),
      bot: { status: { online: false } },
      modules: {
        modules: [
          { enabled: true, last: { ok: false } },
          { enabled: false, last: { ok: false } },
        ],
      },
    });
    expect(r.map((x) => x.key)).toEqual(["discord", "modules"]);
    expect(r[1]?.title).toBe("1 个模块自检没通过");
  });
});

describe("隐藏记录（24 小时，按稳定 key）", () => {
  it("隐藏后 24 小时内读得到，之后自动失效", () => {
    const s = memStore();
    hideReminder("bugs", NOW_MS, s);
    expect(Object.keys(readHidden(NOW_MS + 23 * H, s))).toEqual(["bugs"]);
    expect(readHidden(NOW_MS + 25 * H, s)).toEqual({});
  });
  it("恢复会清空；存储损坏或不可用时当作没有", () => {
    const s = memStore();
    hideReminder("a", NOW_MS, s);
    restoreReminders(s);
    expect(readHidden(NOW_MS, s)).toEqual({});
    expect(readHidden(NOW_MS, memStore({ [HIDDEN_KEY]: "{坏了" }))).toEqual({});
    expect(readHidden(NOW_MS, null)).toEqual({});
    expect(() => hideReminder("x", NOW_MS, null)).not.toThrow();
  });
});

describe("computeVerdict", () => {
  const item = (key: string, level: Reminder["level"]): Reminder => ({
    key,
    level,
    title: key,
    detail: "",
    to: "/",
    label: "",
  });

  it("没有任何提醒：一切正常（绿）", () => {
    expect(computeVerdict([], {})).toMatchObject({ tone: "ok", headline: "一切正常", hiddenCount: 0 });
  });
  it("有紧急项 → 红；只有注意项 → 黄；标题数的是要处理的件数", () => {
    expect(computeVerdict([item("a", "bad"), item("b", "warn")], {})).toMatchObject({
      tone: "bad",
      headline: "有 2 件事要处理",
    });
    expect(computeVerdict([item("b", "warn")], {}).tone).toBe("warn");
  });
  it("提示级别的不算要处理，仍是绿", () => {
    expect(computeVerdict([item("i", "info")], {})).toMatchObject({ tone: "ok", headline: "一切正常" });
  });
  it("被隐藏的提醒不能让页面变绿：全部隐藏 → 黄，标题说明已隐藏", () => {
    const v = computeVerdict([item("a", "bad")], { a: NOW_MS });
    expect(v).toMatchObject({ tone: "warn", headline: "提醒都已隐藏", hiddenCount: 1 });
    expect(v.visible).toEqual([]);
  });
  it("隐藏一部分：剩下的照常计数；隐藏的只有提示级别则仍可以是绿", () => {
    const v = computeVerdict([item("a", "bad"), item("b", "warn")], { a: NOW_MS });
    expect(v).toMatchObject({ tone: "warn", headline: "有 1 件事要处理", hiddenCount: 1 });
    expect(computeVerdict([item("i", "info")], { i: NOW_MS }).tone).toBe("ok");
  });
});
