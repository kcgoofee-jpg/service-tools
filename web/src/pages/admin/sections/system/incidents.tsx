/** 状态页事件：按 Statuspage 标准流程发布（调查中 → 已确定 → 监控中 → 已解决），正文留空自动用标准句式。 */
import { useMemo, useState } from "preact/hooks";
import { fullTime } from "../../../../lib/format";
import { Badge, Button, Card, Notice } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import type { FieldSpec } from "./core";
import { ConfirmDialog, SelectField, TextAreaField, TextField, useForm, withTone } from "./forms";
import {
  COMPONENTS,
  CREATE_STATUSES,
  finalBody,
  IMPACT_LABEL,
  type Impact,
  type Incident,
  leakWarnings,
  nextStatus,
  STANDARD_BODY,
  STATUS_LABEL,
  type Status,
  UPDATE_STATUSES,
} from "./incidents-core";

const opt = (statuses: Status[]) => statuses.map((s) => ({ value: s, label: STATUS_LABEL[s] }));

const CREATE_SPECS: Record<string, FieldSpec> = {
  title: {
    kind: "text",
    label: "标题",
    min: 1,
    max: 200,
    hint: "受影响的功能 + 现象，例如「出图接口请求超时」「部分 V5 出图画质下降」。不写原因。",
  },
  impact: {
    kind: "select",
    label: "影响程度",
    options: (Object.keys(IMPACT_LABEL) as Impact[]).map((k) => ({ value: k, label: IMPACT_LABEL[k] })),
  },
  status: { kind: "select", label: "当前状态", options: opt(CREATE_STATUSES) },
  body: { kind: "text", label: "正文（可留空）", max: 2000 },
};
const CREATE_DEFAULTS = { title: "", impact: "minor", status: "investigating", body: "" };

const UPDATE_BODY_SPEC: FieldSpec = { kind: "text", label: "正文（可留空）", max: 2000 };

const impactTone = (i: string): "warn" | "danger" | "info" | undefined =>
  i === "minor" ? "warn" : i === "major" || i === "critical" ? "danger" : i === "none" ? "info" : undefined;

const BODY_HINT = (status: Status) =>
  `留空则公开页自动使用标准句式：「${STANDARD_BODY[status]}」。有话要说时只写成员能感知到的影响、时间段和进展；不写内部原因细节、数据量、版本号、配置名、哪个成员、哪把 Key。已解决时补一句影响时段。`;

function Warnings(props: { texts: string[] }) {
  const w = leakWarnings(...props.texts);
  if (!w.length) return null;
  return (
    <Notice tone="warn">
      这段文字里好像带了内部细节（{w.join("、")}
      ）。状态页是公开的，建议改成成员能感知到的影响和进展，不写原因和内部数据。
    </Notice>
  );
}

/* ---------------- 发布新事件 ---------------- */
function CreateForm() {
  const form = useForm(CREATE_SPECS, CREATE_DEFAULTS);
  const [comps, setComps] = useState<string[]>(["api"]);
  const [confirm, setConfirm] = useState(false);
  const status = String(form.raw("status")) as Status;
  const title = String(form.raw("title")).trim();
  const body = String(form.raw("body"));
  const ready = title.length > 0 && form.valid && comps.length > 0;
  const compLabels = COMPONENTS.filter((c) => comps.includes(c.key));
  const hidden = compLabels.filter((c) => !c.onPublicPage);
  const onlyHidden = compLabels.length > 0 && compLabels.every((c) => !c.onPublicPage);
  const toggle = (k: string) => setComps((c) => (c.includes(k) ? c.filter((x) => x !== k) : [...c, k]));

  return (
    <Card title="发布新事件" sub="事件会公开显示在状态页，每次更新带时间，最新的在上面。">
      <form
        class="stack-sm"
        {...form.formProps}
        onSubmit={(e) => {
          e.preventDefault();
          if (ready) setConfirm(true);
        }}
      >
        <TextField form={form} name="title" placeholder="例如：出图接口请求超时" />
        <div class="form-grid">
          <SelectField form={form} name="impact" />
          <SelectField form={form} name="status" />
        </div>
        <fieldset class="checks">
          <legend class="field__label">受影响的组件</legend>
          <div class="chips">
            {COMPONENTS.map((c) => (
              <label key={c.key} class="check">
                <input type="checkbox" checked={comps.includes(c.key)} onChange={() => toggle(c.key)} />
                <span>
                  {c.label}
                  {!c.onPublicPage && <span class="muted">（不显示在公开页）</span>}
                </span>
              </label>
            ))}
          </div>
          {comps.length === 0 && (
            <span class="field__error" role="alert">
              至少选一个组件
            </span>
          )}
        </fieldset>
        <TextAreaField form={form} name="body" rows={3} hint={BODY_HINT(status)} />
        <Warnings texts={[title, body]} />
        {onlyHidden && <Notice tone="warn">只选了不显示在公开页的组件：这个事件不会出现在状态页上。</Notice>}
        <div class="savebar">
          <Button type="submit" variant="primary" disabled={!ready}>
            发布事件
          </Button>
        </div>
      </form>
      <ConfirmDialog
        open={confirm}
        title="公开发布这个事件？"
        confirmLabel="发布到状态页"
        onClose={() => setConfirm(false)}
        onConfirm={async () => {
          const ok = await act(
            "POST",
            "/admin/api/incidents",
            { title, impact: form.raw("impact"), components: comps, status, body: body.trim() },
            "事件已发布",
          );
          if (ok) {
            form.reset();
            setComps(["api"]);
            setConfirm(false);
          }
        }}
      >
        <p>下面的内容会公开显示在状态页上，请确认没有内部细节：</p>
        <div class="derive">
          <div>
            <b>{title}</b>
          </div>
          <div>
            影响：{IMPACT_LABEL[String(form.raw("impact")) as Impact]} · 组件：
            {compLabels.map((c) => c.label).join("、")}
          </div>
          <div>
            <b>{STATUS_LABEL[status]}</b> - {finalBody(status, body)}
          </div>
        </div>
        {hidden.length > 0 && (
          <p class="muted small">「{hidden.map((c) => c.label).join("」「")}」不会显示在公开状态页。</p>
        )}
      </ConfirmDialog>
    </Card>
  );
}

