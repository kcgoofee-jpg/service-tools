/** 动态额度：当前结果 + 推算过程 + 最近 7 天的 7 点调整记录 + 算法参数。 */
import { useMemo, useState } from "preact/hooks";
import { fullTime } from "../../../../lib/format";
import { Badge, Button, Card, Notice, toast } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { actOrError, ConfirmDialog, LoadError, Loading, Stat, TableWrap } from "../_kit";
import { capText } from "../system/core";
import { Derive, NumField, SaveBar, Switch, useForm } from "../system/forms";
import {
  ALGO_SPECS,
  deriveV5,
  imagesPerPercent,
  MANUAL_N_SPEC,
  type QuotaAlgo,
  type QuotaReview,
  type RateInfo,
} from "./core";

const URL = "/admin/api/quota-algo";
const SPECS = { ...MANUAL_N_SPEC, ...ALGO_SPECS };

const f1 = (x: number | null | undefined): string => (x == null ? "—" : x.toFixed(1));

function RateLine(props: { info: RateInfo }) {
  const r = props.info;
  return (
    <div>
      恢复速度：接口读数 <b>{f1(r.api)}</b> · 实测 <b>{f1(r.measured)}</b> · 采用 <b>{f1(r.used)}</b>%/天（
      {r.status || "—"}）
      {r.quiet && (
        <>
          {" "}
          · 安静时段直接量到 {r.quiet.rate}%/天（累计 {r.quiet.hours} 小时）
        </>
      )}
      {(r.notes ?? []).length > 0 && <> · {(r.notes ?? []).join("；")}</>}
    </div>
  );
}

