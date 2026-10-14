/** 时间一律按北京时间显示（站点服务日在 Asia/Shanghai 的 7 点切换）。 */
const TZ = "Asia/Shanghai";

const dtf = new Intl.DateTimeFormat("zh-CN", {
  timeZone: TZ,
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});
const tf = new Intl.DateTimeFormat("zh-CN", {
  timeZone: TZ,
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});
const df = new Intl.DateTimeFormat("zh-CN", {
  timeZone: TZ,
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
});

/** Unix 秒 → 「10-14 07:00」 */
export const fullTime = (ts: number | null | undefined): string =>
  ts ? dtf.format(ts * 1000).replace(/\//g, "-") : "—";
/** Unix 秒 → 「07:00」 */
export const shortTime = (ts: number | null | undefined): string => (ts ? tf.format(ts * 1000) : "—");
/** Unix 秒 → 「2026-10-14」 */
export const dayOf = (ts: number): string => df.format(ts * 1000).replace(/\//g, "-");

/** 相对时间：「刚刚 / 3 分钟前 / 2 小时前 / 5 天前」 */
export function rel(ts: number | null | undefined, now = Date.now() / 1000): string {
  if (!ts) return "—";
  const d = Math.max(0, now - ts);
  if (d < 45) return "刚刚";
  if (d < 3600) return `${Math.round(d / 60)} 分钟前`;
  if (d < 86400) return `${Math.round(d / 3600)} 小时前`;
  return `${Math.round(d / 86400)} 天前`;
}

/** 秒数 → 「1 分 20 秒」「45 秒」 */
export function duration(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s} 秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return s % 60 ? `${m} 分 ${s % 60} 秒` : `${m} 分钟`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h} 小时 ${m % 60} 分` : `${h} 小时`;
}

export const num = (n: number | null | undefined): string => (n == null ? "—" : n.toLocaleString("zh-CN"));
