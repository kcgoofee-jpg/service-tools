/** 领 Key 与功能：接口类型、表单规格与请求体（纯函数，可单测）。 */
import type { FieldSpec } from "../system/core";

export type RegistrationInfo = {
  configured: boolean;
  open: boolean;
  max_users: number;
  /** null = 全部已开放功能（含以后新开放的） */
  features: string[] | null;
  daily_images: number;
  daily_v5: number;
  image_scope: "legacy" | "all";
  expires_days: number;
  role_guild: string;
  role_id: string;
  role_note: string;
  active: number;
  reset_at: string;
  v5_capacity: { need: number; global: number; short: boolean; message: string };
};
export type AuditInfo = {
  prompts: boolean;
  thumbs: boolean;
  retention_days: number;
  image_cap_mb: number;
  image_bytes: number;
  image_keep_hours: number;
};
export type Ops = {
  registration: RegistrationInfo;
  features: { global: Record<string, boolean>; labels: Record<string, string>; new_key: string[] };
  audit: AuditInfo;
};

/** 消耗官方 Anlas（付费点数）的功能：开启前要确认（接口没有给出这个清单，与 app/features.py 的 ANLAS_FEATURES 对应） */
export const ANLAS_FEATURES: Record<string, string> = {
  vibe: "每次编码参考图约 2 Anlas",
  upscale: "按图片大小扣 Anlas",
  augment: "按图片大小扣 Anlas",
};

const DISCORD_ID = /^\d{15,21}$/;

export const REG_BASE_SPECS: Record<string, FieldSpec> = {
  max_users: { kind: "int", label: "名额上限", min: 0, max: 1000, unit: "人", hint: "0 = 不限" },
  expires_days: { kind: "int", label: "Key 有效期", min: 0, max: 3650, unit: "天", hint: "0 = 永久" },
  daily_images: {
    kind: "int",
    label: "新成员每日 V4.5 图片",
    min: 0,
    max: 100_000,
    unit: "张",
    hint: "0 = 不限",
  },
  daily_v5: { kind: "int", label: "新成员每日 V5", min: 0, max: 100_000, unit: "张", hint: "0 = 不限" },
  image_scope: {
    kind: "select",
    label: "新成员默认的图片模型范围",
    options: [
      { value: "legacy", label: "仅 V4.5 及以下" },
      { value: "all", label: "含 V5" },
    ],
  },
  role_guild: {
    kind: "text",
    label: "服务器 ID",
    max: 21,
    pattern: DISCORD_ID,
    patternMsg: "服务器 ID 应是 15～21 位的 Discord 数字 ID，或留空",
  },
  role_id: {
    kind: "text",
    label: "身份组 ID",
    max: 21,
    pattern: DISCORD_ID,
    patternMsg: "身份组 ID 应是 15～21 位的 Discord 数字 ID，或留空",
  },
  role_note: {
    kind: "text",
    label: "显示名",
    max: 60,
    hint: "领取失败时告诉成员需要什么身份，例如「创作者」",
  },
  features_mode: {
    kind: "select",
    label: "新成员默认开通的功能",
    options: [
      { value: "all", label: "全部已开放功能（含以后新开放的）" },
      { value: "custom", label: "仅选中的功能" },
    ],
  },
};
export const FEATURE_PREFIX = "feat_";

export function regSpecs(labels: Record<string, string>): Record<string, FieldSpec> {
  const out: Record<string, FieldSpec> = { ...REG_BASE_SPECS };
  for (const [name, label] of Object.entries(labels)) out[FEATURE_PREFIX + name] = { kind: "bool", label };
  return out;
}

/** 接口里的注册设置 → 表单的服务端值 */
export function regServer(r: RegistrationInfo, labels: Record<string, string>): Record<string, unknown> {
  const out: Record<string, unknown> = {
    max_users: r.max_users,
    expires_days: r.expires_days,
    daily_images: r.daily_images,
    daily_v5: r.daily_v5,
    image_scope: r.image_scope,
    role_guild: r.role_guild,
    role_id: r.role_id,
    role_note: r.role_note,
    features_mode: r.features == null ? "all" : "custom",
  };
  for (const name of Object.keys(labels))
    out[FEATURE_PREFIX + name] = r.features == null ? true : r.features.includes(name);
  return out;
}

/** 限定身份组：服务器 ID 与身份组 ID 必须同时填或同时留空 */
export function roleProblem(guild: string, role: string): string | null {
  return Boolean(guild.trim()) !== Boolean(role.trim())
    ? "限定身份组的服务器 ID 与身份组 ID 必须同时填写，或同时留空"
    : null;
}

/** 表单改动 → PUT /ops/registration 的请求体（只含改过的字段；功能清单与身份组成组提交） */
export function regBody(
  changes: Record<string, number | string | boolean>,
  raw: (name: string) => string | boolean,
  labels: string[],
  notify: boolean,
): Record<string, unknown> {
  const body: Record<string, unknown> = {};
  let featuresTouched = false;
  let roleTouched = false;
  for (const [k, v] of Object.entries(changes)) {
    if (k === "features_mode" || k.startsWith(FEATURE_PREFIX)) featuresTouched = true;
    else if (k === "role_guild" || k === "role_id") roleTouched = true;
    else body[k] = v;
  }
  if (featuresTouched) {
    body.features =
      raw("features_mode") === "all" ? null : labels.filter((n) => raw(FEATURE_PREFIX + n) === true);
  }
  if (roleTouched) {
    body.role_guild = String(raw("role_guild")).trim();
    body.role_id = String(raw("role_id")).trim();
  }
  if (!notify) body.notify = false;
  return body;
}

/** 名额 × 每人 V5 与全站 V5 日限的关系（与后端 v5_capacity 同一口径，但用表单里的最新输入实时算） */
export function v5Need(seats: number, perV5: number, scope: string): number {
  return scope === "all" && seats > 0 && perV5 > 0 ? seats * perV5 : 0;
}

export const AUDIT_SPECS: Record<string, FieldSpec> = {
  prompts: { kind: "bool", label: "记录提示词" },
  thumbs: { kind: "bool", label: "保存原图" },
  image_keep_hours: {
    kind: "int",
    label: "原图保存时长",
    min: 0,
    max: 8760,
    unit: "小时",
    hint: "到点自动删；0 = 只按容量上限删",
  },
  retention_days: { kind: "int", label: "提示词保留", min: 0, max: 3650, unit: "天", hint: "0 = 长期保留" },
  image_cap_mb: {
    kind: "int",
    label: "原图容量上限",
    min: 0,
    max: 500_000,
    unit: "MB",
    hint: "超过后删最旧的原图；0 = 不限",
  },
};

export function auditServer(a: AuditInfo): Record<string, unknown> {
  return {
    prompts: a.prompts,
    thumbs: a.thumbs,
    image_keep_hours: a.image_keep_hours,
    retention_days: a.retention_days,
    image_cap_mb: a.image_cap_mb,
  };
}
