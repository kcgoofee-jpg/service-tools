/** 操作日志：后台 / 系统 / Discord 成员做过的事，服务端分页（每页 30 条）。 */
import { useMemo } from "preact/hooks";
import { fullTime } from "../../../../lib/format";
import { Badge, cx, Skeleton } from "../../../../ui";
import { LoadError } from "../_kit";
import { Empty, NARROW, Pager, TableScroll, useMedia, usePageData } from "../members/shared";
import { actionKindGroups, actionLabel } from "./actionNames";
import { intParam, useQueryState } from "./query";
import type { ActionRow, ActionsResp } from "./types";

const ACTORS = [
  { id: "", label: "全部操作者" },
  { id: "admin", label: "后台" },
  { id: "system", label: "系统" },
  { id: "member", label: "Discord 成员" },
];

const Result = (props: { a: ActionRow }) =>
  props.a.ok ? <Badge tone="ok">成功</Badge> : <Badge tone="danger">失败</Badge>;

export function Actions() {
  const { query, set } = useQueryState();
  const narrow = useMedia(NARROW);
  const page = intParam(query.page, 1);
  const actor = ACTORS.some((a) => a.id === query.actor) ? (query.actor as string) : "";
  const kind = query.action ?? "";
  const qs = new URLSearchParams({ page: String(page) });
  if (actor) qs.set("actor", actor);
  if (kind) qs.set("action", kind);
  const { data, error, pending } = usePageData<ActionsResp>(`/admin/api/actions?${qs}`);
  const groups = useMemo(() => actionKindGroups(data?.action_kinds ?? []), [data?.action_kinds]);
  const rows = data?.actions ?? [];

  return (
    <div class="stack">
      <div class="xa-toolbar">
        <label class="lg-ctl">
          <span class="small muted">操作者</span>
          <select
            class="select"
            value={actor}
            onChange={(e) => set({ actor: e.currentTarget.value, page: null })}
          >
            {ACTORS.map((a) => (
              <option key={a.id} value={a.id}>
                {a.label}
              </option>
            ))}
          </select>
        </label>
        <label class="lg-ctl">
          <span class="small muted">操作类型</span>
          <select
            class="select"
            value={kind}
            onChange={(e) => set({ action: e.currentTarget.value, page: null })}
          >
            <option value="">全部操作</option>
            {groups.map((g) => (
              <optgroup key={g.title} label={g.title}>
                {g.options.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </optgroup>
            ))}
          </select>
        </label>
      </div>

      {error && !data ? (
        <LoadError error={error} />
      ) : !data ? (
        <Skeleton h="240px" />
      ) : (
        <div class={cx("card lg-listcard", pending && "xa-busy")} aria-busy={pending}>
          {rows.length === 0 ? (
            <Empty title="暂无记录" />
          ) : narrow ? (
            <ul class="lg-cards" aria-label="操作日志">
              {rows.map((a) => (
                <li key={a.id} class="lg-card">
                  <div class="lg-card__top">
                    <span class="small muted">{fullTime(a.ts)}</span>
                    <Result a={a} />
                  </div>
                  <div>
                    <b>{actionLabel(a.action)}</b>
                    <span class="small muted"> · {a.actor}</span>
                  </div>
                  {a.target && <div class="small">{a.target}</div>}
                  {a.detail && <div class="small muted">{a.detail}</div>}
                </li>
              ))}
            </ul>
          ) : (
            <TableScroll label="操作日志">
              <table class="table lg-table">
                <caption class="sr-only">操作日志</caption>
                <thead>
                  <tr>
                    <th scope="col">时间</th>
                    <th scope="col">操作者</th>
                    <th scope="col">操作</th>
                    <th scope="col">对象</th>
                    <th scope="col">详情</th>
                    <th scope="col">结果</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((a) => (
                    <tr key={a.id}>
                      <td class="nowrap" title={fullTime(a.ts)}>
                        {fullTime(a.ts)}
                      </td>
                      <td class="nowrap">{a.actor}</td>
                      <td class="nowrap">{actionLabel(a.action)}</td>
                      <td>{a.target}</td>
                      <td class="muted lg-note">{a.detail}</td>
                      <td>
                        <Result a={a} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </TableScroll>
          )}
          <Pager
            page={data.page}
            pages={data.pages}
            total={data.total}
            perPage={data.per_page}
            onPage={(p) => set({ page: p === 1 ? null : String(p) })}
          />
        </div>
      )}
    </div>
  );
}
