/** 成员反馈 / 问卷的统计与筛选（纯函数）。 */
import type { FeedbackItem, SurveyRow } from "./types";

const GOOD = ["非常满意", "满意"];
const BAD = ["一般", "不太满意", "很不满意"];

export type FeedbackStats = {
  total: number;
  satisfied: number;
  satRate: number;
  topPain: [string, number] | null;
  topWish: [string, number] | null;
};

function top(items: FeedbackItem[], field: string, ignore: string): [string, number] | null {
  const counts = new Map<string, number>();
  for (const it of items) {
    const v = it.answers?.[field];
    if (v && v !== ignore) counts.set(v, (counts.get(v) ?? 0) + 1);
  }
  const best = [...counts.entries()].sort((a, b) => b[1] - a[1])[0];
  return best ?? null;
}

export function feedbackStats(items: FeedbackItem[]): FeedbackStats {
  const total = items.length;
  const satisfied = items.filter((it) => GOOD.includes(it.answers?.score ?? "")).length;
  return {
    total,
    satisfied,
    satRate: total ? Math.round((satisfied / total) * 100) : 0,
    topPain: top(items, "pain", "没什么困扰"),
    topWish: top(items, "wish", "保持现状就好"),
  };
}

export type ScoreFilter = "" | "good" | "bad";
export function filterFeedback(items: FeedbackItem[], score: ScoreFilter, query: string): FeedbackItem[] {
  const q = query.trim().toLowerCase();
  return items.filter((it) => {
    const s = it.answers?.score ?? "";
    if (score === "good" && !GOOD.includes(s)) return false;
    if (score === "bad" && !BAD.includes(s)) return false;
    if (q) {
      const text = [it.key_name, it.username, it.discord_id, ...Object.values(it.answers ?? {})]
        .filter(Boolean)
        .join(" ")
        .toLowerCase();
      if (!text.includes(q)) return false;
    }
    return true;
  });
}

/** 评价的显示：文字 + 色调（不只靠颜色区分） */
export function scoreTone(score: string): "ok" | "warn" | "danger" | undefined {
  if (score === "非常满意" || score === "满意") return "ok";
  if (score === "一般" || score === "不太满意") return "warn";
  if (score === "很不满意") return "danger";
  return undefined;
}

/* ---------------- 详细问卷（/survey 网页） ---------------- */
export type SurveyStats = {
  total: number;
  understandPct: number;
  willing: number;
  declined: number;
  topImprove: [string, number] | null;
};

function tally(rows: SurveyRow[], field: "understand" | "improve" | "donate"): Array<[string, number]> {
  const o = new Map<string, number>();
  for (const r of rows) {
    for (const part of String(r[field] ?? "").split("、")) {
      const p = part.trim();
      if (p) o.set(p, (o.get(p) ?? 0) + 1);
    }
  }
  return [...o.entries()].sort((a, b) => b[1] - a[1]);
}

export function surveyStats(rows: SurveyRow[]): SurveyStats {
  const und = tally(rows, "understand");
  const imp = tally(rows, "improve");
  const don = tally(rows, "donate");
  const okU = und.filter(([k]) => k === "完全理解" || k === "基本理解").reduce((a, [, n]) => a + n, 0);
  return {
    total: rows.length,
    understandPct: rows.length ? Math.round((okU / rows.length) * 100) : 0,
    willing: don.find(([k]) => k.includes("愿意"))?.[1] ?? 0,
    declined: don.find(([k]) => k === "暂不考虑")?.[1] ?? 0,
    topImprove: imp[0] ?? null,
  };
}
