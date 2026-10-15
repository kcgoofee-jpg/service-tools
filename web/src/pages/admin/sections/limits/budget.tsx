/** Anlas 与 V5 预算：全站月度 Anlas、全站每日 V5、Vibe 每日 Anlas 池、每人每日 Anlas、V5 余量低提示线。 */
import { useState } from "preact/hooks";
import { Card } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { NumField, SaveBar, useForm } from "../system/forms";
import { BUDGET_SPECS, type QuotaAlgo } from "./core";

const URL = "/admin/api/settings";

type Settings = {
  global_monthly_anlas: number;
  global_daily_v5: number;
  global_daily_anlas: number;
  member_daily_anlas: number;
  v5_alert_threshold: number;
};

export default function BudgetCard() {
  const { data, error, loading } = useAdmin<Settings>(URL);
  const qa = useAdmin<QuotaAlgo>("/admin/api/quota-algo");
  const form = useForm(BUDGET_SPECS, data as Record<string, unknown> | null);
  const [saving, setSaving] = useState(false);
  if (loading && !data)
    return (
      <Card title="Anlas 与 V5 预算">
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title="Anlas 与 V5 预算">
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );

  // 动态额度开着时，算法每 10 分钟把全站 V5 日限写成它算出的值：手填的数字会被覆盖
  const algoManaged = !!qa.data && qa.data.params.quota_auto_enabled !== 0;

  const save = async () => {
    setSaving(true);
    // 后端的 PUT 里缺省字段保持原值，所以只发改过的
    const ok = await act("PUT", URL, form.changes(), "预算已保存");
    setSaving(false);
    if (ok) form.commit();
  };

  return (
    <Card title="Anlas 与 V5 预算" sub="全站月度 Anlas 与每日 V5 额度；0 = 不限。">
      <div class="stack-sm">
        <form {...form.formProps} onSubmit={(e) => e.preventDefault()}>
          <div class="form-grid">
            <NumField form={form} name="global_monthly_anlas" />
            <NumField
              form={form}
              name="global_daily_v5"
              disabled={algoManaged}
              hint={
                algoManaged
                  ? `由动态额度管理（每 10 分钟自动设置，现在 ${data.global_daily_v5} 张）；关闭动态额度后才能手动改`
                  : undefined
              }
            />
            <NumField form={form} name="global_daily_anlas" />
            <NumField form={form} name="member_daily_anlas" />
            <NumField form={form} name="v5_alert_threshold" />
          </div>
          <SaveBar form={form} onSave={save} saving={saving} label="保存预算" />
        </form>
      </div>
    </Card>
  );
}
