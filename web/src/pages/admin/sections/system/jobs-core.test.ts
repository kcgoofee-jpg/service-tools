import { describe, expect, it } from "vitest";
import { alertJobs, JOB_STATE_TEXT, type Job, type JobLast, jobAlertCount, jobState } from "./jobs-core";

const NOW = 1_800_000_000;

const job = (last: Partial<JobLast> | null, every_secs = 600): Job => ({
  name: "quota",
  title: "动态额度",
  every: "每 10 分钟",
  every_secs,
  last: last && { at: NOW - 60, ok: true, ms: 42, summary: "每人 V4.5 95 张", ...last },
});

describe("jobState", () => {
  it("四种状态：还没跑过 / 出错 / 超时 / 正常", () => {
    expect(jobState(job(null), NOW)).toBe("never");
    expect(jobState(job({ ok: false, summary: "数据库锁超时" }), NOW)).toBe("error");
    expect(jobState(job({ at: NOW - 3 * 600 - 1 }), NOW)).toBe("stale");
    expect(jobState(job({}), NOW)).toBe("ok");
  });

  it("正好 3 个周期不算超时，多 1 秒才算", () => {
    expect(jobState(job({ at: NOW - 3 * 600 }), NOW)).toBe("ok");
    expect(jobState(job({ at: NOW - 3 * 600 - 1 }), NOW)).toBe("stale");
    // 周期按各自的 every_secs 算：闲置回收每小时一次，3 小时内都正常
    expect(jobState(job({ at: NOW - 3 * 3600 }, 3600), NOW)).toBe("ok");
    expect(jobState(job({ at: NOW - 3 * 3600 - 0.5 }, 3600), NOW)).toBe("stale");
  });

  it("出错优先于超时", () => {
    expect(jobState(job({ ok: false, at: NOW - 86400 }), NOW)).toBe("error");
  });

  it("徽标文案", () => {
    expect(JOB_STATE_TEXT).toEqual({
      ok: "正常",
      error: "出错",
      stale: "超过 3 个周期没跑",
      never: "还没跑过",
    });
  });
});

describe("jobAlertCount（出错 + 超时）", () => {
  it("只数出错和超时，还没跑过和正常的不算", () => {
    const jobs = [
      job(null),
      job({}),
      job({ ok: false }),
      job({ at: NOW - 3 * 600 - 1 }),
      job({ ok: false, at: NOW - 86400 }),
      job({ at: NOW - 3 * 600 }),
    ];
    expect(jobAlertCount({ jobs }, NOW)).toBe(3);
    expect(alertJobs({ jobs }, NOW)).toEqual([jobs[2], jobs[3], jobs[4]]);
  });
  it("没有数据时为 0", () => {
    expect(jobAlertCount(null, NOW)).toBe(0);
    expect(jobAlertCount(undefined, NOW)).toBe(0);
    expect(jobAlertCount({ jobs: [] }, NOW)).toBe(0);
  });
});
