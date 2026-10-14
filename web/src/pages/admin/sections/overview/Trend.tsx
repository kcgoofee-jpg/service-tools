/** 近 7 天走势：一个指标一组柱子，按钮切换指标；最右一根是今天。 */
import { useState } from "preact/hooks";
import { Card, cx } from "../../../../ui";
import { Seg } from "../members/shared";
import type { Overview } from "./types";

type Metric = "images" | "v5" | "requests" | "anlas" | "text_tokens";

const METRICS: Array<{ id: Metric; label: string; title: string; hint: string }> = [
  { id: "images", label: "图片", title: "生成图片张数", hint: "含站长 / 测试 Key；每日上限在「额度与保护」" },
  { id: "v5", label: "V5", title: "V5 张数", hint: "只算占全站 V5 额度的 Key" },
  { id: "requests", label: "请求", title: "请求数", hint: "除查额度外的所有调用" },
  { id: "anlas", label: "Anlas", title: "Anlas 消耗（估算）", hint: "估算值，以官方余额为准" },
  { id: "text_tokens", label: "文本", title: "文本 tokens", hint: "文本 / 聊天功能" },
];

const fmt = (metric: Metric, v: number): string => {
  if (metric === "anlas") return String(Math.round(v));
  if (metric === "text_tokens" && v >= 10000) return `${(v / 10000).toFixed(1)} 万`;
  return String(v);
};

export function barHeights(values: number[]): number[] {
  const max = Math.max(1, ...values);
  return values.map((v) => (v ? Math.max(2, Math.round((v / max) * 100)) : 0));
}

export function Trend(props: { week: Overview["week"] }) {
  const [metric, setMetric] = useState<Metric>("images");
  const m = METRICS.find((x) => x.id === metric) ?? (METRICS[0] as (typeof METRICS)[number]);
  const values = props.week.map((w) => Number(w[metric]) || 0);
  const heights = barHeights(values);
  const label = props.week.map((w, i) => `${w.day.slice(5)} ${fmt(metric, values[i] ?? 0)}`).join("，");
  return (
    <Card title="近 7 天" sub={m.hint} class="ov-card">
      <div class="xa-toolbar">
        <Seg<Metric>
          label="指标"
          value={metric}
          options={METRICS.map((x) => ({ id: x.id, label: x.label, title: x.title }))}
          onChange={setMetric}
        />
      </div>
      <div role="img" aria-label={`${m.title}：${label}`}>
        <ol class="ov-trend">
          {props.week.map((w, i) => (
            <li key={w.day} class={cx("ov-trend__col", i === props.week.length - 1 && "is-today")}>
              <span class="ov-trend__v tabular">{fmt(metric, values[i] ?? 0)}</span>
              <span class="ov-trend__track">
                <i style={{ height: `${heights[i] ?? 0}%`, minHeight: values[i] ? "3px" : "0" }} />
              </span>
              <span class="ov-trend__d small muted">
                {i === props.week.length - 1 ? "今天" : w.day.slice(5)}
              </span>
            </li>
          ))}
        </ol>
      </div>
    </Card>
  );
}
