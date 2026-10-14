/** V5 官方余量：每个账号剩余百分比、恢复速度，以及恢复速度怎么推算出来的。 */
import { fullTime } from "../../../../lib/format";
import { Badge, Card, Meter, Notice } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { imagesPerPercent, type ModulesLite } from "../limits/core";
import { Derive, withTone } from "../system/forms";
import { type Allowance, type AllowanceAccount, rechargeDerivation } from "./core";

function Account(props: { a: AllowanceAccount; threshold: number; per: number }) {
  const a = props.a;
  const exhausted = a.is_negative === true;
  const tone = exhausted ? "danger" : a.low ? "warn" : undefined;
  const d = a.recharge_per_day ? rechargeDerivation(a.recharge_per_day, props.per) : null;
  return (
    <div class="stack-sm">
      <div class="row">
        <b>账号 {a.account}</b>
        {exhausted && <Badge tone="danger">已耗尽</Badge>}
        {!exhausted && a.low && <Badge tone="warn">低于 {props.threshold}%</Badge>}
        {a.uncertain && a.percent != null && <Badge>上次记录，待下次生成确认</Badge>}
        {a.checked_at && <span class="muted small">更新于 {fullTime(a.checked_at)}</span>}
      </div>
      {a.percent == null ? (
        <p class="muted small">还没有读数，首次 V5 生图时会确认。</p>
      ) : (
        <Meter
          label="V5 剩余"
          value={exhausted ? 0 : a.percent}
          max={100}
          text={exhausted ? "已耗尽" : `${a.percent}%`}
          {...withTone(tone)}
        />
      )}
      {a.error && <Notice tone="warn">{a.error}：V5 生图需确认后继续。</Notice>}
      {d && a.recharge_per_day != null && (
        <Derive title="恢复速度怎么算的">
          <div>
            恢复速度 <b>{a.recharge_per_day}%/天</b> = 24 小时 ÷ 每 1% 约{" "}
            <b>{d.hoursPerPercent.toFixed(1)} 小时</b>
          </div>
          <div>
            折成 V5 张数 ≈ {a.recharge_per_day} × {props.per} 张/1% ≈ <b>{d.imagesPerDay} 张/天</b>
          </div>
          <div class="muted">
            官方只给「再恢复 1% 还要多少秒」，取最近 48
            小时里最长的一次读数当周期（刚好读在快恢复完时单次读数会把速度算高）。
          </div>
        </Derive>
      )}
    </div>
  );
}

export default function AllowanceCard() {
  const { data, error, loading } = useAdmin<Allowance>("/admin/api/allowance");
  const mods = useAdmin<ModulesLite>("/admin/api/modules");
  const per = imagesPerPercent(mods.data);
  return (
    <Card
      title="V5 官方余量"
      sub={
        data
          ? `余量低于 ${data.threshold}% 时告警（阈值在「额度与保护 › Anlas 与 V5 预算」里改）。读数只在有 V5 生图或定时查询时更新。`
          : undefined
      }
    >
      {loading && !data && <Loading rows={1} />}
      {!data && error && <LoadError error={error} />}
      {data && !data.accounts.length && <p class="muted small">未配置上游账号。</p>}
      <div class="stack">
        {data?.accounts.map((a) => (
          <Account key={a.account} a={a} threshold={data.threshold} per={per.value} />
        ))}
      </div>
    </Card>
  );
}
