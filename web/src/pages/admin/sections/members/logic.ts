/**
 * 成员判断逻辑（纯函数，无界面依赖，方便单测；总览的「成员要处理」提醒也用这里的 needsAttention）。
 * 时间单位都是 Unix 秒。
 */
import { fullTime, rel } from "../../../../lib/format";
import type { AutoTag, Member, Role, Tag } from "./types";

export const HOUR = 3600;
export const DAY = 86400;
/** 站长点「没问题」后，24 小时内这些提醒不再进「需要处理」 */
export const ACK_TAG = "已查看";

export const userTags = (m: Pick<Member, "tags">): Tag[] => m.tags.filter((t) => t.tag !== ACK_TAG);
export const acked = (m: Pick<Member, "tags">, now: number): boolean =>
  m.tags.some((t) => t.tag === ACK_TAG && now - (t.ts || 0) < DAY);

export const memberLabel = (m: Pick<Member, "discord" | "name">): string =>
  m.discord?.display_name || m.discord?.username || m.name;

export function roleOf(m: Pick<Member, "role" | "is_admin" | "is_test" | "tags">): Role {
  if (m.role) return m.role;
  if (m.is_admin) return "admin";
  if (m.is_test) return "test";
  return m.tags.some((t) => t.tag === "邀请") ? "invite" : "regular";
}

export const isStaff = (m: Pick<Member, "is_admin" | "is_test">): boolean => m.is_admin || m.is_test;
/** 后台手动建的 Key：没有 Discord 绑定的普通成员 */
export const isManual = (m: Pick<Member, "is_admin" | "is_test" | "discord" | "discord_id">): boolean =>
  !m.is_admin && !m.is_test && !m.discord && !m.discord_id;

/** 时长文字：「5 分钟 / 3 小时 / 2 天」 */
export function ago(sec: number): string {
  if (sec < HOUR) return `${Math.max(1, Math.round(sec / 60))} 分钟`;
  if (sec < DAY) return `${Math.round(sec / HOUR)} 小时`;
  return `${Math.round(sec / DAY)} 天`;
}

/* ---------------- 待处理事项 ---------------- */
export type IssueAction =
  | { kind: "share-clear" }
  | { kind: "edit" }
  | { kind: "ack" }
  | { kind: "tag-del"; tag: string };
export type Issue = {
  id: string;
  level: "bad" | "warn" | "info";
  title: string;
  detail: string;
  /** true = 需要站长看一眼（可以点「没问题」24 小时内不再提醒） */
  need?: boolean;
  actions: IssueAction[];
};

export function memberIssues(m: Member, now: number): Issue[] {
  const out: Issue[] = [];
  if (m.share && m.share.paused_until > now) {
    out.push({
      id: "share-paused",
      level: "bad",
      title: "Key 已被限流暂停",
      detail: `到 ${fullTime(m.share.paused_until)} 自动恢复（多半是客户端死循环重试触发）`,
      actions: [{ kind: "share-clear" }],
    });
  }
  if (m.expires_at && m.expires_at < now) {
    out.push({
      id: "expired",
      level: "bad",
      title: "Key 已过期",
      detail: "成员现在用不了",
      actions: [{ kind: "edit" }],
    });
  } else if (m.expires_at && m.expires_at - now < 3 * DAY) {
    out.push({
      id: "expiring",
      level: "warn",
      title: `${Math.max(1, Math.ceil((m.expires_at - now) / DAY))} 天后到期`,
      detail: fullTime(m.expires_at),
      need: true,
      actions: [{ kind: "edit" }, { kind: "ack" }],
    });
  }
  // 自动标签：算法标记「只观察」或单纯客户端短时连点「请求过快」不升级为要处理
  for (const t of m.auto_tags as AutoTag[]) {
    const observe = t.tag === "疑似同一人" || !!t.note?.includes("只观察");
    const rateLimit = t.tag === "请求过快";
    const loud = !observe && !rateLimit && (!!t.note || t.count >= 50);
    out.push({
      id: `auto:${t.tag}`,
      level: loud ? "warn" : "info",
      title: t.tag,
      detail:
        t.note ||
        `近 24 小时因此被拒 ${t.count} 次。是客户端设置的问题，本人改了就好${loud ? "；次数多，可能是客户端在自动重试" : ""}`,
      need: loud,
      actions: loud ? [{ kind: "ack" }] : [],
    });
  }
  for (const t of userTags(m).filter((x) => /疑似|待查|观察/.test(x.tag))) {
    out.push({
      id: `tag:${t.tag}`,
      level: "warn",
      title: `站长标记：${t.tag}`,
      detail: `${t.note || ""}${t.ts ? ` · ${rel(t.ts, now)}` : ""}`,
      need: true,
      actions: [{ kind: "tag-del", tag: t.tag }],
    });
  }
  if (m.reset_today && m.reset_today.count >= 2) {
    out.push({
      id: "reset-many",
      level: "warn",
      title: `今天被手动重置额度 ${m.reset_today.count} 次`,
      detail: "操作日志里能看到是谁重置的",
      need: true,
      actions: [{ kind: "ack" }],
    });
  }
  // 点过「没问题」的 24 小时内，这些可确认的提醒不再出现（bad 级别要单独处理，不受影响）
  return acked(m, now) ? out.filter((x) => !x.need || x.level === "bad") : out;
}

