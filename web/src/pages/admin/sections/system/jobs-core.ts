/** 定时任务：接口类型、状态判定、计数（纯函数，可单测）。数据来自 GET /admin/api/jobs，按后端登记顺序。 */

/** 上次运行的结果；ok 为 false 时 summary 就是错误文字 */
export type JobLast = { at: number; ok: boolean; ms: number; summary: string };
export type Job = {
  name: string;
  title: string;
  /** 「每 10 分钟」这类周期说明 */
  every: string;
  every_secs: number;
  /** 还没跑过时为 null */
  last: JobLast | null;
};
export type JobsResponse = { jobs: Job[] };

/** 后端登记的定时任务 name → 标题（与接口给的 title 一致；Bug 来源 `job:<name>` 也用它翻译） */
export const JOB_TITLES: Record<string, string> = {
  quota: "动态额度",
  upstream_snapshot: "上游 V5 快照",
  hourly_cap: "每小时上限自动调整",
  breaker: "全站熔断",
  key_guard: "单 Key 限流",
  maintenance: "维护",
  idle_recycle: "闲置回收",
};

/** 距上次运行超过这么多个周期就算「超时」（正好这么多个周期不算） */
export const STALE_PERIODS = 3;

export type JobState = "ok" | "error" | "stale" | "never";

export const JOB_STATE_TEXT: Record<JobState, string> = {
  ok: "正常",
  error: "出错",
  stale: `超过 ${STALE_PERIODS} 个周期没跑`,
  never: "还没跑过",
};

/** 还没跑过 → never；上次出错 → error（优先于超时）；距上次超过 3 个周期 → stale；否则 ok。now 为 Unix 秒。 */
export function jobState(job: Job, now: number): JobState {
  const last = job.last;
  if (!last) return "never";
  if (last.ok === false) return "error";
  if (job.every_secs > 0 && now - last.at > STALE_PERIODS * job.every_secs) return "stale";
  return "ok";
}

/** 要站长看一眼的任务（出错 + 超时），按登记顺序 */
export function alertJobs(resp: JobsResponse | null | undefined, now: number): Job[] {
  return (resp?.jobs ?? []).filter((j) => {
    const s = jobState(j, now);
    return s === "error" || s === "stale";
  });
}

/** 要站长看一眼的任务数（出错 + 超时）：侧栏「系统」角标和总览提醒共用 */
export function jobAlertCount(resp: JobsResponse | null | undefined, now: number): number {
  return alertJobs(resp, now).length;
}
