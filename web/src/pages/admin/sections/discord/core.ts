/** Discord 分区：接口类型与小工具（纯函数，可单测）。 */
import type { FieldSpec } from "../system/core";

export type BotStatus = {
  online?: boolean;
  user?: string;
  guild?: string;
  latency_ms?: number | null;
  ready_at?: number;
  version?: string;
  seen_at?: number;
};
export type BotData = { status: BotStatus };

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
