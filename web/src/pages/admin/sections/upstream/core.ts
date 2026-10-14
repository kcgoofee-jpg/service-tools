/** 上游：接口类型与小工具（纯函数，可单测）。 */
import type { FieldSpec } from "../system/core";

/** /admin/api/overview 里的令牌池（掩码后的 Token，从不含完整 Token） */
export type PoolToken = {
  token_id: string;
  position: number;
  token: string;
  usable: boolean;
  admin_enabled: boolean;
  disabled: boolean;
  fails: number;
  blocked_for: number;
  last_ok: number;
  allow_anlas: boolean;
  image_concurrency: number;
  image_active: number;
  v5_daily_limit: number;
  v5_used: number;
  images_today: number;
};
export type PoolData = { pool: PoolToken[]; pool_configured: boolean };

export type TokenState = { text: string; tone: "ok" | "warn" | "danger" | undefined };
export function tokenState(p: PoolToken): TokenState {
  if (!p.admin_enabled) return { text: "已停用", tone: undefined };
  if (p.disabled) return { text: "已失效（401）", tone: "danger" };
  if (p.blocked_for > 0) return { text: `限流中 ${p.blocked_for} 秒`, tone: "warn" };
  return { text: "正常", tone: "ok" };
}

export const TOKEN_SPECS: Record<string, FieldSpec> = {
  v5_daily_limit: {
    kind: "int",
    label: "免费 V5 日限",
    min: 0,
    max: 100_000,
    unit: "张",
    hint: "0 = 不限；只限制这把上游 Token",
  },
  image_concurrency: {
    kind: "int",
    label: "图片并发",
    min: 1,
    max: 4,
    hint: "同时在途的图片数，1～4；调高会增加上游限流风险",
  },
};

/** 上游 Token 的格式：以 pst- 开头（NovelAI 账号设置 → Get Persistent API Token） */
export function tokenFormatProblem(t: string): string | null {
  const v = t.trim();
  if (!v) return "请粘贴 Token";
  if (!v.startsWith("pst-"))
    return "Token 格式不对：应以 pst- 开头（NovelAI 账号设置 → Get Persistent API Token）";
  return null;
}

/** /admin/api/allowance */
export type AllowanceAccount = {
  account: number;
  percent: number | null;
  recharge_per_day: number | null;
  is_negative: boolean | null;
  checked_at: number | null;
  low: boolean;
  uncertain: boolean;
  error: string | null;
  stale: boolean;
};
export type Allowance = { threshold: number; accounts: AllowanceAccount[]; mode: string };

/** 恢复速度的推算：每日 % → 每 1% 约几小时、折成多少张 V5 */
export function rechargeDerivation(
  perDay: number,
  perPercent: number,
): { hoursPerPercent: number; imagesPerDay: number } {
  return { hoursPerPercent: 24 / perDay, imagesPerDay: Math.round(perDay * perPercent) };
}

/** /admin/api/perf */
export type PerfWindow = {
  requests: number;
  ok: number;
  images: number;
  rate_limited: number;
  auth: number;
  server: number;
  other: number;
  success_rate: number | null;
  samples: number;
  p50_ms: number | null;
  p90_ms: number | null;
  avg_wait_ms: number | null;
  p90_wait_ms: number | null;
  actual_per_hour: number | null;
  capacity_per_hour: number | null;
  member_capacity_per_hour: number | null;
};
export type PerfHour = {
  t: number;
  ok: number;
  err: number;
  r429: number;
  images: number;
  p50_ms: number | null;
};
export type PerfFamily = {
  h1: PerfWindow;
  d1: PerfWindow;
  baseline: PerfWindow;
  hourly: PerfHour[];
  baseline_ready: boolean;
};
export type Perf = {
  families: Record<string, PerfFamily>;
  flags: Array<{ code: string; family: string; level: "bad" | "warn"; text: string }>;
  slots: number;
  site_interval: number;
  key_interval: number;
  generated_at: number;
};

export const secText = (ms: number | null | undefined): string =>
  ms == null ? "—" : `${(ms / 1000).toFixed(1)} 秒`;
export const rateText = (r: number | null | undefined): string =>
  r == null ? "—" : `${Math.round(r * 100)}%`;

/** 每小时请求量图的峰值 */
export function hourlyPeak(hours: PerfHour[]): number {
  return Math.max(0, ...hours.map((h) => h.ok + h.err));
}

/** /admin/api/reconciliation */
export type Comparison = {
  reason: string | null;
  since?: number;
  balance_decrease?: number;
  recorded?: number;
  difference?: number;
  unconfirmed_anlas?: number;
  unconfirmed_requests?: number;
};
export type ReconRecord = {
  id: number;
  checked_at: number;
  accounts: Record<string, unknown>;
  balance: number;
  comparison: Comparison;
};
export type Recon = { history: ReconRecord[]; retry_after: number; running: boolean; csrf_token: string };

export const RECON_REASONS: Record<string, string> = {
  first: "已记下当前余额",
  accounts_changed: "账号列表有变化，已重新记录余额",
  cycle_changed: "订阅周期已更新，已重新记录余额",
  balance_increased: "余额增加了，已重新记录余额",
  ledger_reset: "本站用量记录有变动，已重新记录余额",
};

export const anlasText = (n: number | null | undefined): string =>
  n == null ? "—" : n.toLocaleString("zh-CN", { maximumFractionDigits: 2 });
