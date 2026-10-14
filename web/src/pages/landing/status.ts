/**
 * 首页「现在能不能出图」的推导（纯函数，移植自旧版 landing.html 的 render()）。
 *
 * 站长规矩：界面和接口说同一句话。网关拒绝出图时返回的 429 原因（guard.py 的 Reason 文案）
 * 同时出现在 /public/live 的 guard.blocked.text 里；这里的措辞、数字都从那句话和 live 的字段里取，
 * 不自己编（见 status.test.ts：把 guard.py 的原文贴进去，核对两边的数字和关键词一致）。
 */
import { duration } from "../../lib/format";
import type { Live } from "./types";

export type Tone = "ok" | "busy" | "mine" | "warn" | "bad" | "paced";

export type StatusView = {
  tone: Tone;
  /** 一句话里的主干（粗体） */
  headline: string;
  /** 补充原因（灰字），可为空 */
  detail: string;
  /** headline · detail：测试与读屏共用的完整一句话 */
  sentence: string;
  /** 读屏播报用：不含每秒变化的倒计时，避免状态没变也一直念 */
  announce: string;
};

const view = (tone: Tone, headline: string, detail = "", announce?: string): StatusView => {
  const sentence = detail ? `${headline} · ${detail}` : headline;
  return { tone, headline, detail, sentence, announce: announce ?? sentence };
};

/** 排队等待估算：队伍人数变了就重新按「人数 × 放行间隔」估一次，之后每秒倒数（最少 2 秒，不会越数越小到 0）。 */
export type Estimator = (count: number, intervalSec: number, nowMs: number) => number;
export function createEstimator(): Estimator {
  let lastCount = -1;
  let lastSec = 0;
  let seenAt = 0;
  return (count, intervalSec, nowMs) => {
    if (count <= 0) {
      lastCount = 0;
      return 0;
    }
    if (count !== lastCount) {
      lastCount = count;
      lastSec = Math.round(count * intervalSec);
      seenAt = nowMs;
      return lastSec;
    }
    const passed = Math.floor((nowMs - seenAt) / 1000);
    return Math.max(2, lastSec - passed);
  };
}

const num = (v: unknown): number => {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
};
const firstInt = (text: string, re: RegExp): number | null => {
  const m = re.exec(text);
  return m?.[1] ? Number(m[1]) : null;
};

/** 全站被站点规则拦下（guard.blocked）时的说法；text 就是成员收到的 429 原因。 */
function blockedView(code: string, text: string, live: Live): StatusView {
  const wait = firstInt(text, /约 (\d+) 分钟/);
  switch (code) {
    case "paced":
      return view(
        "paced",
        "本时段份额已用完",
        wait ? `全站出图按时段放行，约 ${wait} 分钟后有空位` : "全站出图按时段放行，稍后有空位",
      );
    case "daily_cap": {
      const h = firstInt(text, /明天\s*(\d+)\s*点/) ?? 7;
      return view("warn", "今日出图已满", `明天 ${h} 点恢复`);
    }
    case "quiet_cap": {
      if (/暂停出图/.test(text)) {
        // 恢复整点以 429 原文为准，原文里没有才看规则字段
        const end = firstInt(text, /(\d+)\s*点恢复/) ?? live.rules?.quiet_end ?? 7;
        return view("warn", `安静时段 · 暂停出图 · ${end} 点恢复`);
      }
      // 安静时段里没有整体暂停，只是放慢：「出图放慢到每小时 N 张，约 M 分钟后有空位」
      const cap = firstInt(text, /每小时\s*(\d+)\s*张/);
      const parts = [cap ? `每小时 ${cap} 张` : "", wait ? `约 ${wait} 分钟后有空位` : ""].filter(Boolean);
      return view("warn", "安静时段 · 出图放慢", parts.join("，"));
    }
    case "hourly_cap": {
      const cap = firstInt(text, /每小时\s*(\d+)\s*张/) ?? live.rules?.hourly_cap ?? 0;
      const parts = [
        "滚动窗口",
        cap ? `全站每小时 ${cap} 张` : "",
        "不是整点重置",
        wait ? `约 ${wait} 分钟后逐张恢复` : "额度随时间滑动恢复",
      ].filter(Boolean);
      return view("warn", "最近 60 分钟已到上限", parts.join("，"));
    }
    case "breaker":
      return view("bad", text || "上游连续出错，已暂停出图");
    default:
      // 不认识的原因码：原文照搬，宁可直白也不改写
      return view("warn", text || "暂时无法出图");
  }
}

