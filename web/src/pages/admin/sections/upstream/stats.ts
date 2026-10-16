/** /admin/api/upstream-stats：每个上游账号的请求统计与 Anlas 推算（只读）。类型与文案（纯函数，可单测）。 */
import { anlasText } from "./core";

export type WindowStats = {
  ok: number;
  error: number;
  rejected: number;
  avg_wait_ms: number | null;
  p90_wait_ms: number | null;
  up_429: number;
  up_403: number;
  up_401: number;
};
export type AnlasEstimate = {
  cycle_rolled: boolean;
  balance: number | null;
  fixed: number | null;
  purchased: number | null;
  expires_at: number | null;
  days_left: number | null;
  avg_daily: number;
  projection: "unknown" | "runs_out" | "waste";
  projection_value: number | null;
  snapshot_age_days: number;
  stale: boolean;
  approximate: boolean;
  unattributed_anlas: number;
};
export type AccountStats = {
  token_id: string;
  position: number;
  usable: boolean;
  images_today: number;
  hour_count: number;
  w1h: WindowStats;
  w24h: WindowStats;
  load_share_24h: number | null;
  balance_share: number | null;
  balance: AnlasEstimate | null;
};
export type UpstreamStats = {
  generated_at: number;
  accounts: AccountStats[];
  unattributed_24h: number;
};

export const requestsText = (w: WindowStats): string =>
  `成功 ${w.ok} · 出错 ${w.error} · 选号后被拒 ${w.rejected}`;

const seconds = (ms: number): string => `${(ms / 1000).toFixed(1)} 秒`;
export const waitText = (w: WindowStats): string =>
  w.avg_wait_ms == null || w.p90_wait_ms == null
    ? "—"
    : `平均 ${seconds(w.avg_wait_ms)} · p90 ${seconds(w.p90_wait_ms)}`;

export const upstreamErrorsText = (w: WindowStats): string =>
  `429 × ${w.up_429} · 403 × ${w.up_403} · 401 × ${w.up_401}`;

/** 到期预测：按近 3 天日均用量，固定额度用得完还是会浪费 */
export function projectionText(b: AnlasEstimate): string {
  if (b.cycle_rolled) return "周期已滚动，需要重新核对";
  if (b.projection === "runs_out" && b.projection_value != null)
    return `固定额度将在到期前用完（约 ${b.projection_value.toFixed(1)} 天后）`;
  if (b.projection === "waste" && b.projection_value != null)
    return b.projection_value < 1
      ? "到期时刚好用完"
      : `到期时预计浪费约 ${anlasText(Math.round(b.projection_value))} Anlas`;
  return "—";
}

export const daysLeftText = (b: AnlasEstimate): string =>
  b.days_left == null ? "—" : b.days_left < 1 ? "不到 1 天" : `${Math.floor(b.days_left)} 天`;

export const ageText = (days: number): string => (days < 1 ? "今天核对过" : `${Math.round(days)} 天前核对`);
