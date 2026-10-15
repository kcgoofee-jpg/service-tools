/** 日志相关的显示格式化（纯函数）：模型短名、功能名、耗时。 */

/** 功能 id → 完整名 / 短名（后端 features.FEATURES 的副本） */
export const FEATURES: Array<{ id: string; label: string; short: string }> = [
  { id: "image", label: "文生图", short: "文生图" },
  { id: "upscale", label: "放大", short: "放大" },
  { id: "augment", label: "导演工具（线稿、上色等）", short: "导演工具" },
  { id: "vibe", label: "Vibe 编码", short: "Vibe 编码" },
  { id: "tags", label: "标签补全", short: "标签补全" },
  { id: "text", label: "文本生成 / OpenAI 兼容聊天", short: "文本 / 聊天" },
  { id: "voice", label: "语音合成", short: "语音合成" },
];

/** 用量日志 kind → 功能 id（后端 features.KIND_FEATURE 的副本） */
const KIND_FEATURE: Record<string, string> = {
  image: "image",
  image_stream: "image",
  upscale: "upscale",
  "augment-image": "augment",
  vibe_encode: "vibe",
  tags: "tags",
  text: "text",
  chat: "text",
  voice: "voice",
};

export function kindLabel(kind: string | null | undefined): string {
  const id = KIND_FEATURE[kind ?? ""];
  const f = FEATURES.find((x) => x.id === id);
  return f ? f.short : kind || "—";
}

/** 模型短名：nai-diffusion-4-5-full → V4.5（完整名放 title） */
export function shortModel(model: string | null | undefined): string {
  const m = String(model ?? "");
  if (!m) return "";
  const base = /diffusion-5/.test(m)
    ? "V5"
    : /diffusion-4-5/.test(m)
      ? "V4.5"
      : /diffusion-4/.test(m)
        ? "V4"
        : /furry-3/.test(m)
          ? "Furry V3"
          : /diffusion-3/.test(m)
            ? "V3"
            : /diffusion-2/.test(m)
              ? "V2"
              : /furry/.test(m)
                ? "Furry"
                : /^nai-diffusion$/.test(m)
                  ? "V1"
                  : "";
  if (!base) return m.length > 18 ? `${m.slice(0, 16)}…` : m;
  return base + (/curated/.test(m) ? " Curated" : "") + (/inpainting/.test(m) ? " 重绘" : "");
}

/** 耗时：生成 4.2s、等 12s（等待 < 100ms 不显示） */
export function timing(waitMs: number, durMs: number): { gen: string; wait: string } {
  const sec = (ms: number) => (ms >= 10000 ? `${Math.round(ms / 1000)}s` : `${(ms / 1000).toFixed(1)}s`);
  return { gen: durMs ? sec(durMs) : "", wait: waitMs >= 100 ? sec(waitMs) : "" };
}

/** Bug 来源 → 中文 */
const BUG_SRC: Record<string, string> = {
  request: "接口异常",
  upstream: "上游故障",
  gate: "网关故障",
  accounting: "记账写入",
  disconnect: "客户端断开",
  audit: "生成记录",
  key_sources: "来源统计",
  anlas_pool: "Anlas 分配",
  cleanup: "闲置回收",
  registration_reset: "定时清空",
  scheduling: "出图调度",
  share_guard: "防分享",
  quota_algo: "动态额度",
  audit_image_purge: "原图清理",
  touch_key: "活跃心跳",
  guard_seed: "账号保护",
  kernel: "规则内核",
  "log:rejected": "拦截日志",
};
const MAINT_TASKS: Record<string, string> = {
  perf: "性能采集",
  registrar: "名额发放",
  disk: "磁盘监控",
  purge: "历史清理",
  quota: "额度重置",
};
const WEB_PAGES: Record<string, string> = {
  admin: "后台",
  landing: "首页",
  status: "状态页",
  claim: "领 Key 页",
};
export function bugSource(s: string): string {
  if (BUG_SRC[s]) return BUG_SRC[s];
  if (s.startsWith("maintenance:")) return `维护任务 · ${MAINT_TASKS[s.slice(12)] ?? s.slice(12)}`;
  if (s.startsWith("web:")) return `网页脚本 · ${WEB_PAGES[s.slice(4)] ?? s.slice(4)}`;
  if (s.startsWith("module:")) return `模块 · ${s.slice(7)}`;
  if (s.startsWith("check:")) return `交叉校验 · ${s.slice(6)}`;
  return s;
}

/** Bug 自动处理的原因码（gate_core::errors::AutoReason::code）→ 页面上显示的原因 */
const AUTO_REASON: Record<string, string> = {
  network: "网络断开，不算 Bug",
  upstream: "上游故障，不算 Bug",
  foreign_script: "非本站脚本",
  stale_warn: "24 小时未再出现",
  stale_error: "7 天未再出现",
};
export function autoReason(code: string): string {
  return AUTO_REASON[code] ?? "自动处理";
}
