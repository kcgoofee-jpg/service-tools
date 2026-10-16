/**
 * 成员页的筛选横栏：一排标签（名称 + 实时人数），单行、窄屏横向滚动，不换行，也没有页签 + 下拉两处。
 * 选中的标签高亮，再点一次取消（回到「全部成员」）；选中「24 小时被拒过」时，被拒原因紧跟在它后面展开，
 * 是在它之内的第二层筛选。
 */
import { Fragment } from "preact";
import { useEffect, useRef } from "preact/hooks";
import { cx } from "../../../../ui";

export type Chip = {
  id: string;
  label: string;
  count: number;
  hint?: string;
  /** 有人需要处理时用醒目的颜色显示人数 */
  hot?: boolean;
};
export type Reason = { tag: string; count: number };

export function FilterBar(props: {
  chips: Chip[];
  /** 当前选中的筛选 id；不在 chips 里（如跳转来的站长 Key）时，没有标签高亮 */
  active: string;
  onToggle: (id: string) => void;
  /** 跟在 reasonsOf 那个标签后面的原因；只在它被选中时显示 */
  reasons: Reason[];
  reason: string | null;
  onReason: (tag: string | null) => void;
  /** 展开原因的那个标签的 id */
  reasonsOf: string;
}) {
  const bar = useRef<HTMLDivElement>(null);
  const showReasons = props.active === props.reasonsOf && props.reasons.length > 0;
  // 窄屏：选中的标签（和选中的原因）要滚进视野，别躲在右边看不见
  useEffect(() => {
    const on = bar.current?.querySelectorAll<HTMLElement>(".mb-chip[aria-pressed='true']");
    on?.item(on.length - 1)?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [props.active, props.reason]);

  return (
    // biome-ignore lint/a11y/useSemanticElements: 一排并列的切换按钮，role=group + aria-label 即标准写法（和分段按钮一致）
    <div class="mb-bar" role="group" aria-label="筛选成员" ref={bar}>
      {props.chips.map((c) => (
        <Fragment key={c.id}>
          <button
            type="button"
            class="mb-chip"
            aria-pressed={c.id === props.active}
            title={c.hint}
            onClick={() => props.onToggle(c.id)}
          >
            {c.label}
            <span class={cx("mb-n", c.hot && c.count > 0 && "is-hot")}>{c.count}</span>
          </button>
          {c.id === props.reasonsOf && showReasons && (
            <>
              <span class="mb-bar__lead" aria-hidden="true">
                原因
              </span>
              {props.reasons.map((r) => (
                <button
                  key={r.tag}
                  type="button"
                  class="mb-chip is-sub"
                  aria-pressed={props.reason === r.tag}
                  title={`近 24 小时因「${r.tag}」被拒的成员`}
                  onClick={() => props.onReason(props.reason === r.tag ? null : r.tag)}
                >
                  {r.tag}
                  <span class="mb-n">{r.count}</span>
                </button>
              ))}
            </>
          )}
        </Fragment>
      ))}
    </div>
  );
}
