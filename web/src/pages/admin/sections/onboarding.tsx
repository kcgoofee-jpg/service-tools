/** 领 Key 与功能：自助领 Key、首页加入按钮、全站功能开关、生成记录、全站重置。 */
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead } from "./_kit";
import type { QuotaAlgo } from "./limits/core";
import AuditCard from "./onboarding/audit";
import type { Ops } from "./onboarding/core";
import DangerCard from "./onboarding/danger";
import { FeaturesCard, HomeJoinCard } from "./onboarding/features";
import RegistrationCard from "./onboarding/registration";
import { Sect } from "./system/forms";

export default function Onboarding() {
  const ops = useAdmin<Ops>("/admin/api/ops");
  const qa = useAdmin<QuotaAlgo>("/admin/api/quota-algo");
  // 动态额度开着时，新成员的默认额度由算法每 10 分钟写入，手改会被覆盖
  const algoOn = !!qa.data && qa.data.params.quota_auto_enabled !== 0;
  return (
    <Sect>
      <SectionHead title="领 Key 与功能" desc="新成员怎么领 Key、默认能用什么，以及全站功能开关" />
      {ops.loading && !ops.data && <Loading rows={4} />}
      {!ops.data && ops.error && <LoadError error={ops.error} />}
      {ops.data && (
        <>
          <RegistrationCard ops={ops.data} algoOn={algoOn} />
          <HomeJoinCard />
          <FeaturesCard ops={ops.data} />
          <AuditCard ops={ops.data} />
          <DangerCard />
        </>
      )}
    </Sect>
  );
}
