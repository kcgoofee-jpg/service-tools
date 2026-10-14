import { describe, expect, it } from "vitest";
import { couponLeft, maskKey, quotaRows, resetHour } from "./helpers";
import { mergeMeta } from "./store";
import type { MeMember } from "./types";

const member = (over: Partial<MeMember> = {}): MeMember => ({
  logged_in: true,
  has_key: true,
  image_scope: "all",
  today: {
    images: 30,
    v5: 4,
    legacy: 20,
    daily_images: 150,
    daily_v5: 6,
    anlas: 12,
    daily_anlas: 50,
    reset_hour: 7,
  },
  ...over,
});

describe("maskKey", () => {
  it("长 Key 显示前 7 + … + 后 4", () => {
    expect(maskKey("nai-demo00xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")).toBe("nai-dem…xxxx");
  });
  it("短的原样", () => {
    expect(maskKey("nai-short")).toBe("nai-short");
  });
});

describe("couponLeft", () => {
  const now = 1_000_000;
  it("先向上取整到小时再拆天，不出现「2 天 24 小时」", () => {
    expect(couponLeft(now + 3 * 86400 - 10, now)).toBe("3 天");
    expect(couponLeft(now + 2 * 86400 + 90 * 60, now)).toBe("2 天 2 小时");
  });
  it("不足一天只写小时", () => {
    expect(couponLeft(now + 5 * 3600 + 1, now)).toBe("6 小时");
  });
  it("已过期按 0 小时", () => {
    expect(couponLeft(now - 100, now)).toBe("0 小时");
  });
});

describe("quotaRows", () => {
  it("V4.5 用 legacy 免费图张数；V5 / Anlas 各自有上限时显示", () => {
    const rows = quotaRows(member());
    expect(rows.map((r) => [r.id, r.used, r.cap])).toEqual([
      ["legacy", 20, 150],
      ["v5", 4, 6],
      ["anlas", 12, 50],
    ]);
  });
  it("没有 legacy 字段时退回 images − v5", () => {
    const rows = quotaRows(member({ today: { images: 30, v5: 4, daily_images: 150, daily_v5: 6 } }));
    expect(rows[0]?.used).toBe(26);
  });
  it("个人 V5 不限（0）且模型范围是 all：仍显示，注明受全站上限约束", () => {
    const v5 = quotaRows(
      member({ today: { images: 1, v5: 2, legacy: 1, daily_images: 150, daily_v5: 0 } }),
    ).find((r) => r.id === "v5");
    expect(v5?.cap).toBe(0);
    expect(v5?.hint).toContain("全站日上限");
  });
  it("没配 V5 上限且模型范围不是 all：不显示 V5；没配 Anlas 上限：不显示 Anlas", () => {
    const rows = quotaRows(
      member({
        image_scope: "free",
        today: { images: 1, v5: 0, legacy: 1, daily_images: 150, daily_v5: 0, daily_anlas: 0 },
      }),
    );
    expect(rows.map((r) => r.id)).toEqual(["legacy"]);
  });
  it("个人不限 V4.5：上限 0", () => {
    const rows = quotaRows(member({ today: { images: 1, legacy: 1, daily_images: 0 } }));
    expect(rows[0]?.cap).toBe(0);
  });
});

describe("resetHour", () => {
  it("用接口给的整点，缺省按站长规定的 7 点", () => {
    expect(resetHour(member({ today: { reset_hour: 5 } }))).toBe(5);
    expect(resetHour(member({ today: {} }))).toBe(7);
    expect(resetHour(null)).toBe(7);
  });
});

describe("mergeMeta", () => {
  it("live 里有的字段优先，没有的用 /public/status 补", () => {
    const m = mergeMeta(
      { discord_invite: "https://discord.gg/a", web_login: false, algo_notice: "旧说明" },
      { web_login: true, home_join_open: true },
    );
    expect(m).toEqual({
      discord_invite: "https://discord.gg/a",
      web_login: true,
      algo_notice: "旧说明",
      home_join_open: true,
    });
  });
  it("两边都没有就不出现该字段（不编默认值）", () => {
    expect(mergeMeta(null, null)).toEqual({});
  });
});
