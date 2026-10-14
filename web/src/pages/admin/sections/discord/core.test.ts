import { describe, expect, it } from "vitest";
import { botBody, maskToken, safeAvatar, safeDiscordUrl, surveyFacts } from "./core";

describe("安全显示", () => {
  it("捐赠的 Token 只显示头尾", () => {
    expect(maskToken("pst-abcdefghijklmnopqrstuvwxyz")).toBe("pst-abcd…wxyz");
    expect(maskToken("pst-short")).toBe("pst-…");
  });
  it("机器人上报的链接只认 discord.com，头像只认官方 CDN", () => {
    expect(safeDiscordUrl("https://discord.com/channels/1/2")).toBe("https://discord.com/channels/1/2");
    expect(safeDiscordUrl("javascript:alert(1)")).toBeNull();
    expect(safeDiscordUrl("https://evil.example/discord.com")).toBeNull();
    expect(safeAvatar("https://cdn.discordapp.com/avatars/1/a.png")).toBe(
      "https://cdn.discordapp.com/avatars/1/a.png",
    );
    expect(safeAvatar("https://evil.example/a.png")).toBeNull();
    expect(safeAvatar(null)).toBeNull();
  });
});

describe("机器人设置", () => {
  it("开关在接口里是 0 / 1", () => {
    expect(botBody({ gallery_like: true, gallery_ai: false, gallery_ai_daily: 20 })).toEqual({
      gallery_like: 1,
      gallery_ai: 0,
      gallery_ai_daily: 20,
    });
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
