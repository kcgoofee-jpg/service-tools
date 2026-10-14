import { useState } from "preact/hooks";
import { fullTime, shortTime } from "../../../../lib/format";
import { Badge, Card } from "../../../../ui";
import { act } from "../../Admin";
import { Switch } from "../system/forms";
import { type Check, LEVEL_TEXT, type Mod, moduleState, runCounts } from "./core";

const STATE_BADGE = {
  off: { text: "已关闭", tone: undefined },
  ok: { text: "正常", tone: "ok" },
  warn: { text: "注意", tone: "warn" },
  bad: { text: "有问题", tone: "danger" },
} as const;

function RunStrip(props: { mod: Mod }) {
  const runs = props.mod.last.runs ?? [];
  if (!runs.length) return null;
  const c = runCounts(runs);
  return (
    <div
      class="mod-runs"
      role="img"
      aria-label={`最近 ${runs.length} 轮（每 10 分钟一轮）：正常 ${c.ok}，注意 ${c.warn}，有问题 ${c.bad}`}
    >
      {runs.map((r, i) => (
        <i
          key={i}
          class={`mod-run mod-run--${r.level || "na"}`}
          title={`${shortTime(r.at)} · ${LEVEL_TEXT[r.level] ?? ""}`}
        />
      ))}
    </div>
  );
}

function CheckRow(props: { check: Check }) {
  const c = props.check;
  return (
    <div class="mod-check">
      <div class="mod-check__head">
        <Badge tone={c.level === "bad" ? "danger" : "warn"}>{LEVEL_TEXT[c.level] ?? c.level}</Badge>
        <b>{c.name}</b>
      </div>
      <p class="muted small">{c.detail}</p>
      {(c.evidence ?? []).length > 0 && (
        <details class="mod-ev">
          <summary class="small">查看判定证据（{c.evidence?.length} 条）</summary>
          <ul>
            {c.evidence?.map((e, i) => (
              <li key={i} class="small">
                {e}
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}

export default function ModuleCard(props: { mod: Mod }) {
  const m = props.mod;
  const [pending, setPending] = useState(false);
  const state = moduleState(m);
  const badge = STATE_BADGE[state];
  const checks = m.last.checks ?? [];
  const hot = checks.filter((c) => c.level === "bad" || c.level === "warn");
  const calm = checks.filter((c) => !hot.includes(c));
  const summary = !m.enabled
    ? "已关闭"
    : m.last.ok === false
      ? `上次运行出错：${m.last.error}`
      : m.last.summary || ((m.last.runs ?? []).length ? "" : "等待第一轮（每 10 分钟）");
  const titleId = `mod-${m.name}-t`;

  return (
    <Card
      class={m.enabled ? "" : "mod-off"}
      title={
        <span class="mod-title">
          <span id={titleId}>{m.title}</span>
          <Badge {...(badge.tone ? { tone: badge.tone } : {})}>{badge.text}</Badge>
        </span>
      }
      sub={m.question}
      actions={
        <Switch
          checked={m.enabled}
          disabled={pending}
          labelledBy={titleId}
          onChange={async (on) => {
            setPending(true);
            await act(
              "PUT",
              `/admin/api/modules/${encodeURIComponent(m.name)}`,
              { enabled: on },
              on ? `已启用「${m.title}」` : `已关闭「${m.title}」`,
            );
            setPending(false);
          }}
        />
      }
    >
      <div class="stack-sm">
        {m.hard_note && <p class="muted small">{m.hard_note}</p>}
        <RunStrip mod={m} />
        {summary && (
          <p class={m.last.ok === false ? "mod-err small" : "small"}>
            {summary}
            {m.last.at ? <span class="muted"> · {fullTime(m.last.at)}</span> : null}
          </p>
        )}
        {hot.map((c) => (
          <CheckRow key={c.name} check={c} />
        ))}
        {calm.length > 0 && (
          <ul class="mod-calm" aria-label="通过的校验">
            {calm.map((c) => (
              <li key={c.name} title={c.detail}>
                <Badge {...(c.level === "ok" ? { tone: "ok" as const } : {})}>{c.name}</Badge>
              </li>
            ))}
          </ul>
        )}
        {m.params.length > 0 && (
          <dl class="kv">
            {m.params.map((p) => (
              <div class="kv__row" key={p.name}>
                <dt>{p.name}</dt>
                <dd>
                  {p.value != null && p.value !== "" ? String(p.value) : <span class="muted">—</span>}
                  {p.note && <span class="muted">（{p.note}）</span>}
                </dd>
              </div>
            ))}
          </dl>
        )}
      </div>
    </Card>
  );
}
