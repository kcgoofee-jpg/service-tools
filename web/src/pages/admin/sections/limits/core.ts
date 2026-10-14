/** 额度与保护：接口类型与推算（纯函数，可单测）。 */
import type { FieldSpec } from "../system/core";

/** /admin/api/quota-algo */
export type RateInfo = {
  api: number | null;
  measured: number | null;
  used: number | null;
  status: string;
  notes?: string[];
  quiet?: { rate: number; hours: number; gain: number };
};
export type V5Plan = {
  rate: number;
  percent: number | null;
  k: number;
  global: number;
  people: number;
  each: number;
  rate_info?: RateInfo;
  rate_check?: RateInfo;
  oversell?: { each: number; members: number; global: number } | null;
};
export type QuotaReview = {
  day: string;
  used: number;
  cap: number;
  budget?: number;
  ceiling_hits: number;
  hourly_blocks: number;
  coverage_hours?: number;
  from: [number, number];
  to: [number, number];
  reasons: string[];
  v5_pct?: number | null;
  v5_rate?: number | null;
  v5_used_pct?: number;
  v5_rate_measured?: number;
};
export type QuotaAlgo = {
  last: {
    enabled?: boolean;
    ceiling?: number;
    base?: number;
    v5?: V5Plan;
    members?: number;
    cap?: number;
    updated_at?: number;
    changed_keys?: number;
    review?: QuotaReview | null;
  };
  history: QuotaReview[];
  params: Record<string, number>;
};

/** 官方：100% ≈ 1730 张（23 步估算）→ 17.3 张 / 1%，折成成员常用的 28 步：17.3 × 23 / 28 */
export const DEFAULT_IMAGES_PER_PERCENT = 14.2;

/** /admin/api/modules 里推算要用到的那一小块 */
export type ModulesLite = {
  modules: Array<{ name: string; params: Array<{ name: string; value: unknown }> }>;
};

/** 每 1% 约多少张：取「容量」模块参数（接口给的），拿不到才用默认值 */
export function imagesPerPercent(m: ModulesLite | null | undefined): { value: number; fromApi: boolean } {
  const p = m?.modules.find((x) => x.name === "capacity")?.params.find((x) => x.name.includes("1%"));
  return typeof p?.value === "number"
    ? { value: p.value, fromApi: true }
    : { value: DEFAULT_IMAGES_PER_PERCENT, fromApi: false };
}

export type V5Derivation = {
  rate: number;
  perPercent: number;
  k: number;
  /** rate × perPercent × k 向下取整 */
  calc: number;
  global: number;
  /** 与接口给的全站值是否一致（满电放宽等规则会让两者不同） */
  matches: boolean;
  people: number;
  perPersonRaw: number;
  lo: number;
  hi: number;
  each: number;
  eachMatches: boolean;
};

export function deriveV5(plan: V5Plan, perPercent: number, lo: number, hi: number): V5Derivation {
  const calc = Math.floor(plan.rate * perPercent * plan.k);
  const perPersonRaw = plan.people > 0 ? Math.floor(plan.global / plan.people) : 0;
  const each = Math.max(lo, Math.min(hi, perPersonRaw));
  return {
    rate: plan.rate,
    perPercent,
    k: plan.k,
    calc,
    global: plan.global,
    matches: calc === plan.global,
    people: plan.people,
    perPersonRaw,
    lo,
    hi,
    each: plan.each,
    eachMatches: each === plan.each,
  };
}

export const AP_RULES: Record<string, string> = {
  slots: "名额上限",
  breaker: "全站熔断",
  key_guard: "单个 Key 守护",
  alt_link: "疑似同一人（只观察）",
};
export const AP_MODES: Record<string, { label: string; tone: "warn" | "ok" | "danger" | undefined }> = {
  observe: { label: "观察", tone: "warn" },
  enforce: { label: "执行", tone: "ok" },
  off: { label: "关闭", tone: undefined },
};

/** /admin/api/autopilot */
export type AutopilotRule = { mode: string; value: unknown; why: string };
export type Autopilot = { last: { at?: number; rules?: Record<string, AutopilotRule> }; history: unknown[] };

