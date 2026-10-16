/** 额度与保护：动态额度、账号保护与排队、图片排队与冷却、Anlas 与 V5 预算、V5 Medium 降档。 */
import { SectionHead } from "./_kit";
import BudgetCard from "./limits/budget";
import GuardCard from "./limits/guard";
import MediumCard from "./limits/medium";
import QuotaCard from "./limits/quota";
import RuntimeCard from "./limits/runtime";
import { Sect } from "./system/forms";

export default function Limits() {
  return (
    <Sect>
      <SectionHead title="额度与保护" desc="每人每天能出多少张，以及保护上游账号的硬上限" />
      <QuotaCard />
      <GuardCard />
      <RuntimeCard />
      <BudgetCard />
      <MediumCard />
    </Sect>
  );
}