export const needsAttention = (m: Member, now: number): boolean =>
  m.enabled &&
  !m.is_admin &&
  memberIssues(m, now).some((x) => x.level === "bad" || (x.need && !acked(m, now)));

/* ---------------- 一句话结论与列表状态 ---------------- */
export function memberVerdict(m: Member, now: number): { title: string; sub: string } {
  const issues = memberIssues(m, now);
  const cap = m.daily_images;
  const used = m.today.images;
  let title: string;
  const bad = issues.find((x) => x.level === "bad");
  if (!m.enabled) title = "已停用，Key 现在不能用";
  else if (bad) title = bad.title;
  else if (issues.some((x) => x.need) && !acked(m, now)) {
    title = `有 ${issues.filter((x) => x.need).length} 件事要看一眼`;
  } else if (!m.first_image_at && !m.is_admin) {
    title = `领了 Key ${m.registered_at ? `${ago(now - m.registered_at)}了` : ""}，还没出过图`;
  } else if (cap > 0 && used >= cap) title = `今天的额度用完了（${used} 张）`;
  else if (used > 0) title = `正常使用，今天出了 ${used} 张`;
  else title = `今天还没用${m.last_used_at ? `，上次是 ${rel(m.last_used_at, now)}` : ""}`;
  const why = (m.rejected_why || []).map(([r, n]) => `${r} ${n}`).join("、");
  const ack = m.tags.find((t) => t.tag === ACK_TAG);
  const sub = [
    m.rejected_24h ? `24 小时被拒 ${m.rejected_24h} 次${why ? `：${why}` : ""}` : "",
    m.reset_today?.count ? `站长今天重置过 ${m.reset_today.count} 次额度` : "",
    acked(m, now) && ack ? `${rel(ack.ts, now)}点过「没问题」` : "",
  ]
    .filter(Boolean)
    .join(" · ");
  return { title, sub };
}

export type StatusBadge = {
  tone: "danger" | "warn" | "info" | "accent" | undefined;
  text: string;
  title?: string;
};
/** 列表里每行只放一个最重要的状态 */
export function memberStatus(m: Member, now: number): StatusBadge | null {
  if (!m.enabled) return { tone: "danger", text: "已停用" };
  if (m.share && m.share.paused_until > now) return { tone: "danger", text: "已限流" };
  if (m.expires_at && m.expires_at < now) return { tone: "danger", text: "已过期" };
  if (needsAttention(m, now)) {
    return {
      tone: "warn",
      text: "要处理",
      title: memberIssues(m, now)
        .map((x) => x.title)
        .join("、"),
    };
  }
  if (m.is_admin) return { tone: undefined, text: "站长" };
  if (m.is_test) return { tone: undefined, text: "测试" };
  if (m.daily_images > 0 && m.today.images >= m.daily_images) return { tone: undefined, text: "额度用完" };
  if (m.registered_at && now - m.registered_at < DAY) return { tone: "info", text: "新加入" };
  if (!m.first_image_at) return { tone: undefined, text: "还没出过图" };
  return null;
}

/* ---------------- 筛选 / 搜索 / 排序 ---------------- */
export type FilterDef = { id: string; label: string; test: (m: Member) => boolean; allKeys?: boolean };
export type FilterSet = {
  main: FilterDef[];
  more: FilterDef[];
  userTags: FilterDef[];
  autoTags: FilterDef[];
};

const ALT_TAGS = ["疑似小号", "疑似同一人"];
const collator = new Intl.Collator(["zh-Hans-CN", "en"], { numeric: true, sensitivity: "base" });
export const compareText = (a: string, b: string): number => collator.compare(a, b);