/* ---------------- 单个事件 ---------------- */
function UpdateForm(props: { inc: Incident }) {
  const latest = props.inc.updates[0]?.status;
  const next = nextStatus(latest);
  const specs = useMemo<Record<string, FieldSpec>>(
    () => ({
      status: { kind: "select", label: "状态", options: opt(UPDATE_STATUSES) },
      body: UPDATE_BODY_SPEC,
    }),
    [],
  );
  const server = useMemo(() => ({ status: next, body: "" }), [next]);
  const form = useForm(specs, server);
  const [confirm, setConfirm] = useState(false);
  const status = String(form.raw("status")) as Status;
  const body = String(form.raw("body"));
  const url = `/admin/api/incidents/${props.inc.id}/updates`;
  return (
    <form
      class="stack-sm"
      {...form.formProps}
      onSubmit={(e) => {
        e.preventDefault();
        if (form.valid) setConfirm(true);
      }}
    >
      <div class="form-grid">
        <SelectField form={form} name="status" label="发布更新：状态" />
      </div>
      <TextAreaField form={form} name="body" rows={2} hint={BODY_HINT(status)} label="正文（可留空）" />
      <Warnings texts={[body]} />
      <div class="savebar">
        <Button type="submit" variant="primary" disabled={!form.valid}>
          发布更新
        </Button>
      </div>
      <ConfirmDialog
        open={confirm}
        title="公开发布这条更新？"
        confirmLabel="发布到状态页"
        onClose={() => setConfirm(false)}
        onConfirm={async () => {
          const ok = await act("POST", url, { status, body: body.trim() }, "更新已发布");
          if (ok) {
            form.reset();
            setConfirm(false);
          }
        }}
      >
        <p>「{props.inc.title}」将公开显示：</p>
        <div class="derive">
          <div>
            <b>{STATUS_LABEL[status]}</b> - {finalBody(status, body)}
          </div>
        </div>
      </ConfirmDialog>
    </form>
  );
}

function IncidentItem(props: { inc: Incident }) {
  const inc = props.inc;
  const open = inc.resolved_at == null;
  const names = COMPONENTS.filter((c) => inc.components.includes(c.key)).map((c) => c.label);
  return (
    <article class="incident" aria-label={inc.title}>
      <header class="incident__head">
        <h3 class="incident__title">{inc.title}</h3>
        <Badge {...withTone(impactTone(inc.impact))}>
          {IMPACT_LABEL[inc.impact as Impact] ?? inc.impact}
        </Badge>
        <Badge {...withTone(open ? "warn" : "ok")}>{open ? "进行中" : "已解决"}</Badge>
      </header>
      <p class="muted small">
        {names.join("、") || "—"} · 开始 {fullTime(inc.started_at)}
        {inc.resolved_at ? ` · 解决 ${fullTime(inc.resolved_at)}` : ""}
      </p>
      <ol class="incident__updates">
        {inc.updates.map((u, i) => (
          <li key={`${u.at}-${i}`}>
            <b>{STATUS_LABEL[u.status as Status] ?? u.status}</b> - {u.body}
            <span class="muted small"> · {fullTime(u.at)}</span>
          </li>
        ))}
      </ol>
      <details class="more" open={open}>
        <summary class="small">{open ? "发布更新" : "追加更新（会重新打开事件，除非选「已解决」）"}</summary>
        <div style={{ marginTop: "var(--sp-3)" }}>
          <UpdateForm inc={inc} />
        </div>
      </details>
    </article>
  );
}

export default function Incidents() {
  const { data, error, loading } = useAdmin<{ incidents: Incident[] }>("/admin/api/incidents");
  const list = data?.incidents ?? [];
  const sorted = [...list].sort(
    (a, b) => Number(a.resolved_at != null) - Number(b.resolved_at != null) || b.started_at - a.started_at,
  );
  const recent = sorted.slice(0, 8);
  const older = sorted.slice(8);
  return (
    <>
      <CreateForm />
      <Card
        title="事件记录"
        sub="未解决的在前。发出去的内容不能修改或删除，写错了请发一条「更新」更正。公开状态页缓存约 60 秒。"
      >
        {loading && !data && <Loading rows={2} />}
        {!data && error && <LoadError error={error} />}
        {data && !list.length && <p class="muted small">还没有发布过事件。</p>}
        <div class="stack">
          {recent.map((i) => (
            <IncidentItem key={i.id} inc={i} />
          ))}
        </div>
        {older.length > 0 && (
          <details class="more" style={{ marginTop: "var(--sp-4)" }}>
            <summary class="small muted">更早的 {older.length} 个事件</summary>
            <div class="stack" style={{ marginTop: "var(--sp-3)" }}>
              {older.map((i) => (
                <IncidentItem key={i.id} inc={i} />
              ))}
            </div>
          </details>
        )}
        <details class="more" style={{ marginTop: "var(--sp-4)" }}>
          <summary class="small muted">标准句式（正文留空时使用）</summary>
          <ul class="std-list">
            {UPDATE_STATUSES.map((s) => (
              <li key={s} class="small">
                <b>{STATUS_LABEL[s]}</b> - {STANDARD_BODY[s]}
              </li>
            ))}
          </ul>
        </details>
      </Card>
    </>
  );
}
