import { describe, expect, it } from "vitest";
import {
  aspectOf,
  charsText,
  hasPicture,
  negativeText,
  paramsLine,
  parseExtra,
  statusText,
} from "./auditData";

describe("parseExtra", () => {
  it("坏 JSON / 空 / 非对象都当作空", () => {
    expect(parseExtra("")).toEqual({});
    expect(parseExtra(null)).toEqual({});
    expect(parseExtra("{坏")).toEqual({});
    expect(parseExtra("123")).toEqual({});
    expect(parseExtra('{"params":{"seed":7}}')).toEqual({ params: { seed: 7 } });
  });
});

describe("aspectOf / paramsLine", () => {
  it("按宽高算比例，缺失按 2:3", () => {
    expect(aspectOf({ params: { width: 1216, height: 832 } })).toBeCloseTo(1.4615, 3);
    expect(aspectOf({})).toBeCloseTo(2 / 3, 5);
    expect(aspectOf({ params: { width: 0, height: 10 } })).toBeCloseTo(2 / 3, 5);
  });
  it("参数摘要只列有值的项，种子 0 也要显示", () => {
    expect(
      paramsLine({ params: { seed: 0, sampler: "k_euler", steps: 28, scale: 5, width: 832, height: 1216 } }),
    ).toBe("种子 0 · k_euler · 28 步 · CFG 5 · 832×1216");
    expect(paramsLine({})).toBe("");
  });
});

describe("提示词块", () => {
  it("角色提示词与负面提示词按编号合并", () => {
    expect(charsText(["a", "b"])).toBe("角色1：a\n角色2：b");
    expect(charsText(undefined)).toBe("");
    expect(negativeText({ negative: "bad" }, { char_negatives: ["x"] })).toBe("bad\n角色1负面：x");
    expect(negativeText({ negative: "" }, {})).toBe("");
  });
});

describe("其他", () => {
  it("是否有图、状态文字", () => {
    expect(hasPicture({ has_image: 0, has_thumb: 1 })).toBe(true);
    expect(hasPicture({ has_image: false, has_thumb: false })).toBe(false);
    expect(statusText("ok")).toBe("成功");
    expect(statusText("timeout")).toBe("timeout");
    expect(statusText("")).toBe("错误");
  });
});
