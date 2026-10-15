import { describe, expect, it } from "vitest";
import { maskToken } from "./core";

describe("安全显示", () => {
  it("捐赠的 Token 只显示头尾", () => {
    expect(maskToken("pst-abcdefghijklmnopqrstuvwxyz")).toBe("pst-abcd…wxyz");
    expect(maskToken("pst-short")).toBe("pst-…");
  });
});
