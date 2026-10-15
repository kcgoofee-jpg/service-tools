/** 日志相关接口的数据形状。 */

export type Paged = { page: number; per_page: number; total: number; pages: number };

export type AuditItem = {
  id: number;
  ts: number;
  key_id: number | null;
  key_name: string;
  kind: string;
  model: string;
  status: string;
  prompt: string;
  negative: string;
  /** JSON 字符串：{params, char_prompts, char_negatives} */
  extra: string;
  has_image: boolean | number;
  has_thumb: boolean | number;
};
export type AuditResp = Paged & { items: AuditItem[] };

export type AuditExtra = {
  params?: {
    width?: number;
    height?: number;
    seed?: number;
    sampler?: string;
    steps?: number;
    scale?: number;
  };
  char_prompts?: string[];
  char_negatives?: string[];
};

export type UsageLog = {
  id: number;
  ts: number;
  key_id: number | null;
  key_name: string;
  kind: string;
  model: string;
  status: "ok" | "rejected" | "error" | string;
  images: number;
  anlas: number;
  tokens: number;
  unconfirmed_anlas: number;
  detail: string;
  reason: string;
  wait_ms: number;
  dur_ms: number;
  up_status: number;
  rid: string;
};
export type LogsResp = Paged & { logs: UsageLog[] };

export type ActionRow = {
  id: number;
  ts: number;
  actor: string;
  action: string;
  target: string;
  detail: string;
  ok: number | boolean;
};
export type ActionsResp = Paged & { actions: ActionRow[]; action_kinds: string[] };

export type BugRow = {
  sig: string;
  source: string;
  level: "error" | "warn";
  title: string;
  detail: string;
  count: number;
  first_seen: number;
  last_seen: number;
  last_rid: string;
  last_key: number | null;
  last_path: string;
  resolved_at: number | null;
  /** 自动处理的原因码（见 format.ts 的 autoReason）；站长手动处理 / 未处理为空 */
  auto_resolved?: string | null;
  key_name: string;
};
export type BugsResp = { errors: BugRow[]; open: number };

export type FeedbackQuestion = { id: string; label: string; options: string[] };
export type FeedbackItem = {
  id: number;
  ts: number;
  discord_id: string;
  username: string;
  key_id: number | null;
  key_name: string;
  answers: Record<string, string>;
};
export type FeedbackResp = { questions: FeedbackQuestion[]; items: FeedbackItem[] };

export type SurveyRow = {
  ts: number;
  name?: string | null;
  discord_id?: string | null;
  client?: string | null;
  understand?: string | null;
  improve?: string | null;
  improve_other?: string | null;
  donate?: string | null;
};
export type SurveyResp = {
  total: number;
  rows: SurveyRow[];
  summary: Record<string, Record<string, number>>;
};
