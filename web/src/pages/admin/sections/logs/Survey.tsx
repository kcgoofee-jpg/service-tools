/** 成员问卷（/survey 网页版）：汇总 + 明细。一人一份，可覆盖。 */
import { useState } from "preact/hooks";
import { fullTime } from "../../../../lib/format";
import { Badge, Button, Skeleton } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Stat } from "../_kit";
import { Empty, NARROW, TableScroll, useMedia } from "../members/shared";
import { surveyStats } from "./feedbackStats";
import type { SurveyResp, SurveyRow } from "./types";

const PAGE = 50;

const Donate = (props: { v: string | null | undefined }) =>
  props.v ? (
    <Badge {...(props.v.includes("愿意") ? { tone: "ok" as const } : {})}>{props.v}</Badge>
  ) : (
    <span class="muted">—</span>
  );

function Improve(props: { r: SurveyRow }) {
  const { r } = props;
  return (
    <>
      {r.improve || "—"}
      {r.improve_other && <div class="small muted">补充：{r.improve_other}</div>}
    </>
  );
}

export function Survey() {
  const narrow = useMedia(NARROW);
  const { data, error } = useAdmin<SurveyResp>("/admin/api/survey");
  const [limit, setLimit] = useState(PAGE);
  if (error && !data) return <LoadError error={error} />;
  if (!data) return <Skeleton h="240px" />;
  const rows = data.rows ?? [];
  const s = surveyStats(rows);
  const shown = rows.slice(0, limit);
  const who = (r: SurveyRow) => r.name || r.discord_id || "—";

  return (
    <div class="stack">
      {rows.length > 0 && (
        <div class="stats">
          <Stat label="已填" value={`${s.total} 人`} hint="在册成员里的一份答卷" />
          <Stat label="理解限额" value={`${s.understandPct}%`} hint="完全 / 基本理解" />
          <Stat label="愿意捐 V4.5" value={`${s.willing} 人`} hint={`暂不考虑 ${s.declined} 人`} />
          <Stat
            label="Top 改进诉求"
            value={<span class="lg-statv">{s.topImprove ? s.topImprove[0] : "暂无"}</span>}
            hint={s.topImprove ? `${s.topImprove[1]} 人提到` : ""}
          />
        </div>
      )}
      <div class="card lg-listcard">
        {rows.length === 0 ? (
          <Empty title="还没有成员填过问卷">链接在奶妹的私信里：/survey</Empty>
        ) : narrow ? (
          <ul class="lg-cards" aria-label="成员问卷">
            {shown.map((r, i) => (
              <li key={`${r.ts}-${i}`} class="lg-card">
                <div class="lg-card__top">
                  <b>{who(r)}</b>
                  <span class="small muted">{fullTime(r.ts)}</span>
                </div>
                <div class="small">
                  <span class="muted">客户端：</span>
                  {r.client || "—"}
                </div>
                <div class="small">
                  <span class="muted">限额理解：</span>
                  {r.understand || "—"}
                </div>
                <div class="small">
                  <span class="muted">希望改进：</span>
                  <Improve r={r} />
                </div>
                <div class="small">
                  <span class="muted">捐 V4.5：</span>
                  <Donate v={r.donate} />
                </div>
              </li>
            ))}
          </ul>
        ) : (
          <TableScroll label="成员问卷">
            <table class="table lg-table">
              <caption class="sr-only">成员问卷</caption>
              <thead>
                <tr>
                  <th scope="col">时间</th>
                  <th scope="col">成员</th>
                  <th scope="col">客户端</th>
                  <th scope="col">限额理解</th>
                  <th scope="col">希望改进</th>
                  <th scope="col">捐 V4.5</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((r, i) => (
                  <tr key={`${r.ts}-${i}`}>
                    <td class="nowrap">{fullTime(r.ts)}</td>
                    <td>{who(r)}</td>
                    <td>{r.client || "—"}</td>
                    <td>{r.understand || "—"}</td>
                    <td class="lg-ans">
                      <Improve r={r} />
                    </td>
                    <td>
                      <Donate v={r.donate} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableScroll>
        )}
        {rows.length > shown.length && (
          <div class="lg-more">
            <Button onClick={() => setLimit(limit + PAGE)}>
              显示更多（还有 {rows.length - shown.length} 条）
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
