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
