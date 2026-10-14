import { describe, expect, it } from "vitest";
import { feedbackStats, filterFeedback, scoreTone, surveyStats } from "./feedbackStats";
import type { FeedbackItem, SurveyRow } from "./types";

const fb = (answers: Record<string, string>, p: Partial<FeedbackItem> = {}): FeedbackItem => ({
  id: 1,
  ts: 0,
  discord_id: "9",
  username: "u",
  key_id: 1,
  key_name: "月见草",
  answers,
  ...p,
});

describe("feedbackStats", () => {
  it("满意率只算「非常满意 / 满意」；Top 困扰与期望忽略「没什么困扰 / 保持现状」", () => {
    const items = [
      fb({ score: "非常满意", pain: "没什么困扰", wish: "保持现状就好" }),
      fb({ score: "一般", pain: "排队太久 / 经常提示人多", wish: "多给一些 V5" }),
      fb({ score: "不太满意", pain: "排队太久 / 经常提示人多", wish: "出图更快、少排队" }),
      fb({ score: "满意", pain: "V5 额度太少", wish: "多给一些 V5" }),
    ];
    const s = feedbackStats(items);
    expect(s).toMatchObject({ total: 4, satisfied: 2, satRate: 50 });
    expect(s.topPain).toEqual(["排队太久 / 经常提示人多", 2]);
    expect(s.topWish).toEqual(["多给一些 V5", 2]);
  });
  it("没有数据时不出错", () => {
    expect(feedbackStats([])).toEqual({ total: 0, satisfied: 0, satRate: 0, topPain: null, topWish: null });
  });
});

describe("filterFeedback", () => {
  const items = [
    fb({ score: "满意" }, { key_name: "A" }),
    fb({ score: "很不满意", pain: "出图失败或报错" }, { key_name: "B" }),
  ];
  it("按评价与关键字筛选", () => {
    expect(filterFeedback(items, "good", "").map((x) => x.key_name)).toEqual(["A"]);
    expect(filterFeedback(items, "bad", "").map((x) => x.key_name)).toEqual(["B"]);
    expect(filterFeedback(items, "", "报错").map((x) => x.key_name)).toEqual(["B"]);
    expect(filterFeedback(items, "", "").length).toBe(2);
  });
});

describe("scoreTone", () => {
  it("文字评价对应色调", () => {
    expect(scoreTone("满意")).toBe("ok");
    expect(scoreTone("一般")).toBe("warn");
    expect(scoreTone("很不满意")).toBe("danger");
    expect(scoreTone("")).toBeUndefined();
  });
});

describe("surveyStats", () => {
  it("理解率、愿意捐赠人数、Top 改进诉求（多选用「、」分隔）", () => {
    const rows: SurveyRow[] = [
      { ts: 1, understand: "完全理解", improve: "多给 V5、出图更快", donate: "愿意捐 V4.5" },
      { ts: 2, understand: "基本理解", improve: "多给 V5", donate: "暂不考虑" },
      { ts: 3, understand: "不太理解", improve: "", donate: "" },
    ];
    expect(surveyStats(rows)).toEqual({
      total: 3,
      understandPct: 67,
      willing: 1,
      declined: 1,
      topImprove: ["多给 V5", 2],
    });
    expect(surveyStats([]).total).toBe(0);
  });
});
