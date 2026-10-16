import { describe, expect, it } from "vitest";
import {
  deriveV5,
  guardSpecs,
  imagesPerPercent,
  isMediumMode,
  MEDIUM_MODES,
  mediumStatus,
  pacedText,
  type V5Plan,
} from "./core";

const plan: V5Plan = { rate: 5, percent: null, global: 71, people: 10, each: 7 };

describe("deriveV5（全站 V5 = 恢复 × 每 1% 张数；每人 = max(1, 全站 ÷ max(10, 人数))）", () => {
  it("与接口给的数字对得上", () => {
    const d = deriveV5(plan, 14.2);
    expect(d.calc).toBe(71);
    expect(d.matches).toBe(true);
    expect(d.each).toBe(7);
    expect(d.eachMatches).toBe(true);
  });
  it("当天方案沿用 7 点定下的全站池：和现在的恢复速度算出来的不一样时标出来", () => {
    const d = deriveV5({ ...plan, rate: 6 }, 14.2);
    expect(d.calc).toBe(85);
    expect(d.matches).toBe(false);
    expect(d.each).toBe(7);
  });
  it("人多时按人数平分，不再夹到 3～30；最少 1 张", () => {
    expect(deriveV5({ ...plan, people: 40, each: 1 }, 14.2).each).toBe(1);
    expect(deriveV5({ ...plan, global: 10, people: 40, each: 1 }, 14.2).eachMatches).toBe(true);
    expect(deriveV5({ ...plan, global: 1000, each: 100 }, 14.2).eachMatches).toBe(true);
  });
});

describe("imagesPerPercent", () => {
  it("优先取动态额度接口给的 images_per_percent，拿不到才用默认值", () => {
    expect(imagesPerPercent({ images_per_percent: 13 })).toEqual({ value: 13, fromApi: true });
    expect(imagesPerPercent({})).toEqual({ value: 14.2, fromApi: false });
    expect(imagesPerPercent(null)).toEqual({ value: 14.2, fromApi: false });
    expect(imagesPerPercent(undefined)).toEqual({ value: 14.2, fromApi: false });
  });
});

describe("保护推算", () => {
  it("按时段放行：7 点先放 burst 张，之后每小时补日上限 ÷ 24", () => {
    expect(pacedText(1000, 150)).toBe("7 点先放 150 张，之后每小时补 42 张（= 1000 ÷ 24）");
    expect(pacedText(0, 150)).toBeNull();
    expect(pacedText(1000, 0)).toBeNull();
  });
  it("字段范围取自接口", () => {
    const specs = guardSpecs({
      values: {},
      fields: {
        account_daily_cap: { default: 1000, min: 0, max: 20000, label: "x" },
      },
    });
    expect(Object.keys(specs)).toEqual(["account_daily_cap"]);
    expect(specs.account_daily_cap).toMatchObject({ kind: "int", min: 0, max: 20000 });
  });
});

describe("V5 Medium 降档", () => {
  it("三个模式、识别合法值", () => {
    expect(MEDIUM_MODES.map((m) => m.value)).toEqual(["off", "on", "auto"]);
    expect(isMediumMode("auto")).toBe(true);
    expect(isMediumMode("ON")).toBe(false);
    expect(isMediumMode(undefined)).toBe(false);
  });
  it("关闭 / 开启只有一行状态", () => {
    expect(mediumStatus("off", null)).toEqual([["当前状态", "未启用，V5 Full 照常出"]]);
    expect(mediumStatus("on", null)).toHaveLength(1);
  });
  it("自动档：是否已切换、最低余量、切换条件（回切线 = 提示线 + 5）", () => {
    const rows = mediumStatus("auto", { active: true, lowest_percent: 12, threshold: 20 });
    expect(rows[0]).toEqual(["现在是否已切换", "是，V5 Full 请求正在改走 Medium"]);
    expect(rows[1]).toEqual(["最低的账号 V5 余量", "12%"]);
    expect(rows[2]?.[1]).toBe("低于 20% 时切换，回升到 25% 以上切回");
  });
  it("自动档：还没读到余量时不假装有数", () => {
    const rows = mediumStatus("auto", { active: false, lowest_percent: null, threshold: 20 });
    expect(rows[0]?.[1]).toBe("否，V5 Full 照常出");
    expect(rows[1]?.[1]).toBe("还没读到（有人出过 V5 才有）");
    expect(mediumStatus("auto", undefined)[2]?.[1]).toBe("低于「V5 余量低提示线」时切换");
  });
});
