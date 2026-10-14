/** 首页「加入 Discord」按钮 + 全站功能开关。 */
import { useMemo, useState } from "preact/hooks";
import { Badge, Card, Notice } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import type { FieldSpec } from "../system/core";
import { ConfirmDialog, SaveBar, SwitchRow, useForm } from "../system/forms";
import { ANLAS_FEATURES, type Ops } from "./core";

export function HomeJoinCard() {
  const { data } = useAdmin<{ open: boolean }>("/admin/api/home/join");
  const [pending, setPending] = useState(false);
  if (!data) return null;
  return (
    <Card
      title="首页管理"
      sub="首页「加入 Discord」按钮：独立开关，与「自助领 Key」无关。这里只管按钮显示，领不领得到 Key 由上面的设置管；保存不发公告。"
    >
      <SwitchRow
        title={data.open ? "加入按钮开放（显示邀请链接）" : "已置灰「关闭注册」"}
        desc="开 = 显示邀请按钮；关 = 置灰为「关闭注册」"
        checked={data.open}
        disabled={pending}
        onChange={async (v) => {
          setPending(true);
          await act(
            "POST",
            "/admin/api/home/join",
            { open: v },
            v ? "首页加入按钮已开放" : "首页加入按钮已置灰为「关闭注册」",
          );
          setPending(false);
        }}
      />
    </Card>
  );
}

export function FeaturesCard(props: { ops: Ops }) {
  const { global, labels } = props.ops.features;
  const names = Object.keys(labels);
  const specs = useMemo<Record<string, FieldSpec>>(() => {
    const o: Record<string, FieldSpec> = {};
    for (const n of names) o[n] = { kind: "bool", label: labels[n] ?? n };
    return o;
  }, [labels, names.join(",")]);
  const form = useForm(specs, global);
  const [saving, setSaving] = useState(false);
  const [confirm, setConfirm] = useState(false);

  const ch = form.changes();
  const turningOn = Object.keys(ch).filter((n) => ch[n] === true && ANLAS_FEATURES[n]);
  const turningOff = Object.keys(ch).filter((n) => ch[n] === false);

  const save = async () => {
    setSaving(true);
    const ok = await act("PUT", "/admin/api/ops/features", { flags: ch }, "功能开关已保存");
    setSaving(false);
    if (ok) {
      form.commit();
      setConfirm(false);
    }
  };

  return (
    <Card title="功能开关" sub="关闭后所有成员立即不可用；管理员 Key 仍可使用。">
      <form {...form.formProps} onSubmit={(e) => e.preventDefault()}>
        {names.map((n) => (
          <SwitchRow
            key={n}
            title={labels[n]}
            {...(ANLAS_FEATURES[n]
              ? {
                  tag: (
                    <Badge tone="warn" title="此功能消耗上游官方付费点数（Anlas）">
                      消耗 Anlas
                    </Badge>
                  ),
                  desc: ANLAS_FEATURES[n],
                }
              : {})}
            checked={form.raw(n) === true}
            onChange={(v) => form.set(n, v)}
          />
        ))}
        <SaveBar
          form={form}
          saving={saving}
          onSave={() => (turningOn.length || turningOff.length ? setConfirm(true) : void save())}
        />
      </form>
      <ConfirmDialog
        open={confirm}
        title="确认修改功能开关？"
        confirmLabel="确认保存"
        tone={turningOff.length ? "danger" : "primary"}
        onClose={() => setConfirm(false)}
        onConfirm={save}
      >
        {turningOff.length > 0 && (
          <Notice tone="warn">
            关闭「{turningOff.map((n) => labels[n]).join("」「")}」后，所有成员（管理员 Key
            除外）立即不能使用。
          </Notice>
        )}
        {turningOn.length > 0 && (
          <Notice tone="warn">
            开启「{turningOn.map((n) => labels[n]).join("」「")}」会允许成员使用消耗官方
            Anlas（付费点数）的功能，可能消耗上游账号的付费点数余额。
          </Notice>
        )}
      </ConfirmDialog>
    </Card>
  );
}
