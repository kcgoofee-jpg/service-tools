/** 成员反馈的筛选、问卷的统计（纯函数）。 */
import type { FeedbackItem, SurveyRow } from "./types";

/** 按成员（Key 名 / 用户名 / Discord ID）或内容搜索；关键字为空 = 全部。旧版选择题的记录搜的是那一行摘要。 */
export function filterFeedback(items: FeedbackItem[], query: string): FeedbackItem[] {
  const q = query.trim().toLowerCase();
  if (!q) return items;
  return items.filter((it) =>
    [it.key_name, it.username, it.discord_id, it.text].filter(Boolean).join(" ").toLowerCase().includes(q),
  );
}

/**
 * 把文字拆成「前面」和「末尾 n 个字」（按字符，不拆表情 / 生僻字）。末尾那一行（最后一个换行之后）不够 n 个字时不拆：
 * 要么整段本来就短，要么最后一行是成员自己换的行，不替他改。
 */
export function splitTail(text: string, n: number): [string, string] {
  const chars = [...text.slice(text.lastIndexOf("\n") + 1)];
  if (chars.length <= n) return [text, ""];
  const tail = chars.slice(-n).join("");
  return [text.slice(0, text.length - tail.length), tail];
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
