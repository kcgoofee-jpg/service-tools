/** 成员问卷：成员私信里的 /survey 网页问卷（Discord 的 /反馈 在「日志」里）。重复提交只保留最新一份。 */
import { fullTime } from "../../../../lib/format";
import { Card } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading, Stat } from "../_kit";
import { TableWrap } from "../system/forms";
import { type Survey, surveyFacts } from "./core";

export default function SurveyCard() {
  const { data, error, loading } = useAdmin<Survey>("/admin/api/survey");
  if (loading && !data)
    return (
      <Card title="成员问卷">
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title="成员问卷">
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );
  const f = surveyFacts(data);
  return (
    <Card title="成员问卷" sub="成员私信里的 /survey 网页问卷；重复提交只保留最新一份。">
      {!data.rows.length ? (
        <p class="muted small">还没有成员填过问卷。链接在奶妹的私信里（/survey）。</p>
      ) : (
        <div class="stack-sm">
          <div class="stats">
            <Stat label="已填" value={`${f.total} 人`} />
            <Stat label="理解限额" value={`${f.understoodPct}%`} hint="完全 / 基本理解" />
            <Stat label="愿意捐 V4.5" value={`${f.willing} 人`} />
            <Stat
              label="最多人提的改进"
              value={f.topImprove ? f.topImprove[0] : "暂无"}
              hint={f.topImprove ? `${f.topImprove[1]} 人提到` : undefined}
            />
          </div>
          <TableWrap label="成员问卷明细">
            <table class="table">
              <caption class="sr-only">成员问卷明细</caption>
              <thead>
                <tr>
                  <th scope="col">提交时间</th>
                  <th scope="col">成员</th>
                  <th scope="col">客户端</th>
                  <th scope="col">限额理解</th>
                  <th scope="col">希望改进</th>
                  <th scope="col">捐 V4.5</th>
                </tr>
              </thead>
              <tbody>
                {data.rows.map((r) => (
                  <tr key={`${r.ts}-${r.discord_id ?? r.name}`}>
                    <td class="nowrap">{fullTime(r.ts)}</td>
                    <td>{r.name || r.discord_id || "—"}</td>
                    <td class="nowrap">{r.client || "—"}</td>
                    <td>{r.understand || "—"}</td>
                    <td class="wrap">
                      {r.improve || "—"}
                      {r.improve_other && <div class="muted small">补充：{r.improve_other}</div>}
                    </td>
                    <td>{r.donate || "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        </div>
      )}
    </Card>
  );
}