export function autopilotValue(rule: string, v: unknown): string {
  if (rule === "slots") return typeof v === "number" ? `名额上限 → ${v}` : String(v ?? "—");
  if (rule === "breaker") return v ? "暂停出图 5 分钟" : "正常";
  if (rule === "key_guard") {
    const list = Array.isArray(v) ? (v as Array<{ name?: string; action?: string }>) : [];
    return list.length
      ? list.map((x) => `${x.name ?? "?"}：${x.action === "reset" ? "重置" : "暂停"}`).join("；")
      : "无";
  }
  if (rule === "alt_link") {
    const list = Array.isArray(v) ? v : [];
    return list.length ? `${list.length} 对` : "无";
  }
  return v == null ? "—" : String(v);
}

/** /admin/api/guard */
export type GuardField = { default: number; min: number; max: number; label: string };
export type Guard = {
  values: Record<string, number>;
  fields: Record<string, GuardField>;
  quiet_now: boolean;
  hourly_cap_now: number;
  accounts: Array<{
    position: number;
    usable: boolean;
    today: number;
    today_units: number;
    this_hour: number;
  }>;
  queued_images: number;
};

/** 保护字段的短标题 / 说明。接口的 label 是整段说明文字，这里取短名，说明放提示里。 */
export const GUARD_COPY: Record<string, { label: string; hint: string; unit?: string }> = {
  account_daily_cap: {
    label: "每个上游账号每日上限",
    hint: "每天（7 点重置）最多出多少张；0 = 不限",
    unit: "张",
  },
  daily_burst: {
    label: "7 点先放",
    hint: "每日上限按时段放行：7 点先放这么多张，之后每小时补「每日上限 ÷ 24」张；0 = 不分时段",
    unit: "张",
  },
  account_hourly_cap: {
    label: "每个上游账号每小时上限",
    hint: "按最近 60 分钟（滚动窗口）计；0 = 不限。上游返回 429 时系统会自动减半，平稳一天后自动上调一档，所以手动改的值可能被自动调整接管",
    unit: "张",
  },
  quiet_start: { label: "安静时段开始", hint: "北京时间整点；与结束相同 = 不设安静时段", unit: "点" },
  quiet_end: { label: "安静时段结束", hint: "北京时间整点", unit: "点" },
  quiet_hourly_cap: { label: "安静时段每小时上限", hint: "0 = 安静时段完全暂停出图", unit: "张" },
  interval_jitter: {
    label: "出图间隔随机增加",
    hint: "两次出图间隔额外随机增加 0～N 秒，避免节奏过于规整",
    unit: "秒",
  },
  key_image_queue: {
    label: "每把 Key 额外可排队",
    hint: "每把 Key 在生成中的那张之外，最多再排几张",
    unit: "张",
  },
  queue_per_account: { label: "每个账号全站排队上限", hint: "每个上游账号全站最多同时排几张图", unit: "张" },
};
export const GUARD_CORE = ["account_hourly_cap", "account_daily_cap", "daily_burst"];
export const GUARD_ADVANCED = [
  "quiet_start",
  "quiet_end",
  "quiet_hourly_cap",
  "interval_jitter",
  "key_image_queue",
  "queue_per_account",
];
/** 由动态额度算法每天 7 点写入，后台不让手改（改了会被覆盖） */
export const GUARD_ALGO_OWNED = new Set(["base_daily_images"]);

export function guardSpecs(g: Guard): Record<string, FieldSpec> {
  const out: Record<string, FieldSpec> = {};
  for (const [name, f] of Object.entries(g.fields)) {
    if (GUARD_ALGO_OWNED.has(name)) continue;
    const copy = GUARD_COPY[name];
    out[name] = {
      kind: "int",
      label: copy?.label ?? f.label,
      min: f.min,
      max: f.max,
      ...(copy?.hint ? { hint: `${copy.hint}（${f.min}～${f.max}，默认 ${f.default}）` } : {}),
      ...(copy?.unit ? { unit: copy.unit } : {}),
    };
  }
  return out;
}

