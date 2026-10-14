/** Discord 分区：接口类型与小工具（纯函数，可单测）。 */
import type { FieldSpec } from "../system/core";

export type BotStatus = {
  online?: boolean;
  user?: string;
  guild?: string;
  latency_ms?: number | null;
  ready_at?: number;
  ai_ready?: boolean;
  ai_today?: number;
  version?: string;
  seen_at?: number;
};
export type BotEvent = {
  at: number;
  kind: string;
  title: string;
  author: string;
  detail: string;
  url: string;
};
export type BotData = {
  config: {
    gallery_forum: string;
    gallery_like: number;
    gallery_ai: number;
    gallery_ai_daily: number;
    gallery_ai_model: string;
  };
  status: BotStatus;
  events: BotEvent[];
  kinds: Record<string, string>;
};

export const BOT_SPECS: Record<string, FieldSpec> = {
  gallery_like: { kind: "bool", label: "新帖自动点赞" },
  gallery_ai: { kind: "bool", label: "看图写评论" },
  gallery_forum: {
    kind: "text",
    label: "论坛频道名包含",
    min: 1,
    max: 50,
    hint: "名字里含这几个字的论坛频道才会被监听",
  },
  gallery_ai_daily: {
    kind: "int",
    label: "AI 评论每天最多",
    min: 0,
    max: 500,
    unit: "条",
    hint: "超出后只点赞不评论；0 = 不评论",
  },
  gallery_ai_model: { kind: "text", label: "模型标识", min: 1, max: 80, hint: "例如 LongCat-2.5-Preview" },
};
const BOOL_FIELDS = new Set(["gallery_like", "gallery_ai"]);

/** 表单改动 → 接口请求体（开关在接口里是 0 / 1） */
export function botBody(changes: Record<string, number | string | boolean>): Record<string, number | string> {
  const out: Record<string, number | string> = {};
  for (const [k, v] of Object.entries(changes))
    out[k] = BOOL_FIELDS.has(k) ? Number(v) : (v as number | string);
  return out;
}

export const EVENT_TONE: Record<string, "ok" | "warn" | "danger" | undefined> = {
  like: undefined,
  comment: "ok",
  shy: "warn",
  skip: undefined,
  error: "danger",
};

/** 只允许 Discord 官方域名的链接（接口里的 url 来自机器人上报，不能当可信链接直接放进 href） */
export function safeDiscordUrl(url: string): string | null {
  try {
    const u = new URL(url);
    return u.protocol === "https:" && (u.hostname === "discord.com" || u.hostname.endsWith(".discord.com"))
      ? u.href
      : null;
  } catch {
    return null;
  }
}

/** Discord 头像只接受官方 CDN */
export function safeAvatar(url: string | null | undefined): string | null {
  if (!url) return null;
  try {
    const u = new URL(url);
    return u.protocol === "https:" && u.hostname === "cdn.discordapp.com" ? u.href : null;
  } catch {
    return null;
  }
}

export type Donation = {
  id: number;
  ts: number;
  token: string;
  message: string;
  contact: string;
  handled: number;
  handled_at: number | null;
  handled_note: string | null;
};
export type Donations = { unhandled: Donation[]; handled: Donation[] };

/** 上游 Token 只显示头尾：pst-abcd…wxyz */
export function maskToken(t: string): string {
  return t.length <= 14 ? `${t.slice(0, 4)}…` : `${t.slice(0, 8)}…${t.slice(-4)}`;
}

export type Ban = {
  discord_id: string;
  created_at: number;
  reason: string | null;
  username: string | null;
  display_name: string | null;
  avatar: string | null;
  banned_by: string | null;
};

export const BAN_SPECS: Record<string, FieldSpec> = {
  discord_id: {
    kind: "text",
    label: "Discord 用户 ID",
    min: 1,
    max: 20,
    pattern: /^\d{17,20}$/,
    patternMsg: "Discord 用户 ID 是 17～20 位纯数字",
    hint: "在 Discord 里开启开发者模式后，右键用户头像 → 复制用户 ID",
  },
  reason: { kind: "text", label: "原因", max: 100, hint: "留空则记为「站长手动封禁」" },
};

export const ANNOUNCE_SPECS: Record<string, FieldSpec> = {
  html: {
    kind: "text",
    label: "公告内容（HTML）",
    max: 20000,
    hint: "用户访问首页时看到的内容，支持 HTML；会在隔离的框架里展示，脚本不会执行",
  },
};
/** 让奶妹发到公告频道的消息上限（Discord 单条 2000，后端留了余量） */
export const DC_ANNOUNCE_MAX = 1800;

export type SurveyRow = {
  ts: number;
  name: string | null;
  discord_id: string | null;
  client: string | null;
  understand: string | null;
  improve: string | null;
  improve_other: string | null;
  donate: string | null;
};
export type Survey = {
  total: number;
  rows: SurveyRow[];
  summary: {
    understand: Record<string, number>;
    improve: Record<string, number>;
    donate: Record<string, number>;
    client: Record<string, number>;
  };
};

export function surveyFacts(s: Survey): {
  total: number;
  understoodPct: number;
  willing: number;
  topImprove: [string, number] | null;
} {
  const total = s.rows.length;
  const und = s.summary.understand;
  const count = (k: string): number => und[k] ?? 0;
  const ok = count("完全理解") + count("基本理解");
  const willing = Object.entries(s.summary.donate).reduce((a, [k, v]) => (k.includes("愿意") ? a + v : a), 0);
  const top = Object.entries(s.summary.improve)[0] ?? null;
  return { total, understoodPct: total ? Math.round((ok / total) * 100) : 0, willing, topImprove: top };
}
