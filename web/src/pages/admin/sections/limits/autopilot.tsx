/** 自动驾驶：每条规则最新的判断（观察模式只记录、不执行）。接口没有改模式的入口，这里只读。 */
import { fullTime } from "../../../../lib/format";
import { Badge, Card } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { TableWrap, withTone } from "../system/forms";
import { AP_MODES, AP_RULES, type Autopilot, autopilotValue } from "./core";

export default function AutopilotCard() {
  const { data, error, loading } = useAdmin<Autopilot>("/admin/api/autopilot");
  const rules = Object.entries(data?.last.rules ?? {});
  return (
    <Card
      title="自动驾驶"
      sub={`「观察」只记录会怎么做、不执行；每 10 分钟判断一次${data?.last.at ? ` · 更新于 ${fullTime(data.last.at)}` : ""}`}
    >
      {loading && !data && <Loading rows={2} />}
      {!data && error && <LoadError error={error} />}
      {data && !rules.length && <p class="muted small">还没有运行记录（每 10 分钟一次）。</p>}
      {rules.length > 0 && (
        <TableWrap label="自动驾驶规则的最新判断">
          <table class="table">
            <caption class="sr-only">自动驾驶规则的最新判断</caption>
            <thead>
              <tr>
                <th scope="col">规则</th>
                <th scope="col">模式</th>
                <th scope="col">判断</th>
                <th scope="col">理由</th>
              </tr>
            </thead>
            <tbody>
              {rules.map(([name, r]) => {
                const mode = AP_MODES[r.mode];
                return (
                  <tr key={name}>
                    <th scope="row">{AP_RULES[name] ?? name}</th>
                    <td>{mode ? <Badge {...withTone(mode.tone)}>{mode.label}</Badge> : r.mode}</td>
                    <td>{autopilotValue(name, r.value)}</td>
                    <td class="muted wrap">{r.why}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </TableWrap>
      )}
    </Card>
  );
}
