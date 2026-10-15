import { describe, expect, it } from "vitest";
import type { Job } from "../system/jobs-core";
import { buildReminders, computeVerdict, type Reminder } from "./reminders";
import type { Overview } from "./types";

const NOW = 1_800_000_000;

const ov = (p: Partial<Overview> = {}): Overview => ({
  today: { images: 0, member_images: 0, v5: 0 },
  keys_total: 1,
  keys_active: 1,
  pool: [{ position: 1, admin_enabled: true, disabled: false }],
  pool_configured: true,
  v5_limit: 0,
  feature_usage: [],
  log_since: null,
  ...p,
});
const base = { bot: null, bugs: null, jobs: null, now: NOW };
const job = (title: string, last: Job["last"]): Job => ({
  name: title,
  title,
  every: "每 10 分钟",
  every_secs: 600,
  last,
});

describe("buildReminders", () => {
  it("一切正常时没有提醒", () => {
    expect(buildReminders({ ...base, overview: ov() })).toEqual([]);
  });

  it("上游 Token 失效 / 没配置 是紧急，key 稳定", () => {
    const r = buildReminders({
      ...base,
      overview: ov({ pool: [{ position: 2, admin_enabled: true, disabled: true }], pool_configured: false }),
    });
    expect(r.map((x) => x.key)).toEqual(["up-acct-2-dead", "up-notconfigured"]);
    expect(r.every((x) => x.level === "bad")).toBe(true);
  });

  it("停用的上游账号不提醒", () => {
    const r = buildReminders({
      ...base,
      overview: ov({ pool: [{ position: 1, admin_enabled: false, disabled: true }] }),
    });
    expect(r).toEqual([]);
  });

  it("Bug：有错误级别 → 紧急，只有注意级别 → 注意", () => {
    const errs = buildReminders({
      ...base,
      overview: ov(),
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
      bugs: { open: 1, errors: [{ level: "warn", resolved_at: null }] },
    });
    expect(warns[0]?.level).toBe("warn");
  });

  it("机器人离线、定时任务出错或超时（还没跑过的不算），链接到系统页的定时任务卡片", () => {
    const run = (at: number, ok = true) => ({ at, ok, ms: 42, summary: ok ? "完成" : "数据库锁超时" });
    const r = buildReminders({
      ...base,
      overview: ov(),
      bot: { status: { online: false } },
      jobs: {
        jobs: [
          job("动态额度", run(NOW - 60, false)),
          job("全站熔断", run(NOW - 3 * 600 - 1)),
          job("维护", run(NOW - 3 * 600)),
          job("闲置回收", null),
        ],
      },
    });
    expect(r.map((x) => x.key)).toEqual(["discord", "jobs"]);
    expect(r[1]).toMatchObject({
      level: "warn",
      title: "2 个定时任务上次出错或超时没运行",
      detail: "动态额度、全站熔断",
      to: "/admin/system#jobs",
    });
  });

  it("定时任务都正常时不提醒", () => {
    const r = buildReminders({
      ...base,
      overview: ov(),
      jobs: { jobs: [job("动态额度", { at: NOW - 60, ok: true, ms: 1, summary: "" }), job("维护", null)] },
    });
    expect(r).toEqual([]);
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
    expect(computeVerdict([])).toMatchObject({ tone: "ok", headline: "一切正常" });
  });
  it("有紧急项 → 红；只有注意项 → 黄；标题数的是要处理的件数", () => {
    expect(computeVerdict([item("a", "bad"), item("b", "warn")])).toMatchObject({
      tone: "bad",
      headline: "有 2 件事要处理",
    });
    expect(computeVerdict([item("b", "warn")]).tone).toBe("warn");
  });
});