export type DeriveOpts = {
  estimate?: Estimator;
  nowMs?: number;
  /** 成员自己的平均出图耗时（秒），用来补「你的图正在生成」 */
  avgDurS?: number | null;
};

/** live 还没到时返回 null（页面显示骨架屏）。判断顺序与旧版一致：上游冷却 → 全站拦截 → 排队满 → 突发限速 → 小时上限 → 排队 → 空闲；自己的图优先显示。 */
export function deriveStatus(live: Live | null, opts: DeriveOpts = {}): StatusView | null {
  if (!live) return null;
  const nowMs = opts.nowMs ?? Date.now();
  const estimate = opts.estimate ?? createEstimator();
  const g = live.guard ?? {};
  const r = live.rules ?? {};
  const mineView = live.me ?? null;
  const q = mineView ?? live.queue ?? {};
  const waiting = num(q.waiting);
  const running = num(q.running);
  const mine = mineView?.mine ?? [];
  const myRunning = mine.some((x) => x.state === "running");
  const myWaiting = mine.find((x): x is Extract<typeof x, { state: "waiting" }> => x.state === "waiting");
  const interval = num(live.interval) > 0 ? num(live.interval) : 0;
  const hour = num(g.hour);
  const hourCap = num(g.hour_cap);
  const queueCap = num(r.queue_cap);
  const blocked = g.blocked ?? null;

  let out: StatusView;
  if (num(g.cooldown) > 0) {
    // 与 main.check_image_cooldown 的 429 同一句：上游图片服务限流保护中，所有图片生成暂停约 N 秒
    out = view("bad", "上游图片服务限流保护中", `所有图片生成暂停约 ${num(g.cooldown)} 秒`);
  } else if (blocked && (blocked.code || blocked.text)) {
    out = blockedView(blocked.code ?? "", (blocked.text ?? "").trim(), live);
  } else if (queueCap > 0 && waiting >= queueCap) {
    out = view("warn", "排队已满", `全站最多同时排 ${queueCap} 张，请稍后再试`);
  } else if (num(g.burst_wait) > 0) {
    // 10 分钟分摊到点前必须说「限速中」，不能承诺「即发即画」：否则成员照着面板发请求拿到的是 429
    out = view(
      "warn",
      "本时段已到限速",
      `每 10 分钟 ${num(g.burst_cap) || "—"} 张，用来抹平突发、保护上游账号，约 ${num(g.burst_wait)} 分钟后可继续`,
    );
  } else if (hourCap > 0 && hour >= hourCap) {
    out = view(
      "warn",
      "最近 60 分钟已到上限",
      `滚动窗口，全站每小时 ${hourCap} 张，不是整点重置，额度随时间滑动恢复`,
    );
  } else if (waiting + running > 0) {
    const ahead = waiting + running;
    if (interval > 0) {
      const sec = estimate(ahead, interval, nowMs);
      out = view(
        "busy",
        `约 ${duration(sec)}后开始`,
        `前面有 ${ahead} 张在等`,
        `排队中 · 前面有 ${ahead} 张在等`,
      );
    } else {
      out = view("busy", "排队中", `前面有 ${ahead} 张在等`);
    }
  } else {
    estimate(0, interval, nowMs);
    out = view("ok", "马上开始", "通道空闲");
  }

  if (myRunning) {
    const avg = opts.avgDurS && opts.avgDurS > 0 ? `平均约 ${opts.avgDurS} 秒` : "";
    out = view("mine", "你的图正在生成", avg);
  } else if (myWaiting) {
    const eta = myWaiting.eta != null ? Math.max(2, num(myWaiting.eta)) : 0;
    out = view(
      "mine",
      `你的图排第 ${myWaiting.position} 位`,
      eta ? `约 ${duration(eta)}后开始画` : "",
      `你的图排第 ${myWaiting.position} 位`,
    );
  }
  return out;
}

export type Usage = { used: number; cap: number };

/** 全站今日用量：0 的上限 = 站长关掉了这个上限（显示「不限」），不要写死历史数字。 */
export function dailyUsage(live: Live | null): { total: Usage; v5: Usage } {
  const q = live?.quota ?? {};
  const used = q.units_today ?? q.images_today ?? 0;
  return {
    total: { used: Math.round(num(used)), cap: num(q.images_cap) },
    v5: { used: num(q.v5_today), cap: num(q.v5_cap) },
  };
}
