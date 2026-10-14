/** 图片排队与冷却：排队等待上限、任务间隔、429 冷却。 */
import { useState } from "preact/hooks";
import { Card } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { NumField, SaveBar, useForm } from "../system/forms";
import { RUNTIME_SPECS } from "./core";

const URL = "/admin/api/runtime-limits";

export default function RuntimeCard() {
  const { data, error, loading } = useAdmin<Record<string, number>>(URL);
  const form = useForm(RUNTIME_SPECS, data);
  const [saving, setSaving] = useState(false);
  if (loading && !data)
    return (
      <Card title="图片排队与冷却">
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title="图片排队与冷却">
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );

  const save = async () => {
    setSaving(true);
    const ok = await act("PUT", URL, form.changes(), "运行限制已保存并生效");
    setSaving(false);
    if (ok) form.commit();
  };
  return (
    <Card title="图片排队与冷却" sub="排队等待与任务间隔：间隔越短出图越快，也越容易触发上游限流。">
      <form {...form.formProps} onSubmit={(e) => e.preventDefault()}>
        <div class="form-grid">
          {Object.keys(RUNTIME_SPECS).map((n) => (
            <NumField key={n} form={form} name={n} />
          ))}
        </div>
        <SaveBar form={form} onSave={save} saving={saving} label="保存运行限制" />
      </form>
    </Card>
  );
}
