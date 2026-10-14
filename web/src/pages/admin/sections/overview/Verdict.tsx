/** 总览顶部：一句话结论 + 要处理的事（每条带「去看看」和「隐藏 24 小时」）。 */
import { X } from "lucide-preact";
import { Badge, Button, cx } from "../../../../ui";
import type { Verdict as VerdictData } from "./reminders";

const LEVEL = {
  bad: { tone: "danger", text: "紧急" },
  warn: { tone: "warn", text: "注意" },
  info: { tone: "info", text: "提示" },
} as const;

export function Verdict(props: {
  verdict: VerdictData;
  onHide: (key: string) => void;
  onRestore: () => void;
}) {
  const { verdict: v } = props;
  return (
    <section class={cx("card ov-verdict", `is-${v.tone}`)} aria-labelledby="ov-verdict-h">
      <header class="ov-verdict__head">
        <span class="ov-dot" aria-hidden="true" />
        <h2 id="ov-verdict-h" class="card__title">
          {v.headline}
        </h2>
        <span class="spacer" />
        {v.hiddenCount > 0 && (
          <Button size="sm" variant="ghost" onClick={props.onRestore} title="把隐藏的提醒全部恢复">
            已隐藏 {v.hiddenCount} 条 · 恢复
          </Button>
        )}
      </header>
      {v.visible.length > 0 && (
        <ul class="ov-issues">
          {v.visible.map((x) => (
            <li key={x.key} class={cx("ov-issue", `is-${x.level}`)}>
              <Badge tone={LEVEL[x.level].tone}>{LEVEL[x.level].text}</Badge>
              <div class="ov-issue__t">
                <b>{x.title}</b>
                {x.detail && <span class="small muted">{x.detail}</span>}
              </div>
              <div class="ov-issue__a">
                <a class="btn btn--sm" href={x.to} aria-label={`去看看：${x.label}`}>
                  去看看
                </a>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`隐藏「${x.title}」，24 小时内不再出现`}
                  title="隐藏这条（24 小时内不再出现）"
                  onClick={() => props.onHide(x.key)}
                >
                  <X size={14} aria-hidden="true" />
                  隐藏
                </Button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
