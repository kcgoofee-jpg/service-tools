import { describe, expect, it } from "vitest";
import { DONATE, EMPTY, IMPROVE, payload, UNDERSTAND, validate } from "./questions";

describe("问卷", () => {
  it("选项文字与旧版问卷的取值一致（后台按这些文字统计）", () => {
    expect(UNDERSTAND.map((c) => c.value)).toEqual(["完全理解", "基本理解", "不太理解", "没注意过"]);
    expect(DONATE.map((c) => c.value)).toEqual(["愿意捐V4.5", "可以考虑", "暂不考虑"]);
    expect(IMPROVE).toHaveLength(6);
  });

  it("必答题没选就指出是第几题", () => {
    expect(validate(EMPTY)).toEqual({ understand: "第 2 题还没选", donate: "第 4 题还没选" });
    expect(validate({ ...EMPTY, understand: "完全理解" })).toEqual({ donate: "第 4 题还没选" });
    expect(validate({ ...EMPTY, understand: "完全理解", donate: "暂不考虑" })).toEqual({});
  });

  it("提交体字段名沿用旧版 /member/survey", () => {
    expect(
      payload({
        client: " 柏宝绘 1.2 ",
        understand: "基本理解",
        improve: ["稳定性"],
        improveOther: " 想要深色模式 ",
        donate: "可以考虑",
      }),
    ).toEqual({
      client: "柏宝绘 1.2",
      understand: "基本理解",
      improve: ["稳定性"],
      improve_other: "想要深色模式",
      donate: "可以考虑",
    });
  });
});
