import type { ComponentChildren } from "preact";
import { Meter } from "../../ui";

type MeterTone = "v5" | "warn" | "danger" | undefined;

/** 快用满时变色：≥90% 琥珀，用满红。基础色由调用方给（V5 青色）。 */
export function toneFor(used: number, cap: number, base?: "v5"): MeterTone {
  if (cap > 0 && used >= cap) return "danger";
  if (cap > 0 && used >= cap * 0.9) return "warn";
  return base;
}

/** 用量条：cap = 0 表示站长关了这个上限，显示「不限」而不是画一条空的进度条。 */
export function UsageMeter(props: {
  label: ComponentChildren;
  used: number;
  cap: number;
  base?: "v5";
  hint?: ComponentChildren;
}) {
  const { used, cap } = props;
  if (cap > 0) {
    const tone = toneFor(used, cap, props.base);
    return <Meter label={props.label} value={used} max={cap} hint={props.hint} {...(tone ? { tone } : {})} />;
  }
  return (
    <div class="meter">
      <div class="meter__label">
        <span>{props.label}</span>
        <span class="meter__value">{used} / 不限</span>
      </div>
      {props.hint && <p class="meter__hint">{props.hint}</p>}
    </div>
  );
}
