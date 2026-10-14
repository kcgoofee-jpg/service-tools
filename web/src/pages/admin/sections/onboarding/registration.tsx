/** 自助领 Key：开放开关、名额、有效期、新成员默认额度与功能、限定身份组。 */
import { useMemo, useState } from "preact/hooks";
import { Badge, Button, Card, Meter, Notice, toast } from "../../../../ui";
import { act } from "../../Admin";
import {
  ConfirmDialog,
  Derive,
  NumField,
  SaveBar,
  SelectField,
  SwitchRow,
  TextField,
  useForm,
  withTone,
} from "../system/forms";
import { FEATURE_PREFIX, type Ops, regBody, regServer, regSpecs, roleProblem, v5Need } from "./core";

const URL = "/admin/api/ops/registration";

export default function RegistrationCard(props: { ops: Ops; algoOn: boolean }) {
  const r = props.ops.registration;
  const labels = props.ops.features.labels;
  const names = useMemo(() => Object.keys(labels), [labels]);
  const specs = useMemo(() => regSpecs(labels), [labels]);
  const server = useMemo(() => regServer(r, labels), [r, labels]);
  const form = useForm(specs, server);
  const [saving, setSaving] = useState(false);
  const [toggleTo, setToggleTo] = useState<boolean | null>(null);
  const [pendingSave, setPendingSave] = useState(false);

  const seats = Number(form.raw("max_users"));
  const per = Number(form.raw("daily_v5"));
  const scope = String(form.raw("image_scope"));
  const need = v5Need(Number.isFinite(seats) ? seats : 0, Number.isFinite(per) ? per : 0, scope);
  const glob = r.v5_capacity.global;
  const short = need > 0 && glob > 0 && need > glob;
  const roleErr = roleProblem(String(form.raw("role_guild")), String(form.raw("role_id")));
  const pct = r.max_users > 0 ? Math.min(100, (r.active / r.max_users) * 100) : 0;
  const customFeatures = form.raw("features_mode") === "custom";
  const algoHint = props.algoOn
    ? "由「额度与保护 › 动态额度」自动设置（每 10 分钟重算），关闭动态额度后才能手改"
    : undefined;

  const send = async (notify: boolean) => {
    const body = regBody(form.changes(), form.raw as (n: string) => string | boolean, names, notify);
    setSaving(true);
    const ok = await act("PUT", URL, body, "领 Key 设置已保存");
    setSaving(false);
    if (ok) {
      form.commit();
      setPendingSave(false);
    }
  };
  const trySave = () => {
    if (roleErr) {
      toast(roleErr, { error: true });
      return;
    }
    // 名额上限变化会在成员公告频道发名额公告：让站长选发不发
    if ("max_users" in form.changes()) setPendingSave(true);
    else void send(true);
  };
  const setOpen = async (open: boolean, notify: boolean) => {
    const ok = await act(
      "PUT",
      URL,
      { open, ...(notify ? {} : { notify: false }) },
      open
        ? notify
          ? "已开放领 Key（已公告）"
          : "已开放领 Key（未公告）"
        : notify
          ? "已暂停领 Key（已公告）"
          : "已暂停领 Key（未公告）",
    );
    if (ok) setToggleTo(null);
  };

  return (
    <Card
      title="自助领 Key"
      sub="Discord /register 自助领 Key 的开放状态、名额限制与新成员默认权限。机器人下一条命令即生效。"
      actions={
        <Badge {...withTone(r.open ? "ok" : "warn")} dot>
          {r.open ? "可领取" : "未开放"}
        </Badge>
      }
    >
      <div class="stack-sm">
        {!r.configured && <Notice>Discord 机器人尚未配置：以下设置会保存，机器人启用后才生效。</Notice>}
        {r.v5_capacity.short && <Notice tone="warn">{r.v5_capacity.message}</Notice>}
        <SwitchRow
          title="开放自助领取"
          desc={`${r.open ? "成员可通过 Discord /register 自助领取 Key" : "新成员暂时无法领取 Key"}${r.reset_at ? ` · 重置时间 ${r.reset_at}` : ""}`}
          checked={r.open}
          onChange={(v) => setToggleTo(v)}
        />
        <Meter
          label="名额"
          value={r.active}
          max={r.max_users}
          text={`${r.active} / ${r.max_users > 0 ? r.max_users : "不限"}`}
          {...(r.max_users > 0 && r.active >= r.max_users
            ? { tone: "danger" as const }
            : pct >= 80
              ? { tone: "warn" as const }
              : {})}
          hint="已领取并绑定 Discord 的成员数 / 名额上限"
        />

        <form class="stack-sm" {...form.formProps} onSubmit={(e) => e.preventDefault()}>
          <div class="form-grid">
            <NumField form={form} name="max_users" />
            <NumField form={form} name="expires_days" />
            <NumField form={form} name="daily_images" disabled={props.algoOn} hint={algoHint} />
            <NumField form={form} name="daily_v5" disabled={props.algoOn} hint={algoHint} />
            <SelectField
              form={form}
              name="image_scope"
              disabled={props.algoOn}
              hint={algoHint ?? "新成员默认可用的模型范围"}
            />
          </div>
          {scope === "all" && (
            <Derive title="V5 名额推算" {...(short ? { tone: "warn" as const } : {})}>
              {need > 0 ? (
                <div>
                  名额 <b>{seats}</b> × 每人 V5 <b>{per}</b> = <b>{need}</b> 张/天；全站 V5 日限{" "}
                  <b>{glob > 0 ? glob : "不限"}</b>
                  {short && <>：超出 {need - glob} 张，先用的人用满后，后面的人当天没有 V5</>}
                </div>
              ) : (
                <div>名额不限或每人 V5 不限：无法推算总需求。</div>
              )}
            </Derive>
          )}

          <fieldset class="checks">
            <legend class="form-section__title">限定身份组</legend>
            <p class="muted small">只有在指定服务器里拥有指定身份组的人能领 Key；留空 = 不限定。</p>
            <div class="form-grid">
              <TextField form={form} name="role_guild" inputMode="numeric" />
              <TextField form={form} name="role_id" inputMode="numeric" />
              <TextField form={form} name="role_note" />
            </div>
            {roleErr && (form.isDirty("role_guild") || form.isDirty("role_id")) && (
              <span class="field__error" role="alert">
                {roleErr}
              </span>
            )}
          </fieldset>

          <fieldset class="checks">
            <legend class="form-section__title">新成员默认开通的功能</legend>
            <div class="form-grid">
              <SelectField form={form} name="features_mode" label="范围" />
            </div>
            {customFeatures && (
              <>
                <div class="actions">
                  <Button
                    size="sm"
                    onClick={() => {
                      for (const n of names) form.set(FEATURE_PREFIX + n, true);
                    }}
                  >
                    全选
                  </Button>
                  <Button
                    size="sm"
                    onClick={() => {
                      for (const n of names) form.set(FEATURE_PREFIX + n, false);
                    }}
                  >
                    清空
                  </Button>
                </div>
                <div class="chips">
                  {names.map((n) => (
                    <label class="check" key={n}>
                      <input
                        type="checkbox"
                        checked={form.raw(FEATURE_PREFIX + n) === true}
                        onChange={(e) => form.set(FEATURE_PREFIX + n, (e.target as HTMLInputElement).checked)}
                      />
                      <span>{labels[n]}</span>
                    </label>
                  ))}
                </div>
              </>
            )}
          </fieldset>
          <SaveBar form={form} onSave={trySave} saving={saving} />
        </form>
      </div>

      <ConfirmDialog
        open={toggleTo !== null}
        title={toggleTo ? "开放自助领 Key？" : "暂停自助领 Key？"}
        confirmLabel={toggleTo ? "开放并公告" : "暂停并公告"}
        tone="primary"
        onClose={() => setToggleTo(null)}
        extra={
          <Button onClick={() => void setOpen(toggleTo === true, false)}>
            {toggleTo ? "仅开放，不公告" : "仅暂停，不公告"}
          </Button>
        }
        onConfirm={() => setOpen(toggleTo === true, true)}
      >
        <p>
          {toggleTo
            ? "开放后成员可以用 /register 自助领取。会在成员公告频道发布一条名额公告，也可以选择只开放、不公告。"
            : "暂停后新成员无法领取，已领取的成员不受影响。会在成员公告频道发布一条公告，也可以选择只暂停、不公告。"}
        </p>
      </ConfirmDialog>
      <ConfirmDialog
        open={pendingSave}
        title="名额上限有变化"
        confirmLabel="保存并公告"
        tone="primary"
        onClose={() => setPendingSave(false)}
        extra={<Button onClick={() => void send(false)}>仅保存，不公告</Button>}
        onConfirm={() => send(true)}
      >
        <p>
          名额上限从 {r.max_users} 改为 {String(form.raw("max_users"))}。要在成员公告频道发布名额公告吗？
        </p>
      </ConfirmDialog>
    </Card>
  );
}
