/** 分区通用小部件：页头、指标格、加载 / 错误占位。各分区只用这些 + ui 组件库。 */
import type { ComponentChildren } from "preact";
import { Notice, Skeleton } from "../../../ui";

export function SectionHead(props: { title: string; desc?: ComponentChildren; actions?: ComponentChildren }) {
  return (
    <header class="section-head">
      <div>
        <h1>{props.title}</h1>
        {props.desc && <p>{props.desc}</p>}
      </div>
      <span class="spacer" />
      {props.actions}
    </header>
  );
}

export function Stat(props: {
  label: ComponentChildren;
  value: ComponentChildren;
  hint?: ComponentChildren;
}) {
  return (
    <div class="stat">
      <div class="stat__label">{props.label}</div>
      <div class="stat__value">{props.value}</div>
      {props.hint && <div class="stat__hint">{props.hint}</div>}
    </div>
  );
}

export function Loading(props: { rows?: number }) {
  return (
    <div class="stack" aria-busy="true">
      {Array.from({ length: props.rows ?? 3 }, (_, i) => (
        <Skeleton key={i} h="56px" />
      ))}
    </div>
  );
}

export function LoadError(props: { error: string }) {
  return <Notice tone="danger">加载失败：{props.error}</Notice>;
}
