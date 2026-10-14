import { describe, expect, it } from "vitest";
import { bugSource, formatClient, kindLabel, shortModel, timing } from "./format";

describe("shortModel", () => {
  it("把 NovelAI 模型名缩成版本号", () => {
    expect(shortModel("nai-diffusion-5-full")).toBe("V5");
    expect(shortModel("nai-diffusion-4-5-curated")).toBe("V4.5 Curated");
    expect(shortModel("nai-diffusion-4-full")).toBe("V4");
    expect(shortModel("nai-diffusion-3-inpainting")).toBe("V3 重绘");
    expect(shortModel("")).toBe("");
    expect(shortModel("some-very-long-unknown-model-name")).toBe("some-very-long-u…");
  });
});

describe("formatClient", () => {
  it("认出家族并补系统 / 外壳", () => {
    expect(formatClient("DSH-Tavern/2.6 (Linux; Android 14; wv) Chrome/120")).toBe(
      "DSH Tavern 配图插件 · Android · App 内置页",
    );
    expect(formatClient("")).toBe("");
    expect(formatClient("undici")).toContain("Node 脚本");
    expect(formatClient("Mozilla/5.0")).toBe("通用 UA（客户端没填完整）");
  });
});

describe("kindLabel", () => {
  it("流式出图、导演工具等别名归到对应功能", () => {
    expect(kindLabel("image_stream")).toBe("文生图");
    expect(kindLabel("augment-image")).toBe("导演工具");
    expect(kindLabel("chat")).toBe("文本 / 聊天");
    expect(kindLabel("mystery")).toBe("mystery");
    expect(kindLabel("")).toBe("—");
  });
});

describe("timing", () => {
  it("≥10 秒取整，等待不足 100ms 不显示", () => {
    expect(timing(50, 4200)).toEqual({ gen: "4.2s", wait: "" });
    expect(timing(12500, 15000)).toEqual({ gen: "15s", wait: "13s" });
    expect(timing(0, 0)).toEqual({ gen: "", wait: "" });
  });
});

describe("bugSource", () => {
  it("前缀类来源翻译", () => {
    expect(bugSource("upstream")).toBe("上游故障");
    expect(bugSource("maintenance:disk")).toBe("维护任务 · 磁盘监控");
    expect(bugSource("web:admin")).toBe("网页脚本 · 后台");
    expect(bugSource("check:foo")).toBe("交叉校验 · foo");
    expect(bugSource("zzz")).toBe("zzz");
  });
});
