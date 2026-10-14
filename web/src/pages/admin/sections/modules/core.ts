/** 模块：接口类型、状态判定、排序（纯函数，可单测）。 */

export type Level = "ok" | "warn" | "bad" | "na" | "";
export type Check = { name: string; ok: boolean; detail: string; level: Level; evidence?: string[] };
export type Run = { at: number; level: Level };
export type Mod = {
  name: string;
  title: string;
  question: string;
  enabled: boolean;
  hard_note: string;
  periodic: boolean;
  last: {
    enabled?: boolean;
    at?: number;
    ok?: boolean;
    ms?: number;
    summary?: string;
    error?: string;
    checks?: Check[];
    runs?: Run[];
  };
  params: Array<{ name: string; value: unknown; note: string }>;
};
export type ModulesResponse = { modules: Mod[] };

export const LEVEL_TEXT: Record<string, string> = { ok: "正常", warn: "注意", bad: "有问题", na: "不适用" };

/** 结构性交叉校验的「站长已确认」键（按校验名对应；接口没有给出键，只能按名字） */
export const ACK_KEYS: Record<string, string> = {
  "V5：每人额度 × 人数 ≤ 全站上限": "v5_oversell",
  "每人承诺 ≤ 全站日容量": "registration_ready",
};

export type ModState = "off" | "bad" | "warn" | "ok";

export function moduleState(m: Mod): ModState {
  if (!m.enabled) return "off";
  const checks = m.last.checks ?? [];
  if (m.last.ok === false || checks.some((c) => c.level === "bad")) return "bad";
  if (checks.some((c) => c.level === "warn")) return "warn";
  return "ok";
}

/** 侧栏角标：启用中的模块里「有问题」级别的校验数（上次直接跑挂也算一个） */
export function failingChecks(mods: Mod[]): number {
  let n = 0;
  for (const m of mods) {
    if (!m.enabled) continue;
    n += (m.last.checks ?? []).filter((c) => c.level === "bad").length;
    if (m.last.ok === false) n += 1;
  }
  return n;
}

const lastAt = (m: Mod, level: Level): number =>
  Math.max(0, ...(m.last.runs ?? []).filter((r) => r.level === level).map((r) => r.at || 0));

/** 报错优先：已关闭的沉底；上次跑挂 > 校验不通过 > 最近报过错 > 注意 > 其余 */
export function rank(m: Mod): number {
  const checks = m.last.checks ?? [];
  if (!m.enabled) return 0;
  if (m.last.ok === false) return 6;
  if (checks.some((c) => c.level === "bad")) return 5;
  if (lastAt(m, "bad")) return 4;
  if (checks.some((c) => c.level === "warn") || lastAt(m, "warn")) return 3;
  return 1;
}

export function sortMods(mods: Mod[], errorFirst: boolean): Mod[] {
  const list = mods.slice();
  if (!errorFirst) return list;
  return list.sort(
    (a, b) => rank(b) - rank(a) || lastAt(b, "bad") - lastAt(a, "bad") || (a.title > b.title ? 1 : -1),
  );
}

export function runCounts(runs: Run[]): { ok: number; warn: number; bad: number } {
  return {
    ok: runs.filter((r) => r.level === "ok").length,
    warn: runs.filter((r) => r.level === "warn").length,
    bad: runs.filter((r) => r.level === "bad").length,
  };
}
