/**
 * 总览的「有 N 件事要处理」：提醒的生成、隐藏记录（本机 localStorage，按稳定 key 记 24 小时）、结论。
 * 纯函数，存储可注入，方便单测。
 *
 * 站长反馈：有些提醒没法立刻消除，所以每条可以隐藏 24 小时；隐藏按稳定 key 记（不按文案，数字变了也不会重新冒出来）；
 * 被隐藏的提醒不能让页面显示成绿色——还有事没处理就不给绿点。
 */
import { rel } from "../../../../lib/format";
import { memberLabel, needsAttention } from "../members/logic";
import type { Member } from "../members/types";
import type { BotResp, BugsResp, ModulesResp, Overview, StatusResp } from "./types";

export type Level = "bad" | "warn" | "info";
export type Reminder = {
  /** 稳定 key：隐藏记录用它，不能随文案或数字变化 */
  key: string;
  level: Level;
  title: string;
  detail: string;
  /** 「去看看」跳转的路径 */
  to: string;
  /** 按钮的无障碍名称后缀（说明去哪里） */
  label: string;
};

export type ReminderInput = {
  overview: Overview;
  status: StatusResp | null;
  bot: BotResp | null;
  bugs: BugsResp | null;
  modules: ModulesResp | null;
  members: Member[] | null;
  now: number;
};

const num = (n: number): string => n.toLocaleString("zh-CN");

export function buildReminders(inp: ReminderInput): Reminder[] {
  const { overview: d, status, bot, bugs, modules, members, now } = inp;
  const items: Reminder[] = [];
  const add = (level: Level, key: string, title: string, detail: string, to: string, label: string) =>
    items.push({ key, level, title, detail, to, label });
  const up = status?.upstream;

  if (up && up.image_cooldown_seconds > 0) {
    add(
      "bad",
      "up-cooldown",
      `上游限流，全站暂停出图 ${up.image_cooldown_seconds} 秒`,
      "会自动恢复；频繁出现要降低每小时上限",
      "/admin/upstream",
      "上游限流",
    );
  } else if (up?.status === "degraded") {
    add(
      "bad",
      "up-degraded",
      "上游不稳定",
      `最近 ${up.recent} 次请求失败 ${up.failed} 次`,
      "/admin/upstream",
      "上游不稳",
    );
  }
  for (const p of d.pool ?? []) {
    if (p.admin_enabled && p.disabled) {
      add(
        "bad",
        `up-acct-${p.position}-dead`,
        `上游账号 #${p.position} 失效（401）`,
        "Token 过期或被改了密码，需要替换",
        "/admin/upstream",
        "上游账号失效",
      );
    } else if (p.admin_enabled && p.blocked_for > 0) {
      add(
        "warn",
        `up-acct-${p.position}-blocked`,
        `上游账号 #${p.position} 限流中`,
        `约 ${p.blocked_for} 秒后恢复`,
        "/admin/upstream",
        "上游限流中",
      );
    }
  }
  if (!d.pool_configured) {
    add(
      "bad",
      "up-notconfigured",
      "还没有配置上游 Token",
      "现在所有出图请求都会失败",
      "/admin/upstream",
      "未配置上游",
    );
  }
  if (bot?.status && !bot.status.online) {
    add(
      "bad",
      "discord",
      "奶妹离线",
      bot.status.seen_at ? `最近心跳 ${rel(bot.status.seen_at, now)}` : "还没收到心跳",
      "/admin/discord",
      "机器人离线",
    );
  }
  if (bugs && bugs.open > 0) {
    const err = bugs.errors.filter((e) => !e.resolved_at && e.level === "error").length;
    add(
      err ? "bad" : "warn",
      "bugs",
      `${bugs.open} 个 Bug 没处理`,
      err ? `其中 ${err} 个是错误级别` : "都是「注意」级别，多数是客户端断开之类",
      "/admin/logs/bugs",
      "Bug 未处理",
    );
  }
  const att = (members ?? []).filter((m) => needsAttention(m, now));
  if (att.length) {
    add(
      "warn",
      "members-attention",
      `${att.length} 个成员要处理`,
      att.slice(0, 3).map(memberLabel).join("、") + (att.length > 3 ? " 等" : ""),
      "/admin/members?filter=attention",
      "成员要处理",
    );
  }
  const badMods = (modules?.modules ?? []).filter((m) => {
    const last = m.last ?? {};
    const badChecks = (last.checks ?? []).filter((c) => c.level === "bad").length;
    return m.enabled && (last.ok === false || badChecks > 0);
  }).length;
  if (badMods) {
    add(
      "warn",
      "modules",
      `${badMods} 个模块自检没通过`,
      "数据对不上或规则没生效",
      "/admin/modules",
      "模块自检",
    );
  }
  const dk = status?.disk;
  if (dk && dk.total_mb > 0) {
    const free = dk.free_mb / dk.total_mb;
    if (free < 0.2) {
      add(
        free < 0.1 ? "bad" : "warn",
        "disk",
        `磁盘只剩 ${Math.round(free * 100)}%`,
        "原图和日志会越来越多",
        "/admin/system",
        "磁盘",
      );
    }
  }
  if ((d.today.unconfirmed_anlas || 0) > 0) {
    add(
      "warn",
      "unconfirmed-anlas",
      `失败请求可能扣了 ${d.today.unconfirmed_anlas} Anlas`,
      `今天 ${d.today.unconfirmed_requests} 笔，去上游页对一下官方余额`,
      "/admin/upstream",
      "Anlas 待核对",
    );
  }
  if (d.anlas_budget > 0 && d.month.anlas >= d.anlas_budget * 0.8) {
    add(
      "warn",
      "anlas-budget",
      `本月 Anlas 用了 ${Math.round((d.month.anlas / d.anlas_budget) * 100)}%`,
      `${num(d.month.anlas)} / ${num(d.anlas_budget)}`,
      "/admin/limits",
      "Anlas 预算",
    );
  }
  return items;
}

