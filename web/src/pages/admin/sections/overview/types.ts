/** 总览用到的接口形状：/admin/api/overview，以及提醒要看的 errors / discord-bot。定时任务（/admin/api/jobs）的形状在 system/jobs-core.ts。 */
import type { PoolToken } from "../upstream/core";

export type FeatBucket = {
  ok: number;
  error: number;
  rejected: number;
  images: number;
  tokens: number;
  anlas: number;
};

export type Overview = {
  today: { images: number; member_images: number; v5: number };
  active_members?: number;
  members_total?: number;
  keys_total: number;
  keys_active: number;
  /** 令牌池：提醒只看每个账号是否启用 / 失效 */
  pool: Array<Pick<PoolToken, "position" | "admin_enabled" | "disabled">>;
  pool_configured: boolean;
  v5_limit: number;
  feature_usage: Array<{ id: string; label: string; today: FeatBucket; week: FeatBucket }>;
  log_since: number | null;
};

export type BugsResp = {
  open: number;
  errors: Array<{ level: "error" | "warn"; resolved_at: number | null }>;
};
export type BotResp = { status?: { online?: boolean; seen_at?: number } };

export type StatusCodeRow = { code: number; n: number; pct: number; meaning: string };
export type FailureReasonRow = {
  label: string;
  reason: string;
  status: "rejected" | "error" | string;
  n: number;
  pct: number;
};
export type StatusCodesResp = {
  total: number;
  codes: StatusCodeRow[];
  upstream: StatusCodeRow[];
  upstream_total: number;
  reasons: FailureReasonRow[];
  reasons_total: number;
};