function History(props: { rows: QuotaReview[] }) {
  const rows = props.rows.slice(-7).reverse();
  if (!rows.length) return <p class="muted small">每天 7 点的调整记录会出现在这里。</p>;
  const all = props.rows;
  return (
    <TableWrap label="最近 7 个服务日的额度调整记录">
      <table class="table">
        <caption class="sr-only">最近 7 个服务日的额度调整记录</caption>
        <thead>
          <tr>
            <th scope="col">日期</th>
            <th scope="col" class="num">
              用量 / 账号日上限
            </th>
            <th scope="col" class="num">
              7 点 V5 剩余
            </th>
            <th scope="col" class="num">
              上限拦截小时数
            </th>
            <th scope="col" class="num">
              顶格人数
            </th>
            <th scope="col" class="num">
              每人（昨 → 今）
            </th>
            <th scope="col">理由</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((h) => {
            const idx = all.indexOf(h);
            const prev = idx > 0 ? all[idx - 1] : undefined;
            const delta = h.v5_pct != null && prev?.v5_pct != null ? h.v5_pct - prev.v5_pct : null;
            return (
              <tr key={h.day}>
                <td class="tnum nowrap">{h.day}</td>
                <td class="num">
                  {h.used} / {h.cap}
                </td>
                <td class="num">
                  {h.v5_pct == null ? "—" : `${Math.round(h.v5_pct)}%`}
                  {delta != null && (
                    <span class="muted">
                      {" "}
                      {delta >= 0 ? "+" : ""}
                      {delta.toFixed(1)}
                    </span>
                  )}
                </td>
                <td class="num">{h.hourly_blocks}</td>
                <td class="num">{h.ceiling_hits}</td>
                <td class="num">
                  {h.from[0]} → {h.to[0]}
                </td>
                <td class="muted">{(h.reasons ?? []).join("；")}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </TableWrap>
  );
}

export default function QuotaCard() {
  const { data, error, loading } = useAdmin<QuotaAlgo>(URL);
  const [confirmOff, setConfirmOff] = useState(false);
  const [saving, setSaving] = useState(false);
  const [running, setRunning] = useState(false);

  const server = useMemo(() => {
    if (!data) return null;
    return { ...data.params, quota_target_avg: data.last.ceiling ?? data.params.quota_target_avg };
  }, [data]);
  const form = useForm(SPECS, server);

  if (loading && !data)
    return (
      <Card title="动态额度">
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title="动态额度">
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );

  const autoOn = data.params.quota_auto_enabled !== 0;
  const l = data.last;
  const v5 = l.v5;
  const per = imagesPerPercent(data);
  const review = l.review ?? data.history[data.history.length - 1];
  const d = v5 ? deriveV5(v5, per.value) : null;
  const rc = v5?.rate_check ?? v5?.rate_info;

  const setAuto = (on: boolean) =>
    actOrError(
      "PUT",
      URL,
      { quota_auto_enabled: on ? 1 : 0 },
      on ? "动态额度已开启并重算" : "动态额度已关闭",
    );
  const save = async () => {
    setSaving(true);
    const ok = await act("PUT", URL, form.changes(), "已保存，额度已按新参数重算");
    setSaving(false);
    if (ok) form.commit();
  };

  return (
    <Card
      title="动态额度"
      sub={`每 10 分钟重算，每个服务日 7 点按昨天的用量微调${l.updated_at ? ` · 更新于 ${fullTime(l.updated_at)}` : ""}`}
      actions={
        <div class="actions">
          <Button
            disabled={!autoOn || running}
            onClick={async () => {
              setRunning(true);
              await act("PUT", URL, {}, "额度已重新计算");
              setRunning(false);
            }}
          >
            {running ? "计算中…" : "立即重算"}
          </Button>
          <span class="actions">
            <span id="qa-auto-label" class="small">
              动态额度
            </span>
            <Switch
              checked={autoOn}
              labelledBy="qa-auto-label"
              onChange={(on) =>
                on
                  ? void setAuto(true).then((err) => err && toast(err, { error: true }))
                  : setConfirmOff(true)
              }
            />
          </span>
        </div>
      }
    >
      {!autoOn && (
        <Notice tone="warn">
          动态额度已关闭：每把 Key
          的额度不会再自动跟随上游余量与用量调整，下面的数字是关闭前的最后一次结果。需要重新由算法分配时，把右上角的开关打开。
        </Notice>
      )}
      <div class="stack-sm" style={{ marginTop: autoOn ? 0 : "var(--sp-4)" }}>
        <div class="stats">
          <Stat label="V4.5 每人每天" value={l.ceiling ?? "—"} hint={`由算法管理 ${l.members ?? 0} 人`} />
          <Stat
            label="V5 每人每天"
            value={v5 ? v5.each : "—"}
            hint={v5 ? `全站 ${capText(v5.global, " 张/天")}` : undefined}
          />
          <Stat
            label="账号 V5 剩余"
            value={v5?.percent == null ? "—" : `${Math.round(v5.percent)}%`}
            hint={v5 ? `恢复 ${v5.rate}%/天` : undefined}
          />
        </div>

        {d && v5 && (
          <Derive>
            <div>
              全站 V5 = 恢复 <b>{d.rate}%/天</b> × <b>{d.perPercent} 张/1%</b> = <b>{d.calc} 张</b>
              {!d.matches && (
                <>
                  ；实际采用 <b>{d.global}</b>（沿用 7 点定下的当天方案）
                </>
              )}
            </div>
            {rc && <RateLine info={rc} />}
            <div>
              每人 V5 = 全站 <b>{d.global}</b> ÷ <b>{d.people}</b> 人（前 3 天用过 V5 的人数，至少按 10
              人算），至少 10 张 → <b>{d.each} 张</b>
              {!d.eachMatches && (
                <>
                  ；实际采用 <b>{v5.each}</b>
                </>
              )}
            </div>
            {v5.oversell && (
              <div>
                每人 {v5.oversell.each} × {v5.oversell.members} 人 = {v5.oversell.each * v5.oversell.members}{" "}
                张，超过全站 {v5.oversell.global} 张：额度是个人上限，全站先到先得。
              </div>
            )}
            <div class="muted">
              每 1% ≈ {per.value} 张：官方 100% ≈ 1730 张（按 23 步估算），折成成员常用的 28 步
              {per.fromApi ? "（取自动态额度接口）" : "（接口没给出，用默认值）"}。
            </div>
          </Derive>
        )}

        {review && (
          <Derive title={`V4.5 每人额度 · ${review.day} 的调整`}>
            {review.budget != null && (
              <div>
                可分 = 账号日上限 <b>{review.cap}</b> − 全站 V5 <b>{review.cap - review.budget}</b> ={" "}
                <b>{review.budget} 张</b>， 按昨天各人的用量做最大最小公平分配。
              </div>
            )}
            <div>
              {review.from[0] === review.to[0]
                ? `每人 ${review.to[0]} 张，不变`
                : `每人 ${review.from[0]} → ${review.to[0]} 张`}
              ：{(review.reasons ?? []).join("；")}
            </div>
            <div class="muted">
              昨天用量 {review.used} 张 · 顶格 {review.ceiling_hits} 人 · 数据覆盖{" "}
              {review.coverage_hours ?? "—"} 小时
            </div>
          </Derive>
        )}

        <History rows={data.history} />

        <form
          class="form-section"
          {...form.formProps}
          onSubmit={(e) => {
            e.preventDefault();
          }}
        >
          <h3 class="form-section__title">算法参数</h3>
          <div class="form-grid">
            <NumField form={form} name="quota_target_avg" />
            <NumField form={form} name="quota_step" />
            <NumField form={form} name="quota_ceiling_max" />
            <NumField form={form} name="quota_base_min_fair" />
          </div>
          <SaveBar form={form} onSave={save} saving={saving} />
        </form>
      </div>

      <ConfirmDialog
        open={confirmOff}
        title="关闭动态额度？"
        confirmText="关闭"
        danger
        onClose={() => setConfirmOff(false)}
        onConfirm={() => setAuto(false)}
      >
        <p>关闭后，每把 Key 的额度、领 Key 的默认额度和全站 V5 日限都不再由算法更新，需要站长手动维护。</p>
        <Badge tone="warn">已经分配出去的额度保持不变</Badge>
      </ConfirmDialog>
    </Card>
  );
}
