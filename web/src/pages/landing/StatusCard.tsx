import { useEffect, useRef, useState } from "preact/hooks";
import { Card, cx, Skeleton } from "../../ui";
import { useStore } from "./ctx";
import { createEstimator, dailyUsage, deriveStatus } from "./status";
import { UsageMeter } from "./Usage";

/**
 * 全站算法说明按「。」「 · 」拆成行，括号补充单独成块：窄屏只在这些位置换行，不会把「重置」「10/天」拆开。
 * 每个块在样式里是 inline-block + nowrap。
 */
export function noteLines(note: string): string[][] {
  return note
    .split(/[。\n]/)
    .flatMap((sentence) => sentence.split(" · "))
    .map((seg) => seg.trim())
    .filter(Boolean)
    .map((seg) => seg.split(/(?=（)/).filter(Boolean));
}

/** 全站实时状态：一句话（能不能马上出图 + 原因）+ 全站今日用量。所有访客都看得到。 */
export function StatusCard() {
  const { live, me, meta, connection } = useStore();
  const estimator = useRef(createEstimator());
  // 排队倒计时每秒走一格（live 没有变化时也要动）
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => {
      if (!document.hidden) setNow(Date.now());
    }, 1000);
    return () => clearInterval(t);
  }, []);

  const data = live.value;
  const who = me.value;
  const timing = who?.logged_in ? who.timing : null;
  const view = deriveStatus(data, {
    estimate: estimator.current,
    nowMs: now,
    avgDurS: timing?.avg_dur_s ?? null,
  });
  const usage = dailyUsage(data);
  const algo = (meta.value.algo_notice ?? "").trim();
  // 登录且已有 Key：个人额度卡已经说清楚了，这句全站算法说明会和自己的数字打架
  const hasKey = !!(who?.logged_in && who.has_key);

  // 读屏只在「状态本身」变化时播报（不含每秒变化的倒计时）
  const announce = view?.announce ?? "";
  const [spoken, setSpoken] = useState("");
  useEffect(() => {
    setSpoken(announce);
  }, [announce]);

  return (
    <Card title="当前状态" class="lp-status">
      {view ? (
        <div class="lp-status__line">
          <span class={cx("dot", "dot--pulse", "lp-dot", `lp-dot--${view.tone}`)} aria-hidden="true" />
          <p class="lp-status__text">
            <strong class="lp-status__head">{view.headline}</strong>
            {view.detail && <span class="lp-status__detail">{view.detail}</span>}
          </p>
        </div>
      ) : (
        <div class="lp-status__line" aria-busy="true">
          <Skeleton h="28px" />
        </div>
      )}
      <p class="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {spoken}
      </p>
      {connection.value === "down" && (
        <p class="lp-note">
          {data ? "实时数据暂时读不到，显示的是最近一次的数据。" : "暂时读不到实时状态，正在重试…"}
        </p>
      )}
      <div class="lp-meters">
        <UsageMeter label="全站今日已用" used={usage.total.used} cap={usage.total.cap} />
        <UsageMeter label="V5 已用" used={usage.v5.used} cap={usage.v5.cap} base="v5" />
      </div>
      {algo && !hasKey && (
        <div class="lp-note lp-note--algo">
          {noteLines(algo).map((parts) => (
            <p key={parts.join("")}>
              {parts.map((s) => (
                <span key={s}>{s}</span>
              ))}
            </p>
          ))}
        </div>
      )}
    </Card>
  );
}
