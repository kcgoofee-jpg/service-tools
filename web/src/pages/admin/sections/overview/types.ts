/** 总览用到的接口形状：/admin/api/overview、status、status-codes，以及提醒要看的 errors / discord-bot / modules。 */

export type FeatBucket = {
  ok: number;
  error: number;
  rejected: number;
  images: number;
  tokens: number;
  anlas: number;
};

export type PoolAccount = {
  token_id: string;
  position: number;
  admin_enabled: boolean;
  /** 401 失效 */
  disabled: boolean;
  /** 限流剩余秒数 */
  blocked_for: number;
};

export type Overview = {
  today: {
    images: number;
    member_images: number;
    anlas: number;
    text_tokens: number;
    requests: number;
    v5: number;
    unconfirmed_requests: number;
    unconfirmed_anlas: number;
  };
  active_members?: number;
  members_total?: number;
  keys_total: number;
  keys_active: number;
  week: Array<{
    day: string;
    images: number;
    anlas: number;
    text_tokens: number;
    requests: number;
    v5: number;
  }>;
  month: { anlas: number; unconfirmed_requests: number; unconfirmed_anlas: number };
  pool: PoolAccount[];
  pool_configured: boolean;
  anlas_budget: number;
  v5_limit: number;
  feature_usage: Array<{ id: string; label: string; today: FeatBucket; week: FeatBucket }>;
  log_since: number | null;
};

export type StatusResp = {
  alerts: { configured: boolean; sent: number };
  audit: {
    prompts: boolean;
    thumbs: boolean;
    retention_days: number;
    image_retention_days: number;
    image_cap_mb: number;
    image_bytes: number;
    image_keep_hours: number;
  };
  upstream: {
    status: "ok" | "degraded" | "idle";
    recent: number;
    failed: number;
    image_cooldown_seconds: number;
  };
  protection: {
    auth_fail_max: number;
    auth_fail_window: number;
    auth_block_seconds: number;
    login_max_attempts: number;
  };
  disk: { free_mb: number; total_mb: number };
};

export type LiveResp = { guard?: { hour?: number; hour_cap?: number }; quota?: { v5_cap?: number } };

export type BugsResp = {
  open: number;
  errors: Array<{ level: "error" | "warn"; resolved_at: number | null }>;
};
export type BotResp = { status?: { online?: boolean; seen_at?: number } };
export type ModulesResp = {
  modules: Array<{ enabled: boolean; last?: { ok?: boolean; checks?: Array<{ level: string }> } | null }>;
};

export type StatusCodeRow = { code: number; n: number; pct: number; meaning: string };
export type StatusCodesResp = {
  total: number;
  codes: StatusCodeRow[];
  upstream: StatusCodeRow[];
  upstream_total: number;
};