/** idleDays：后端「满 N 天未生图」回收的天数；0 = 回收关闭，不出现「快被回收」筛选 */
export function buildFilters(everyone: Member[], now: number, idleDays: number): FilterSet {
  const main: FilterDef[] = [
    { id: "attention", label: "需要处理", test: (m) => needsAttention(m, now) },
    { id: "active", label: "启用中", test: (m) => m.enabled },
    { id: "today", label: "今日活跃", test: (m) => m.today.requests > 0 },
    { id: "never", label: "还没出过图", test: (m) => !m.first_image_at && !m.is_admin },
    { id: "disabled", label: "已停用", test: (m) => !m.enabled },
    { id: "all", label: "全部成员", test: () => true },
  ];
  const more: FilterDef[] = [
    { id: "new", label: "24 小时内新加入", test: (m) => !!m.registered_at && now - m.registered_at < DAY },
  ];
  if (idleDays > 0) {
    more.push({
      id: "reclaim",
      label: `快被回收（满 ${idleDays} 天未生图）`,
      // 与后端回收口径一致：只看最近一次成功生图（没生过图的从创建时间算），站长 / 测试 / 邀请 Key 豁免
      test: (m) =>
        roleOf(m) === "regular" &&
        m.enabled &&
        now - (m.last_image_at ?? m.created_at) > (idleDays - 1) * DAY,
    });
  }
  more.push(
    { id: "rejected", label: "24 小时被拒过", test: (m) => m.rejected_24h > 0 },
    { id: "manual", label: "后台手动建的 Key", test: isManual },
    { id: "staff", label: "站长 / 测试 Key", test: isStaff, allKeys: true },
    { id: "role:invite", label: "身份组：邀请", test: (m) => roleOf(m) === "invite", allKeys: true },
    { id: "role:regular", label: "身份组：普通", test: (m) => roleOf(m) === "regular" },
    { id: "role:test", label: "身份组：测试", test: (m) => roleOf(m) === "test", allKeys: true },
    { id: "role:admin", label: "身份组：站长", test: (m) => roleOf(m) === "admin", allKeys: true },
    {
      id: "alt",
      label: "疑似小号 / 同一人",
      test: (m) =>
        userTags(m).some((x) => ALT_TAGS.includes(x.tag)) ||
        m.auto_tags.some((x) => ALT_TAGS.includes(x.tag)),
      allKeys: true,
    },
  );
  const builtin = new Set(
    [...main, ...more].map((f) => f.label.replace(/\s*[（(].*$/, "")).concat(["站长", "邀请", ...ALT_TAGS]),
  );
  const userSet = [...new Set(everyone.flatMap((m) => userTags(m).map((t) => t.tag)))]
    .filter((t) => !builtin.has(t))
    .sort(compareText);
  const autoSet = [...new Set(everyone.flatMap((m) => m.auto_tags.map((t) => t.tag)))]
    .filter((t) => !userSet.includes(t) && !builtin.has(t))
    .sort(compareText);
  return {
    main,
    more,
    userTags: userSet.map((t) => ({
      id: `tag:${t}`,
      label: t,
      test: (m: Member) => userTags(m).some((x) => x.tag === t),
      allKeys: true,
    })),
    autoTags: autoSet.map((t) => ({
      id: `auto:${t}`,
      label: t,
      test: (m: Member) => m.auto_tags.some((x) => x.tag === t),
      allKeys: true,
    })),
  };
}

export const allFilters = (s: FilterSet): FilterDef[] => [...s.main, ...s.more, ...s.userTags, ...s.autoTags];

/** 默认筛选：有要处理的人就直接看「需要处理」，否则「启用中」 */
export function defaultFilter(everyone: Member[], now: number): string {
  const members = everyone.filter((m) => !isStaff(m));
  return members.some((m) => needsAttention(m, now)) ? "attention" : "active";
}

export function matchesQuery(m: Member, raw: string): boolean {
  const q = raw.trim().toLowerCase();
  if (!q) return true;
  return (
    m.name.toLowerCase().includes(q) ||
    String(m.discord_id || "").includes(q) ||
    [m.discord?.username, m.discord?.display_name].some((v) =>
      String(v || "")
        .toLowerCase()
        .includes(q),
    ) ||
    m.tags.some((t) => `${t.tag} ${t.note || ""}`.toLowerCase().includes(q))
  );
}

export type SortKey = "today" | "week" | "total" | "recent" | "created" | "name";
export const SORT_LABEL: Record<SortKey, string> = {
  today: "今日用量",
  week: "近 7 天",
  total: "累计",
  recent: "最近使用",
  created: "加入时间",
  name: "名称",
};

const sortValue: Record<Exclude<SortKey, "name">, (m: Member) => number> = {
  today: (m) => m.today.images,
  week: (m) => m.week.images,
  total: (m) => m.total_images,
  recent: (m) => m.last_used_at || 0,
  created: (m) => m.registered_at || 0,
};

const titleOf = (m: Member): string => memberLabel(m) || "";
export function sortMembers<T extends Member>(list: T[], key: SortKey, dir: "asc" | "desc"): T[] {
  const sign = dir === "asc" ? 1 : -1;
  return list.slice().sort((a, b) => {
    if (key === "name") return sign * compareText(titleOf(a), titleOf(b));
    const f = sortValue[key];
    return sign * (f(a) - f(b)) || compareText(titleOf(a), titleOf(b));
  });
}
/** 数字 / 时间列默认大的在前，文字列默认正序 */
export const defaultDir = (key: SortKey): "asc" | "desc" => (key === "name" ? "asc" : "desc");

/** 在成员列表里找到某个 Key（或合并后的任一把 Key）对应的成员 */
export const findByKeyId = <T extends Member>(list: T[], id: number): T | undefined =>
  list.find((m) => m.id === id || (m.all_key_ids || []).includes(id));
