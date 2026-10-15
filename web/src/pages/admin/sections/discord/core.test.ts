import { describe, expect, it } from "vitest";
import { maskToken, safeAvatar, surveyFacts } from "./core";

describe("安全显示", () => {
  it("捐赠的 Token 只显示头尾", () => {
    expect(maskToken("pst-abcdefghijklmnopqrstuvwxyz")).toBe("pst-abcd…wxyz");
    expect(maskToken("pst-short")).toBe("pst-…");
  });
  it("头像只认官方 CDN", () => {
    expect(safeAvatar("https://cdn.discordapp.com/avatars/1/a.png")).toBe(
      "https://cdn.discordapp.com/avatars/1/a.png",
    );
    expect(safeAvatar("https://evil.example/a.png")).toBeNull();
    expect(safeAvatar(null)).toBeNull();
  });
});

describe("问卷汇总", () => {
  it("理解率与愿意捐的人数", () => {
    const f = surveyFacts({
      total: 3,
      rows: [{}, {}, {}] as never,
      summary: {
        understand: { 完全理解: 1, 基本理解: 1, 不太理解: 1 },
        improve: { 排队更快: 2 },
        donate: { 愿意捐一部分: 1, 暂不考虑: 2 },
        client: {},
      },
    });
    expect(f).toMatchObject({ total: 3, understoodPct: 67, willing: 1, topImprove: ["排队更快", 2] });
  });
});