/** 按时段放行的推算：7 点先放 burst 张，之后每小时补 round(日上限 ÷ 24) 张 */
export function pacedText(dailyCap: number, burst: number): string | null {
  if (!dailyCap || !burst) return null;
  return `7 点先放 ${burst} 张，之后每小时补 ${Math.round(dailyCap / 24)} 张（= ${dailyCap} ÷ 24）`;
}

/** /admin/api/runtime-limits 的范围（与后端 RUNTIME_LIMIT_BOUNDS 一致） */
export const RUNTIME_SPECS: Record<string, FieldSpec> = {
  queue_timeout: {
    kind: "int",
    label: "单阶段排队等待上限",
    min: 15,
    max: 300,
    unit: "秒",
    hint: "一张图在某个排队环节最多等多久，超时就放弃（15～300）",
  },
  key_image_min_interval: {
    kind: "int",
    label: "同一把 Key 的图片任务间隔",
    min: 15,
    max: 120,
    unit: "秒",
    hint: "同一个成员两次出图至少隔多久（15～120）",
  },
  image_min_interval: {
    kind: "int",
    label: "每个上游账号的图片请求间隔",
    min: 15,
    max: 120,
    unit: "秒",
    hint: "同一个上游账号两次发图至少隔多久（15～120）",
  },
  image_429_cooldown_seconds: {
    kind: "int",
    label: "上游 429 后最短冷却",
    min: 60,
    max: 3600,
    unit: "秒",
    hint: "上游限流后全站暂停出图的最短时间（60～3600）",
  },
};

/** /admin/api/settings 的字段范围（与后端 put_settings 一致） */
export const BUDGET_SPECS: Record<string, FieldSpec> = {
  global_monthly_anlas: {
    kind: "float",
    label: "全站月度 Anlas 预算",
    min: 0,
    max: 1_000_000,
    hint: "0 = 不限",
  },
  global_daily_v5: {
    kind: "int",
    label: "全站每日 V5 张数",
    min: 0,
    max: 100_000,
    unit: "张",
    hint: "0 = 不限",
  },
  v5_alert_threshold: {
    kind: "int",
    label: "V5 官方余量告警阈值",
    min: 1,
    max: 100,
    unit: "%",
    hint: "账号 V5 剩余低于这个百分比时告警（1～100）",
  },
  member_daily_anlas: {
    kind: "int",
    label: "每人每日 Anlas 上限",
    min: 0,
    max: 100_000,
    hint: "0 = 不限；只对没手动设 Anlas 日额度的 Key 生效（Vibe 编码一张参考图约 2 点）",
  },
};

/** 动态额度参数（PUT /quota-algo 接受的、线上算法真正读取的几项） */
export const ALGO_SPECS: Record<string, FieldSpec> = {
  quota_step: {
    kind: "int",
    label: "放宽步长",
    min: 1,
    max: 500,
    unit: "张",
    hint: "有人用满当天额度、且账号还有余量时，次日 7 点每人 +N 张（1～500）",
  },
  quota_ceiling_max: {
    kind: "int",
    label: "V4.5 每人额度上限",
    min: 10,
    max: 5000,
    unit: "张",
    hint: "算法放宽时的天花板（10～5000）",
  },
  quota_base_min_fair: {
    kind: "int",
    label: "V4.5 每人额度下限",
    min: 1,
    max: 2000,
    unit: "张",
    hint: "需求拥挤、按需求平分时的底线（1～2000）",
  },
  quota_v5_min: {
    kind: "int",
    label: "V5 每人额度下限",
    min: 0,
    max: 500,
    unit: "张",
    hint: "按人头平分后太少时的底线（0～500）",
  },
  quota_v5_max: {
    kind: "int",
    label: "V5 每人额度上限",
    min: 1,
    max: 500,
    unit: "张",
    hint: "按人头平分后太多时的天花板（1～500）",
  },
};
export const MANUAL_N_SPEC: Record<string, FieldSpec> = {
  quota_target_avg: {
    kind: "int",
    label: "手动指定今天每人 V4.5 额度",
    min: 10,
    max: 2000,
    unit: "张",
    hint: "保存后立即同步给所有由算法管理的 Key；明天 7 点起算法继续按昨天的用量微调（10～2000）",
  },
};
