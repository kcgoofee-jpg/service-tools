/** 用量日志：每次请求一行；按成员 / 功能 / 错误编号筛选，服务端分页，可导出客户端日志。 */
import { useEffect, useState } from "preact/hooks";
import { fullTime, num } from "../../../../lib/format";
import { Badge, cx, SearchSelect, Skeleton } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, TableWrap } from "../_kit";
import { Empty, NARROW, Pager, useDebounced, useMedia, usePageData } from "../members/shared";
import { FEATURES, kindLabel, shortModel, timing } from "./format";
import { intParam, useQueryState } from "./query";
import type { LogsResp, UsageLog } from "./types";

const SIZES = [20, 50, 100];

function StatusBadge(props: { l: UsageLog }) {
  const { l } = props;
  return (
    <span class="lg-status">
      {l.status === "ok" ? (
        <Badge tone="ok">成功</Badge>
      ) : l.status === "rejected" ? (
        <Badge tone="warn">拒绝</Badge>
      ) : (
        <Badge tone="danger">错误</Badge>
      )}
      {l.up_status ? (
        <span class="small muted" title="NovelAI 返回给网关的状态码">
          上游 {l.up_status}
        </span>
      ) : l.status !== "ok" ? (
        <span class="small muted" title="网关自己拦下，没发到 NovelAI">
          网关拦下
        </span>
      ) : null}
    </span>
  );
}

function Timing(props: { l: UsageLog }) {
  const t = timing(props.l.wait_ms, props.l.dur_ms);
  if (!t.gen && !t.wait) return <span class="muted">—</span>;
  return (
    <span class="lg-timing" title="等待：收到请求到发往上游（排队 + 冷却）；生成：上游处理时间">
      {t.gen && <span class="nowrap">生成 {t.gen}</span>}
      {t.wait && <span class="nowrap muted">等 {t.wait}</span>}
    </span>
  );
}

function Cost(props: { l: UsageLog }) {
  const { l } = props;
  const parts = [
    l.images ? <span class="nowrap">{l.images} 张</span> : null,
    l.anlas ? <span class="nowrap">{+Number(l.anlas).toFixed(2)} Anlas</span> : null,
    l.unconfirmed_anlas > 0 ? (
      <Badge tone="warn" title="失败请求可能产生的费用，实际扣款尚未确认">
        ≈ {l.unconfirmed_anlas}
      </Badge>
    ) : null,
    l.tokens ? <span class="nowrap">{num(l.tokens)} tokens</span> : null,
  ].filter(Boolean);
  if (!parts.length) return <span class="muted">—</span>;
  return <span class="lg-cost">{parts}</span>;
}

function Note(props: { l: UsageLog; onRid: (rid: string) => void }) {
  const { l } = props;
  return (
    <>
      {l.detail && <span class="muted">{l.detail}</span>}
      {l.rid && (
        <button
          type="button"
          class="lg-rid mono"
          title="错误编号（响应头 X-Request-Id）；点击只看这一条"
          onClick={() => props.onRid(l.rid)}
        >
          #{l.rid}
        </button>
      )}
    </>
  );
}

