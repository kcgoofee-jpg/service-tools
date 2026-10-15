import { describe, expect, it } from "vitest";
import { maskToken, safeAvatar } from "./core";

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
