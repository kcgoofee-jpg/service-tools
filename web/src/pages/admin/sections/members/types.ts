/** 成员相关接口的数据形状（/admin/api/members、/admin/api/keys、/admin/api/bans 等）。 */

export type Role = "regular" | "invite" | "test" | "admin";
export const ROLE_LABEL: Record<Role, string> = {
  regular: "普通",
  invite: "邀请",
  test: "测试",
  admin: "站长",
};

export type Tag = { tag: string; note: string; by: string; ts: number };
export type AutoTag = { tag: string; count: number; note?: string };
export type Discord = {
  id: string;
  username: string | null;
  display_name: string | null;
  avatar_url: string;
  profile_url: string;
};

export type Member = {
  id: number;
  name: string;
  enabled: boolean;
  is_admin: boolean;
  is_test: boolean;
  role: Role;
  role_label: string;
  discord_id: string | null;
  discord: Discord | null;
  created_at: number;
  last_used_at: number | null;
  expires_at: number | null;
  daily_images: number;
  daily_v5: number;
  allow_anlas: boolean;
  daily_anlas: number;
  image_model_scope: string;
  quota_auto: boolean;
  today: { images: number; v5: number; anlas: number; requests: number };
  week: { images: number; v5: number; anlas: number; requests: number };
  total_images: number;
  registered_at: number | null;
  first_image_at: number | null;
  last_image_at: number | null;
  rejected_24h: number;
  rejected_why: Array<[string, number]>;
  share: { paused_until: number } | null;
  reset_today: { count: number; last_ts: number } | null;
  tags: Tag[];
  auto_tags: AutoTag[];
  hours24: number[];
  key_count: number;
  all_key_ids: number[];
};

export type KeyRow = {
  id: number;
  name: string;
  token: string;
  enabled: boolean;
  daily_images: number;
  daily_anlas: number;
  daily_v5: number;
  monthly_anlas: number;
  rpm: number;
  allow_anlas: boolean;
  allow_img2img: boolean;
  exclude_global_v5: boolean;
  image_model_scope: string;
  is_admin: boolean;
  is_test: boolean;
  quota_auto: boolean;
  role: Role;
  /** null = 全部已开放功能；数组 = 仅这些功能 */
  features: string[] | null;
  expires_at: number | null;
  created_at: number;
  last_used_at: number | null;
  used: { images: number; anlas: number; v5: number; requests: number };
};

/** 成员 + 对应的 Key 行（编辑与展示需要的字段分在两个接口里） */
export type MemberRow = Member & { k?: KeyRow | undefined };

export type MembersResp = { members: Member[]; inactivity_days: number };
export type KeysResp = { keys: KeyRow[] };

export type Ban = {
  discord_id: string;
  created_at: number;
  reason: string;
  username: string;
  display_name: string;
  avatar: string;
  banned_by: string;
};

export type Coupon = {
  id: number;
  kind: string;
  note: string;
  by: string;
  created_at: number;
  expires_at: number;
  used_at: number | null;
};

export type OpsSnapshot = {
  registration: { daily_images: number; daily_v5: number; expires_days: number; features: string[] | null };
  features: { labels: Record<string, string>; new_key: string[] | null };
  audit: {
    prompts: boolean;
    thumbs: boolean;
    retention_days: number;
    image_cap_mb: number;
    image_bytes: number;
    image_keep_hours: number;
  };
};

/** 功能名（后端 features.FEATURES 的副本；接口给了 labels 就以接口为准） */
export const FEATURE_LABELS: Record<string, string> = {
  image: "文生图",
  upscale: "放大",
  augment: "导演工具（线稿、上色等）",
  vibe: "Vibe 编码",
  tags: "标签补全",
};

/** 3.0 已经没有文本 / 语音功能：旧网关若还带着这两项，页面里也不列出来 */
const GONE_FEATURES = new Set(["text", "voice"]);
export const isLiveFeature = (id: string): boolean => !GONE_FEATURES.has(id);
export const featureIds = (labels: Record<string, string>): string[] =>
  Object.keys(labels).filter(isLiveFeature);