/* ---------------- 隐藏记录 ---------------- */
export const HIDDEN_KEY = "owlOvHidden";
const HIDE_MS = 24 * 3600 * 1000;
type Store = Pick<Storage, "getItem" | "setItem" | "removeItem">;
export type HiddenMap = Record<string, number>;

const defaultStore = (): Store | null => {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
};

/** 读隐藏记录，顺便丢掉超过 24 小时的。存储不可用（隐私模式等）时当作没有。 */
export function readHidden(nowMs = Date.now(), store: Store | null = defaultStore()): HiddenMap {
  try {
    const raw = JSON.parse(store?.getItem(HIDDEN_KEY) || "{}") as HiddenMap;
    const out: HiddenMap = {};
    for (const [k, t] of Object.entries(raw)) if (typeof t === "number" && nowMs - t <= HIDE_MS) out[k] = t;
    return out;
  } catch {
    return {};
  }
}
export function hideReminder(
  key: string,
  nowMs = Date.now(),
  store: Store | null = defaultStore(),
): HiddenMap {
  const map = readHidden(nowMs, store);
  map[key] = nowMs;
  try {
    store?.setItem(HIDDEN_KEY, JSON.stringify(map));
  } catch {
    /* 存不进去就只在本次页面里生效 */
  }
  return map;
}
export function restoreReminders(store: Store | null = defaultStore()): HiddenMap {
  try {
    store?.removeItem(HIDDEN_KEY);
  } catch {
    /* 忽略 */
  }
  return {};
}

/* ---------------- 结论 ---------------- */
export type Verdict = {
  tone: "ok" | "warn" | "bad";
  headline: string;
  visible: Reminder[];
  hiddenCount: number;
};

export function computeVerdict(items: Reminder[], hidden: HiddenMap): Verdict {
  const visible = items.filter((x) => !hidden[x.key]);
  const hiddenItems = items.filter((x) => hidden[x.key]);
  const serious = visible.filter((x) => x.level !== "info").length;
  const tone: Verdict["tone"] = visible.some((x) => x.level === "bad")
    ? "bad"
    : serious
      ? "warn"
      : hiddenItems.some((x) => x.level !== "info")
        ? "warn" // 有被隐藏的提醒：不给绿点
        : "ok";
  const headline = serious ? `有 ${serious} 件事要处理` : tone === "warn" ? "提醒都已隐藏" : "一切正常";
  return { tone, headline, visible, hiddenCount: hiddenItems.length };
}
