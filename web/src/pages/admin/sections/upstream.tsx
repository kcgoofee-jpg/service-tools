/** 上游：令牌池、V5 官方余量、上游表现提示、Anlas 消耗核对。 */
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead } from "./_kit";
import { Sect } from "./system/forms";
import AccountsCard from "./upstream/accounts";
import AllowanceCard from "./upstream/allowance";
import type { PoolData } from "./upstream/core";
import PerfCard from "./upstream/perf";
import ReconcileCard from "./upstream/reconcile";
import TokensCard from "./upstream/tokens";
import "./upstream.css";

export default function Upstream() {
  const { data, error, loading } = useAdmin<PoolData>("/admin/api/overview");
  return (
    <Sect>
      <SectionHead title="上游" desc="NovelAI 账号池、V5 官方余量与上游表现" />
      {loading && !data && <Loading rows={3} />}
      {!data && error && <LoadError error={error} />}
      {data && <TokensCard pool={data.pool} configured={data.pool_configured} />}
      <AccountsCard />
      <AllowanceCard />
      <PerfCard />
      <ReconcileCard />
    </Sect>
  );
}
