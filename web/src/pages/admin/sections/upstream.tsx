/** 上游：令牌池、V5 官方余量、上游表现、Anlas 消耗核对。 */
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead, Stat } from "./_kit";
import { Sect } from "./system/forms";
import AllowanceCard from "./upstream/allowance";
import type { PoolData } from "./upstream/core";
import PerfCard from "./upstream/perf";
import ReconcileCard from "./upstream/reconcile";
import TokensCard from "./upstream/tokens";
import "./upstream.css";

export default function Upstream() {
  const { data, error, loading } = useAdmin<PoolData>("/admin/api/overview");
  const pool = data?.pool ?? [];
  const usable = pool.filter((p) => p.usable).length;
  const images = pool.reduce((a, p) => a + p.images_today, 0);
  const fails = pool.reduce((a, p) => a + p.fails, 0);
  const v5 = pool.reduce((a, p) => a + p.v5_used, 0);
  return (
    <Sect>
      <SectionHead title="上游" desc="NovelAI 账号池、V5 官方余量与上游表现" />
      {loading && !data && <Loading rows={3} />}
      {!data && error && <LoadError error={error} />}
      {data && (
        <>
          <div class="stats">
            <Stat label="可用账号" value={`${usable} / ${pool.length}`} hint="正常、未停用、未被限流" />
            <Stat label="今日出图（账号合计）" value={images} hint={`失败 ${fails} 次`} />
            <Stat label="今日免费 V5（账号合计）" value={v5} />
          </div>
          <TokensCard pool={pool} configured={data.pool_configured} />
        </>
      )}
      <AllowanceCard />
      <PerfCard />
      <ReconcileCard />
    </Sect>
  );
}
