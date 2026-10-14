/** 上游表现：V4.5 / V5 分开，最近 60 分钟、最近 24 小时与过去 7 天基线对比；近 24 小时每小时请求量。 */
import { fullTime } from "../../../../lib/format";
import { Badge, Card, Notice } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { TableWrap } from "../system/forms";
import { hourlyPeak, type Perf, type PerfFamily, type PerfWindow, rateText, secText } from "./core";

function HourlyChart(props: { fam: string; hours: PerfFamily["hourly"] }) {
  const peak = hourlyPeak(props.hours);
  if (!peak) return null;
  const total = props.hours.reduce((a, h) => a + h.ok + h.err, 0);
  return (
    <div class="hchart">
      <div
        class="hchart__bars"
        role="img"
        aria-label={`${props.fam} 近 24 小时每小时请求量，共 ${total} 次，峰值 ${peak} 次/小时`}
      >
        {props.hours.map((h) => {
          const t = h.ok + h.err;
          const hh = new Date(h.t * 1000).toLocaleTimeString("zh-CN", {
            timeZone: "Asia/Shanghai",
            hour: "2-digit",
            hour12: false,
          });
          return (
            <div class="hchart__col" key={h.t} title={`${hh} 点 · 请求 ${t}（成功 ${h.ok} / 失败 ${h.err}）`}>
              <div
                class="hchart__stack"
                style={{ height: `${t ? Math.max(4, Math.round((t / peak) * 100)) : 0}%` }}
              >
                <div class="hchart__err" style={{ flexGrow: h.err }} />
                <div class="hchart__ok" style={{ flexGrow: h.ok }} />
              </div>
            </div>
          );
        })}
      </div>
      <p class="muted small">
        近 24 小时每小时请求量（北京时间，最右为当前小时；峰值 {peak} 次/小时）：
        <span class="legend legend--ok">成功</span>
        <span class="legend legend--err">失败</span>
      </p>
    </div>
  );
}

function Row(props: { label: string; w: PerfWindow; first?: boolean }) {
  const w = props.w;
  const other = w.auth + w.server + w.other;
  return (
    <tr>
      <th scope="row" class="nowrap">
        {props.label}
      </th>
      <td class="num">
        {w.requests} / {rateText(w.success_rate)}
      </td>
      <td class="num">{w.rate_limited ? <Badge tone="warn">{w.rate_limited}</Badge> : 0}</td>
      <td class="num">{other ? <Badge tone="danger">{other}</Badge> : 0}</td>
      <td class="num">{secText(w.p50_ms)}</td>
      <td class="num">{secText(w.p90_ms)}</td>
      <td class="num">{secText(w.avg_wait_ms)}</td>
      <td class="num">
        {w.actual_per_hour ?? "—"} / {w.capacity_per_hour ?? "—"}
        {w.member_capacity_per_hour != null && (
          <div class="muted small">单人最多约 {w.member_capacity_per_hour}</div>
        )}
      </td>
    </tr>
  );
}

function Family(props: { name: string; f: PerfFamily }) {
  const { f, name } = props;
  return (
    <section class="stack-sm" aria-label={`${name} 表现`}>
      <h3 class="form-section__title">{name}</h3>
      <HourlyChart fam={name} hours={f.hourly} />
      <TableWrap label={`${name} 的请求、成功率、耗时与产能`}>
        <table class="table">
          <caption class="sr-only">{name} 的请求、成功率、耗时与产能</caption>
          <thead>
            <tr>
              <th scope="col">时段</th>
              <th scope="col" class="num">
                请求 / 成功率
              </th>
              <th scope="col" class="num">
                限流（429）
              </th>
              <th scope="col" class="num">
                其他失败
              </th>
              <th scope="col" class="num">
                生成 P50
              </th>
              <th scope="col" class="num">
                生成 P90
              </th>
              <th scope="col" class="num">
                平均排队
              </th>
              <th scope="col" class="num">
                实际 / 上限（张/时）
              </th>
            </tr>
          </thead>
          <tbody>
            <Row label="最近 60 分钟" w={f.h1} />
            <Row label="最近 24 小时" w={f.d1} />
            <Row label="过去 7 天基线" w={f.baseline} />
          </tbody>
        </table>
      </TableWrap>
      {!f.baseline_ready && (
        <p class="muted small">基线样本不足：需要先积累几天带耗时的日志，才能判断「变慢」。</p>
      )}
    </section>
  );
}

export default function PerfCard() {
  const { data, error, loading } = useAdmin<Perf>("/admin/api/perf");
  const fams = Object.entries(data?.families ?? {});
  return (
    <Card
      title="上游表现"
      sub={
        data
          ? `V4.5 / V5 分开统计，和过去 7 天（不含最近 24 小时）对比 · 生成于 ${fullTime(data.generated_at)}`
          : "V4.5 / V5 分开统计"
      }
    >
      {loading && !data && <Loading rows={2} />}
      {!data && error && <LoadError error={error} />}
      {data && (
        <div class="stack">
          {data.flags.length > 0 ? (
            <div class="stack-sm">
              {data.flags.map((f) => (
                <Notice key={`${f.code}-${f.family}`} tone={f.level === "bad" ? "danger" : "warn"}>
                  {f.text}
                </Notice>
              ))}
            </div>
          ) : (
            fams.length > 0 && <Notice tone="ok">暂无异常：耗时、限流、成功率都在正常范围。</Notice>
          )}
          {!fams.length && <p class="muted small">暂无出图记录。</p>}
          {fams.map(([name, f]) => (
            <Family key={name} name={name} f={f} />
          ))}
          {fams.length > 0 && (
            <details class="more">
              <summary class="small muted">怎么看这些数字</summary>
              <ul class="std-list small">
                <li>
                  P50 = 中位数：一半的图比这快，代表典型速度；P90：90% 的图比这快，看较慢那 10% 有多慢。
                </li>
                <li>平均排队 = 收到请求到发往上游之间的等待（排队 + 冷却）。</li>
                <li>
                  实际 = 这个时段平均每小时出的张数；上限 = min（3600 秒 ÷ max（P50 秒，出图间隔{" "}
                  {data.site_interval} 秒）× 并发槽 {data.slots}，每小时上限 × 可用账号数）。单人最多 =
                  同样的公式换成每把 Key 的间隔 {data.key_interval} 秒。
                </li>
                <li>限流 = 上游返回 429；其他失败 = 401 / 402 / 403 / 5xx 或连接失败。</li>
              </ul>
            </details>
          )}
        </div>
      )}
    </Card>
  );
}
