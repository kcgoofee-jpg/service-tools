/** 账号保护与排队：每个上游账号的每日 / 每小时上限、安静时段、间隔抖动、排队数。 */
import { useMemo, useState } from "preact/hooks";
import { Card, Notice, toast } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { actOrError, ConfirmDialog, LoadError, Loading } from "../_kit";
import { Derive, NumField, SaveBar, useForm } from "../system/forms";
import { GUARD_ADVANCED, GUARD_CORE, type Guard, guardSpecs, pacedText } from "./core";

const URL = "/admin/api/guard";

export default function GuardCard() {
  const { data, error, loading } = useAdmin<Guard>(URL);
  const specs = useMemo(() => (data ? guardSpecs(data) : {}), [data]);
  const form = useForm(specs, data?.values ?? null);
  const [saving, setSaving] = useState(false);
  const [zero, setZero] = useState<string[] | null>(null);

  if (loading && !data)
    return (
      <Card title="账号保护与排队">
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title="账号保护与排队">
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );

  const cap = Number(form.raw("account_daily_cap")) || 0;
  const burst = Number(form.raw("daily_burst")) || 0;
  const paced = pacedText(cap, burst);

  /** 返回 undefined = 已保存；返回字符串 = 失败原因 */
  const doSave = async () => {
    setSaving(true);
    const err = await actOrError("PUT", URL, form.changes(), "账号保护已保存，立即生效");
    setSaving(false);
    if (!err) form.commit();
    return err;
  };
  const trySave = () => {
    const ch = form.changes();
    // 每小时 / 每日上限是保护唯一上游账号的硬闸门：填 0 = 关掉该项，必须确认
    const zeros = ["account_hourly_cap", "account_daily_cap"].filter((k) => k in ch && ch[k] === 0);
    if (zeros.length) setZero(zeros.map((k) => form.specs[k]?.label ?? k));
    else void doSave().then((err) => err && toast(err, { error: true }));
  };

  return (
    <Card
      title="账号保护与排队"
      sub="每个上游账号的出图总量与节奏：总量和节奏比单人限额更能保护账号；保存后立即生效。"
    >
      <div class="stack-sm">
        <form class="stack-sm" {...form.formProps} onSubmit={(e) => e.preventDefault()}>
          <div class="form-grid">
            {GUARD_CORE.map((n) => (
              <NumField key={n} form={form} name={n} />
            ))}
          </div>
          {paced && (
            <Derive title="按时段放行的推算">
              <div>{paced}</div>
              <div class="muted">没用完的额度留给后面的时段；0 张 = 不分时段，整天先到先得。</div>
            </Derive>
          )}
          <details class="more">
            <summary class="small">更多：安静时段、间隔抖动、排队上限</summary>
            <div class="form-grid" style={{ marginTop: "var(--sp-3)" }}>
              {GUARD_ADVANCED.map((n) => (
                <NumField key={n} form={form} name={n} />
              ))}
            </div>
          </details>
          <p class="muted small">
            V4.5 由「动态额度」管理：每人上限 {data.values.base_daily_images} 张，先到先得。
          </p>
          <SaveBar form={form} onSave={trySave} saving={saving} label="保存账号保护" />
        </form>
      </div>

      <ConfirmDialog
        open={zero !== null}
        title="确定关掉这项保护？"
        confirmText="关掉保护并保存"
        danger
        onClose={() => setZero(null)}
        onConfirm={doSave}
      >
        <Notice tone="warn">
          「{(zero ?? []).join("」「")}」填了
          0，等于关掉这项对上游账号的限量。上游账号被限流或封控的风险会上升。
        </Notice>
      </ConfirmDialog>
    </Card>
  );
}