export function Usage() {
  const { query, set } = useQueryState();
  const narrow = useMedia(NARROW);
  const keys = useAdmin<{ keys: Array<{ id: number; name: string }> }>("/admin/api/keys");

  const page = intParam(query.page, 1);
  const per = intParam(query.per, 20, SIZES);
  const keyId = /^\d+$/.test(query.key ?? "") ? (query.key as string) : "";
  const feature = FEATURES.some((f) => f.id === query.feature) ? (query.feature as string) : "";
  const rid = (query.rid ?? "").toLowerCase().replace(/[^0-9a-f]/g, "");
  // 隐藏测试 Key 默认开；指定了成员或错误编号时不再隐藏（要看的就是那一条）
  const hideTest = query.hide_test !== "0" && !keyId && !rid;

  const [ridInput, setRidInput] = useState(rid);
  useEffect(() => setRidInput(rid), [rid]);
  const dRid = useDebounced(ridInput, 350);
  useEffect(() => {
    const clean = dRid
      .trim()
      .toLowerCase()
      .replace(/[^0-9a-f]/g, "");
    if (clean !== rid) set({ rid: clean, page: null });
  }, [dRid]);

  const qs = new URLSearchParams();
  if (keyId) qs.set("key_id", keyId);
  if (rid) qs.set("rid", rid);
  if (feature) qs.set("feature", feature);
  if (hideTest) qs.set("hide_test", "true");
  qs.set("per_page", String(per));
  qs.set("page", String(page));
  const { data, error, pending } = usePageData<LogsResp>(`/admin/api/logs?${qs}`);

  const filters = (
    <div class="xa-toolbar lg-filters-in">
      <div class="field-inline">
        <span class="field-inline__k">成员</span>
        <SearchSelect
          label="成员"
          value={keyId}
          allLabel="全部 Key"
          options={(keys.data?.keys ?? []).map((k) => ({ value: String(k.id), label: k.name }))}
          onChange={(v) => set({ key: v, page: null })}
        />
      </div>
      <label class="field-inline">
        <span class="field-inline__k">功能</span>
        <select
          class="select"
          value={feature}
          onChange={(e) => set({ feature: e.currentTarget.value, page: null })}
        >
          <option value="">全部功能</option>
          {FEATURES.map((f) => (
            <option key={f.id} value={f.id}>
              {f.short}
            </option>
          ))}
        </select>
      </label>
      <label class="field-inline">
        <span class="field-inline__k">错误编号</span>
        <input
          class="input mono"
          type="search"
          placeholder="例如 3f9a0c"
          value={ridInput}
          onInput={(e) => setRidInput(e.currentTarget.value)}
        />
      </label>
      <label class="xa-check lg-check">
        <input
          type="checkbox"
          checked={hideTest}
          disabled={!!keyId || !!rid}
          onChange={(e) => set({ hide_test: e.currentTarget.checked ? null : "0", page: null })}
        />
        <span>隐藏测试 Key</span>
      </label>
    </div>
  );

  const rows = data?.logs ?? [];
  return (
    <div class="stack">
      <div class="lg-bar">
        {narrow ? (
          <details class="lg-filters">
            <summary class="btn">筛选</summary>
            {filters}
          </details>
        ) : (
          filters
        )}
      </div>

      {error && !data ? (
        <LoadError error={error} />
      ) : !data ? (
        <Skeleton h="240px" />
      ) : (
        <div class={cx("card lg-listcard", pending && "xa-busy")} aria-busy={pending}>
          {rows.length === 0 ? (
            <Empty title="暂无记录">换个筛选条件试试。</Empty>
          ) : narrow ? (
            <ul class="lg-cards" aria-label="用量日志">
              {rows.map((l) => (
                <li key={l.id} class="lg-card">
                  <div class="lg-card__top">
                    <span class="small muted" title={fullTime(l.ts)}>
                      {fullTime(l.ts)}
                    </span>
                    <StatusBadge l={l} />
                  </div>
                  <div>
                    <b>{l.key_name}</b>
                  </div>
                  <div class="small">
                    {kindLabel(l.kind)}
                    {l.model ? ` · ${shortModel(l.model)}` : ""}
                  </div>
                  <div class="lg-card__meta small">
                    <Timing l={l} />
                    <Cost l={l} />
                  </div>
                  {(l.detail || l.rid) && (
                    <div class="small">
                      <Note l={l} onRid={(r) => set({ rid: r, page: null })} />
                    </div>
                  )}
                </li>
              ))}
            </ul>
          ) : (
            <TableWrap label="用量日志">
              <table class="table lg-table">
                <caption class="sr-only">用量日志</caption>
                <thead>
                  <tr>
                    <th scope="col">时间</th>
                    <th scope="col">Key</th>
                    <th scope="col">类型</th>
                    <th scope="col">模型</th>
                    <th scope="col">状态</th>
                    <th scope="col">耗时</th>
                    <th scope="col">消耗</th>
                    <th scope="col">备注</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((l) => (
                    <tr key={l.id}>
                      <td class="nowrap" title={fullTime(l.ts)}>
                        {fullTime(l.ts)}
                      </td>
                      <td>{l.key_name}</td>
                      <td class="nowrap">{kindLabel(l.kind)}</td>
                      <td class="nowrap" title={l.model}>
                        {shortModel(l.model)}
                      </td>
                      <td>
                        <StatusBadge l={l} />
                      </td>
                      <td>
                        <Timing l={l} />
                      </td>
                      <td>
                        <Cost l={l} />
                      </td>
                      <td class="lg-note">
                        <Note l={l} onRid={(r) => set({ rid: r, page: null })} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </TableWrap>
          )}
          <Pager
            page={data.page}
            pages={data.pages}
            total={data.total}
            perPage={data.per_page}
            sizes={SIZES}
            onPage={(p) => set({ page: p === 1 ? null : String(p) })}
            onSize={(n) => set({ per: n === 20 ? null : String(n), page: null })}
          />
        </div>
      )}
    </div>
  );
}
