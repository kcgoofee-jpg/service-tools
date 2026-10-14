/**
 * 状态页事件：常量与文案检查（纯函数，可单测）。
 *
 * 标准句式与生命周期照 app/status_page.py（Atlassian Statuspage 标准）：
 * 调查中 → 已确定 → 监控中 → 已解决，中途可发「更新」。标题 = 受影响的功能 + 现象，不写原因；
 * 正文只写成员能感知到的影响、时间段和进展，不写内部原因细节、数据量、版本号、配置名、哪个成员、哪把 Key。
 */

export type Status = "investigating" | "identified" | "update" | "monitoring" | "resolved";
export type Impact = "none" | "minor" | "major" | "critical";

export const STATUS_LABEL: Record<Status, string> = {
  investigating: "调查中",
  identified: "已确定",
  update: "更新",
  monitoring: "监控中",
  resolved: "已解决",
};
/** Statuspage 标准句式（与后端 STANDARD_BODY 一致）：正文留空时公开页用这些 */
export const STANDARD_BODY: Record<Status, string> = {
  investigating: "我们正在调查此问题。",
  identified: "问题已定位，正在实施修复。",
  update: "我们正在继续处理此问题。",
  monitoring: "修复已上线，我们正在观察结果。",
  resolved: "此事件已解决。",
};
export const IMPACT_LABEL: Record<Impact, string> = {
  none: "无影响（仅通知）",
  minor: "性能下降",
  major: "部分中断",
  critical: "严重中断",
};
export const COMPONENTS: Array<{ key: string; label: string; onPublicPage: boolean }> = [
  { key: "upstream", label: "NovelAI 上游（生图）", onPublicPage: true },
  { key: "api", label: "出图接口", onPublicPage: true },
  { key: "web", label: "官网与后台", onPublicPage: false },
  { key: "bot", label: "Discord 机器人（奶妹）", onPublicPage: false },
];
export const CREATE_STATUSES: Status[] = ["investigating", "identified", "monitoring", "resolved"];
export const UPDATE_STATUSES: Status[] = ["investigating", "identified", "update", "monitoring", "resolved"];

export type Incident = {
  id: number;
  title: string;
  impact: string;
  components: string[];
  started_at: number;
  resolved_at: number | null;
  updates: Array<{ status: string; body: string; at: number }>;
};

/** 按标准生命周期推荐下一个状态 */
export function nextStatus(latest: string | undefined): Status {
  switch (latest) {
    case "investigating":
      return "identified";
    case "identified":
      return "monitoring";
    case "monitoring":
      return "resolved";
    case "resolved":
      return "update";
    default:
      return "update";
  }
}

/** 最终会公开显示的正文：留空用标准句式 */
export function finalBody(status: Status, body: string): string {
  return body.trim() || STANDARD_BODY[status];
}

const LEAK_PATTERNS: Array<[RegExp, string]> = [
  [/pst-[\w-]+/i, "Token"],
  [/\b(?:api[-_ ]?key|token|secret)\b/i, "Key / Token"],
  [/(?:哪把|某把|这把|该)\s*key/i, "具体哪把 Key"],
  [/#\d+/, "编号（Key / 成员 / Bug）"],
  [/\bv?\d+\.\d+(?:\.\d+)+\b/i, "版本号"],
  [/\d+(?:\.\d+)?\s*(?:MB|GB|KB|条记录|行数据)/i, "数据量"],
  [/\.env|sqlite|数据库|配置项|环境变量|内存|线程|堆栈|重启了|部署了|代码|回滚|缓存/i, "内部实现细节"],
  [/@\w+/, "成员 @ 提及"],
  [/(?:由于|因为|原因是|根因|导致于)/, "原因说明"],
];

/** 标题 / 正文里疑似泄露内部细节的地方（只提醒，不拦截）。 */
export function leakWarnings(...texts: string[]): string[] {
  const joined = texts.join("\n");
  const out: string[] = [];
  for (const [re, label] of LEAK_PATTERNS) if (re.test(joined) && !out.includes(label)) out.push(label);
  return out;
}
