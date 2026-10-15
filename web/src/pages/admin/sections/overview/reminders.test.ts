import { describe, expect, it } from "vitest";
import type { Mod } from "../modules/core";
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
const base = { bot: null, bugs: null, modules: null, now: NOW };
const mod = (enabled: boolean, last: Mod["last"]): Mod => ({
  name: "m",
  title: "m",
  question: "",
  enabled,
  hard_note: "",
  periodic: true,
  params: [],
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

  it("机器人离线、模块自检失败（已关闭的模块不算）", () => {
    const r = buildReminders({
      ...base,
      overview: ov(),
      bot: { status: { online: false } },
      modules: { modules: [mod(true, { ok: false }), mod(false, { ok: false }), mod(true, { ok: true })] },
    });
    expect(r.map((x) => x.key)).toEqual(["discord", "modules"]);
    expect(r[1]?.title).toBe("1 个模块自检没通过");
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
