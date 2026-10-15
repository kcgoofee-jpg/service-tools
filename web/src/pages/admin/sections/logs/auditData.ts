/** 生成记录的纯函数：解析 extra、参数摘要、画廊缩略图比例、提示词复制块。 */
import type { AuditExtra, AuditItem } from "./types";

export function parseExtra(raw: string | null | undefined): AuditExtra {
  if (!raw) return {};
  try {
    const v = JSON.parse(raw) as unknown;
    return v && typeof v === "object" ? (v as AuditExtra) : {};
  } catch {
    return {};
  }
}

export const hasPicture = (it: Pick<AuditItem, "has_image" | "has_thumb">): boolean =>
  !!(it.has_image || it.has_thumb);

/** 宽 / 高，缺失时按竖图 2:3 */
export function aspectOf(ex: AuditExtra): number {
  const w = Number(ex.params?.width);
  const h = Number(ex.params?.height);
  return w > 0 && h > 0 ? w / h : 2 / 3;
}

/** 「种子 123 · Euler · 28 步 · CFG 5 · 832×1216」 */
export function paramsLine(ex: AuditExtra): string {
  const p = ex.params ?? {};
  const size = p.width && p.height ? `${p.width}×${p.height}` : "";
  return [
    p.seed != null ? `种子 ${p.seed}` : "",
    p.sampler ?? "",
    p.steps ? `${p.steps} 步` : "",
    p.scale != null ? `CFG ${p.scale}` : "",
    size,
  ]
    .filter(Boolean)
    .join(" · ");
}

/** 角色提示词合并成一段，供「复制角色」 */
export const charsText = (list: string[] | undefined): string =>
  (list ?? []).map((c, i) => `角色${i + 1}：${c}`).join("\n");
export const negativeText = (it: Pick<AuditItem, "negative">, ex: AuditExtra): string =>
  [it.negative, ...(ex.char_negatives ?? []).map((c, i) => `角色${i + 1}负面：${c}`)]
    .filter(Boolean)
    .join("\n");

export const STATUS_TEXT: Record<string, string> = { ok: "成功", rejected: "拒绝", error: "错误" };
export const statusText = (s: string | null | undefined): string => STATUS_TEXT[s ?? ""] ?? (s || "错误");
