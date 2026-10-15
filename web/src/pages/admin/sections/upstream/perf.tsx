/** 上游表现：只留系统根据耗时、限流、成功率判断出的提示（V4.5 / V5 分开统计，和过去 7 天对比）。 */
import { fullTime } from "../../../../lib/format";
import { Card, Notice } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import type { Perf } from "./core";

export default function PerfCard() {
  const { data, error, loading } = useAdmin<Perf>("/admin/api/perf");
  return (
    <Card
      title="上游表现"
      sub={
        data
          ? `V4.5 / V5 分开统计，和过去 7 天（不含最近 24 小时）对比 · 生成于 ${fullTime(data.generated_at)}`
          : "V4.5 / V5 分开统计"
      }
    >
      {loading && !data && <Loading rows={1} />}
      {!data && error && <LoadError error={error} />}
      {data &&
        (data.flags.length > 0 ? (
          <div class="stack-sm">
            {data.flags.map((f) => (
              <Notice key={`${f.code}-${f.family}`} tone={f.level === "bad" ? "danger" : "warn"}>
                {f.text}
              </Notice>
            ))}
          </div>
        ) : (
          <Notice tone="ok">暂无异常：耗时、限流、成功率都在正常范围。</Notice>
        ))}
    </Card>
  );
}
