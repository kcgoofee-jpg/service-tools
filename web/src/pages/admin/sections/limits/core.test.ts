import { describe, expect, it } from "vitest";
import { deriveV5, guardSpecs, imagesPerPercent, pacedText, type V5Plan } from "./core";

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
  it("优先取容量模块给的参数，拿不到才用默认值", () => {
    expect(
      imagesPerPercent({ modules: [{ name: "capacity", params: [{ name: "V5 每 1% ≈ 张", value: 13 }] }] }),
    ).toEqual({
      value: 13,
      fromApi: true,
    });
    expect(imagesPerPercent(null)).toEqual({ value: 14.2, fromApi: false });
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
