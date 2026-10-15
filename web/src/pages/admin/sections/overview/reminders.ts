/**
 * 总览的「有 N 件事要处理」：只列真正要站长动手的事——未处理的 Bug、定时任务出错或超时没运行、奶妹离线、上游 Token 失效或没配置。
 * 纯函数，方便单测。磁盘、预算、限流中的账号这类不用马上处理的事不在这里提醒（真正的重大事故走告警渠道）。
 */
import { rel } from "../../../../lib/format";
import { alertJobs, type JobsResponse, jobAlertCount } from "../system/jobs-core";
import type { BotResp, BugsResp, Overview } from "./types";

export type Level = "bad" | "warn";
export type Reminder = {
  key: string;
  level: Level;
  title: string;
  detail: string;
  /** 「去看看」跳转的路径 */
  to: string;
  /** 按钮的无障碍名称后缀（说明去哪里） */
  label: string;
};

export type ReminderInput = {
  overview: Overview;
  bot: BotResp | null;
  bugs: BugsResp | null;
  jobs: JobsResponse | null;
  now: number;
};

export function buildReminders(inp: ReminderInput): Reminder[] {
  const { overview: d, bot, bugs, jobs, now } = inp;
  const items: Reminder[] = [];
  const add = (level: Level, key: string, title: string, detail: string, to: string, label: string) =>
    items.push({ key, level, title, detail, to, label });

  for (const p of d.pool ?? []) {
    if (p.admin_enabled && p.disabled) {
      add(
        "bad",
        `up-acct-${p.position}-dead`,
        `上游账号 #${p.position} 失效（401）`,
        "Token 过期或被改了密码，需要替换",
        "/admin/upstream",
        "上游账号失效",
      );
    }
  }
  if (!d.pool_configured) {
    add(
      "bad",
      "up-notconfigured",
      "还没有配置上游 Token",
      "现在所有出图请求都会失败",
      "/admin/upstream",
      "未配置上游",
    );
  }
  if (bot?.status && !bot.status.online) {
    add(
      "bad",
      "discord",
      "奶妹离线",
      bot.status.seen_at ? `最近心跳 ${rel(bot.status.seen_at, now)}` : "还没收到心跳",
      "/admin/discord",
      "机器人离线",
    );
  }
  if (bugs && bugs.open > 0) {
    const err = bugs.errors.filter((e) => !e.resolved_at && e.level === "error").length;
    add(
      err ? "bad" : "warn",
      "bugs",
      `${bugs.open} 个 Bug 没处理`,
      err ? `其中 ${err} 个是错误级别` : "都是「注意」级别，多数是客户端断开之类",
      "/admin/logs/bugs",
      "Bug 未处理",
    );
  }
  const badJobs = jobAlertCount(jobs, now);
  if (badJobs) {
    add(
      "warn",
      "jobs",
      `${badJobs} 个定时任务上次出错或超时没运行`,
      alertJobs(jobs, now)
        .map((j) => j.title)
        .join("、"),
      "/admin/system#jobs",
      "定时任务",
    );
  }
  return items;
}

/* ---------------- 结论 ---------------- */
export type Verdict = {
  tone: "ok" | "warn" | "bad";
  headline: string;
  items: Reminder[];
};

export function computeVerdict(items: Reminder[]): Verdict {
  const tone: Verdict["tone"] = items.some((x) => x.level === "bad") ? "bad" : items.length ? "warn" : "ok";
  return { tone, headline: items.length ? `有 ${items.length} 件事要处理` : "一切正常", items };
}
