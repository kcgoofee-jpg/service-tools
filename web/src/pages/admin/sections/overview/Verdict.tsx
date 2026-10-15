/** 总览顶部：一句话结论 + 要处理的事（每条带「去看看」）。 */
import { Badge, cx } from "../../../../ui";
import type { Verdict as VerdictData } from "./reminders";

const LEVEL = {
  bad: { tone: "danger", text: "紧急" },
  warn: { tone: "warn", text: "注意" },
} as const;

export function Verdict(props: { verdict: VerdictData }) {
  const { verdict: v } = props;
  return (
    <section class={cx("card ov-verdict", `is-${v.tone}`)} aria-labelledby="ov-verdict-h">
      <header class="ov-verdict__head">
        <span class="ov-dot" aria-hidden="true" />
        <h2 id="ov-verdict-h" class="card__title">
          {v.headline}
        </h2>
      </header>
      {v.items.length > 0 && (
        <ul class="ov-issues">
          {v.items.map((x) => (
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
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
