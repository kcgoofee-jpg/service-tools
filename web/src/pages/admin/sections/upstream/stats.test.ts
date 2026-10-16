import { describe, expect, it } from "vitest";
import {
  type AnlasEstimate,
  ageText,
  daysLeftText,
  projectionText,
  requestsText,
  type WindowStats,
  waitText,
} from "./stats";

const w = (over: Partial<WindowStats>): WindowStats => ({
  ok: 0,
  error: 0,
  rejected: 0,
  avg_wait_ms: null,
  p90_wait_ms: null,
  up_429: 0,
  up_403: 0,
  up_401: 0,
  ...over,
});
const est = (over: Partial<AnlasEstimate>): AnlasEstimate => ({
  cycle_rolled: false,
  balance: 1000,
  fixed: 900,
  purchased: 100,
  expires_at: 1,
  days_left: 10.4,
  avg_daily: 50,
  projection: "unknown",
  projection_value: null,
  snapshot_age_days: 1,
  stale: false,
  approximate: false,
  unattributed_anlas: 0,
  ...over,
});

describe("账号统计文案", () => {
  it("请求与等待", () => {
    expect(requestsText(w({ ok: 3, error: 1, rejected: 2 }))).toBe("成功 3 · 出错 1 · 选号后被拒 2");
    expect(waitText(w({}))).toBe("—");
    expect(waitText(w({ avg_wait_ms: 1250, p90_wait_ms: 3400 }))).toBe("平均 1.3 秒 · p90 3.4 秒");
  });
  it("到期预测三种结果", () => {
    expect(projectionText(est({ cycle_rolled: true }))).toMatch(/重新核对/);
    expect(projectionText(est({ projection: "runs_out", projection_value: 6 }))).toMatch(/用完（约 6.0 天后/);
    expect(projectionText(est({ projection: "waste", projection_value: 500 }))).toMatch(/浪费约 500/);
    expect(projectionText(est({}))).toBe("—");
  });
  it("倒计时与核对时间", () => {
    expect(daysLeftText(est({}))).toBe("10 天");
    expect(daysLeftText(est({ days_left: 0.3 }))).toBe("不到 1 天");
    expect(ageText(0.2)).toBe("今天核对过");
    expect(ageText(4.2)).toBe("4 天前核对");
  });
});
