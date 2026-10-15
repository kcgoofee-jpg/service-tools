import { describe, expect, it } from "vitest";
import { autopilotValue, deriveV5, guardSpecs, imagesPerPercent, pacedText, type V5Plan } from "./core";

const plan: V5Plan = { rate: 5, percent: null, k: 0.9, global: 63, people: 10, each: 6 };

describe("deriveV5（全站 V5 = 恢复 × 每 1% 张数 × 系数；每人 = 全站 ÷ 人数，限制在范围内）", () => {
  it("与接口给的数字对得上", () => {
    const d = deriveV5(plan, 14.2, 3, 30);
    expect(d.calc).toBe(63);
    expect(d.matches).toBe(true);
    expect(d.perPersonRaw).toBe(6);
    expect(d.eachMatches).toBe(true);
  });
  it("对不上时标出来（满电放宽等规则会让接口值更大）", () => {
    const d = deriveV5({ ...plan, global: 260, each: 26, percent: 95 }, 14.2, 10, 30);
    expect(d.matches).toBe(false);
    expect(d.perPersonRaw).toBe(26);
  });
  it("每人额度被下限 / 上限夹住", () => {
    expect(deriveV5({ ...plan, global: 10, each: 3 }, 14.2, 3, 30).eachMatches).toBe(true);
    expect(deriveV5({ ...plan, global: 1000, each: 30 }, 14.2, 3, 30).eachMatches).toBe(true);
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
  it("字段范围取自接口，由算法管理的保底不出现在表单里", () => {
    const specs = guardSpecs({
      values: {},
      fields: {
        account_daily_cap: { default: 1000, min: 0, max: 20000, label: "x" },
        base_daily_images: { default: 100, min: 0, max: 100000, label: "y" },
      },
      quiet_now: false,
      hourly_cap_now: 150,
      accounts: [],
      queued_images: 0,
    });
    expect(Object.keys(specs)).toEqual(["account_daily_cap"]);
    expect(specs.account_daily_cap).toMatchObject({ kind: "int", min: 0, max: 20000 });
  });
});

describe("自动驾驶", () => {
  it("值的显示", () => {
    expect(autopilotValue("slots", 15)).toBe("名额上限 → 15");
    expect(autopilotValue("breaker", false)).toBe("正常");
    expect(autopilotValue("key_guard", [{ name: "小明", action: "pause" }])).toBe("小明：暂停");
    expect(autopilotValue("key_guard", [])).toBe("无");
    expect(autopilotValue("key_guard", [{ name: "甲" }, { name: "乙" }])).toBe("甲：暂停；乙：暂停");
  });
});
