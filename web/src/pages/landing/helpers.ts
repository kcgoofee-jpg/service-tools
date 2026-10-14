import type { MeMember } from "./types";

/** Key 打码：前 7 位 + … + 后 4 位（太短的原样显示）。 */
export function maskKey(key: string): string {
  return key.length > 14 ? `${key.slice(0, 7)}…${key.slice(-4)}` : key;
}

/** 重置券剩余时间：先按小时向上取整再拆成「天 + 小时」，避免出现「2 天 24 小时」。 */
export function couponLeft(expiresAt: number, nowSec = Date.now() / 1000): string {
  const total = Math.ceil(Math.max(0, expiresAt - nowSec) / 3600);
  const days = Math.floor(total / 24);
  const hours = total % 24;
  return days >= 1 ? `${days} 天${hours ? ` ${hours} 小时` : ""}` : `${hours} 小时`;
}

export type QuotaRow = {
  id: "legacy" | "v5" | "anlas";
  label: string;
  used: number;
  /** 0 = 不限 */
  cap: number;
  hint?: string;
};

/**
 * 成员今日额度三行（字段含义沿用旧版首页）：
 * - V4.5：用 today.legacy（免费图张数），没有就退回 images − v5；上限 daily_images（0 = 不限）
 * - V5：上限 daily_v5；没配上限且这把 Key 的模型范围不是 all 时整行隐藏；0 + all = 个人不限（仍受全站日上限约束）
 * - Anlas：只有配了 daily_anlas 才显示
 */
export function quotaRows(me: MeMember): QuotaRow[] {
  const t = me.today ?? {};
  const v5 = t.v5 ?? 0;
  const legacy = t.legacy != null ? t.legacy : Math.max(0, (t.images ?? 0) - v5);
  const rows: QuotaRow[] = [
    { id: "legacy", label: "V4.5", used: legacy, cap: (t.daily_images ?? 0) > 0 ? (t.daily_images ?? 0) : 0 },
  ];
  const capV5 = (t.daily_v5 ?? 0) > 0 ? (t.daily_v5 ?? 0) : 0;
  if (capV5 > 0 || me.image_scope === "all") {
    rows.push({
      id: "v5",
      label: "V5",
      used: v5,
      cap: capV5,
      ...(capV5 === 0 ? { hint: "个人不限额度，仍受全站日上限约束" } : {}),
    });
  }
  if ((t.daily_anlas ?? 0) > 0) {
    rows.push({ id: "anlas", label: "Anlas", used: t.anlas ?? 0, cap: t.daily_anlas ?? 0 });
  }
  return rows;
}

/** 每日重置的整点：接口给什么用什么（成员才有），拿不到按站长规定的 7 点。 */
export const resetHour = (me: MeMember | null | undefined): number => me?.today?.reset_hour ?? 7;

export const displayName = (me: MeMember): string =>
  me.discord?.display_name || me.discord?.username || "成员";
