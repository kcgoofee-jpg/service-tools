/** 各功能用量：今日 / 近 7 天的 成功 / 失败 / 拒绝，以及产出。只算成员，不含站长 / 测试 Key。点功能名看对应用量日志。 */
import { fullTime, num } from "../../../../lib/format";
import { Card, cx } from "../../../../ui";
import { TableWrap } from "../_kit";
import { isLiveFeature } from "../members/types";
import type { FeatBucket, Overview } from "./types";

export function output(b: FeatBucket): string {
  return (
    [
      b.images ? `${num(b.images)} 张` : "",
      b.tokens ? `${num(b.tokens)} tokens` : "",
      b.anlas ? `${num(b.anlas)} Anlas` : "",
    ]
      .filter(Boolean)
      .join(" · ") || "—"
  );
}

function Triple(props: { b: FeatBucket }) {
  const { b } = props;
  return (
    <span class="tabular">
      {b.ok} / <span class={cx(b.error > 0 && "ov-bad")}>{b.error}</span> /{" "}
      <span class={cx(b.rejected > 0 && "ov-warn")}>{b.rejected}</span>
    </span>
  );
}

export function FeatureUsage(props: { rows: Overview["feature_usage"]; logSince: number | null }) {
  const days = props.logSince ? (Date.now() / 1000 - props.logSince) / 86400 : 7;
  const partial = props.logSince !== null && days < 7;
  return (
    <Card
      title="各功能用量"
      sub={`只算成员，不含站长 / 测试 Key${partial ? ` · 记录从 ${fullTime(props.logSince)} 开始，「近 7 天」还不满 7 天` : ""}`}
      class="ov-card"
    >
      <TableWrap label="各功能用量">
        <table class="table ov-feat">
          <caption class="sr-only">各功能今日与近 7 天用量，成功 / 失败 / 拒绝</caption>
          <thead>
            <tr>
              <th scope="col">功能</th>
              <th scope="col">
                今日
                <span class="ov-sub">成功 / 失败 / 拒绝</span>
              </th>
              <th scope="col">
                近 7 天<span class="ov-sub">成功 / 失败 / 拒绝</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {props.rows
              .filter((f) => isLiveFeature(f.id))
              .map((f) => {
                const idle = !f.week.ok && !f.week.error && !f.week.rejected;
                return (
                  <tr key={f.id} class={cx(idle && "ov-idle")}>
                    <th scope="row">
                      <a href={`/admin/logs/usage?feature=${f.id}`} title="查看这项功能的用量日志">
                        {f.label}
                      </a>
                    </th>
                    <td>
                      <Triple b={f.today} />
                      <span class="ov-sub">{output(f.today)}</span>
                    </td>
                    <td>
                      <Triple b={f.week} />
                      <span class="ov-sub">{output(f.week)}</span>
                    </td>
                  </tr>
                );
              })}
          </tbody>
        </table>
      </TableWrap>
    </Card>
  );
}
