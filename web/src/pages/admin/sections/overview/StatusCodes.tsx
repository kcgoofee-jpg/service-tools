/** 状态码分布：网关→成员 / NovelAI→网关 两个视角，今日 / 1 小时 / 24 小时 / 7 天 四个时间范围。 */
import { useState } from "preact/hooks";
import { Badge, Card } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { Empty, Seg } from "../members/shared";
import type { StatusCodeRow, StatusCodesResp } from "./types";

type View = "gw" | "up";
type Range = "today" | "1" | "24" | "168";

const RANGES: Array<{ id: Range; label: string }> = [
  { id: "today", label: "今日" },
  { id: "1", label: "1 小时" },
  { id: "24", label: "24 小时" },
  { id: "168", label: "7 天" },
];

const tone = (code: number): "ok" | "warn" | "danger" | "info" | undefined => {
  if (!code) return undefined;
  const k = Math.floor(code / 100);
  return k === 2 ? "ok" : k === 3 ? "info" : k === 4 ? "warn" : "danger";
};

export const pctLabel = (c: Pick<StatusCodeRow, "n" | "pct">): string =>
  c.n > 0 && c.pct === 0 ? "<0.1%" : `${c.pct}%`;

function Rows(props: { rows: StatusCodeRow[] }) {
  if (!props.rows.length) return <Empty title="这段时间还没有请求" />;
  return (
    <ul class="ov-sc">
      {props.rows.map((c) => (
        <li key={c.code} class="ov-sc__row">
          <Badge {...(tone(c.code) ? { tone: tone(c.code) as "ok" } : {})}>{c.code || "未发出"}</Badge>
          <div class="ov-sc__main">
            <span class="tabular">{c.n.toLocaleString("zh-CN")} 次</span>
            <span class="small muted">{c.meaning}</span>
          </div>
          <span class="ov-sc__pct tabular">{pctLabel(c)}</span>
          <span class="ov-sc__bar" aria-hidden="true">
            <i
              class={`is-${tone(c.code) ?? "none"}`}
              style={{ width: `${c.n > 0 ? Math.max(1, c.pct) : 0}%`, minWidth: c.n > 0 ? "3px" : "0" }}
            />
          </span>
        </li>
      ))}
    </ul>
  );
}

/** 按时间范围分开取数（key 换了就重新挂载，切换时显示骨架而不是旧数据） */
function Body(props: { range: Range; view: View }) {
  const { data, error } = useAdmin<StatusCodesResp>(`/admin/api/status-codes?hours=${props.range}`);
  if (error && !data) return <LoadError error={error} />;
  if (!data) return <Loading rows={3} />;
  return <Rows rows={props.view === "up" ? (data.upstream ?? []) : data.codes} />;
}

export function StatusCodes() {
  const [view, setView] = useState<View>("gw");
  const [range, setRange] = useState<Range>("today");
  return (
    <Card
      title="状态码分布"
      sub={
        view === "gw"
          ? "成员接口（出图、查额度等）返回的 HTTP 状态码；这里的 429 / 402 是网关返回的"
          : "NovelAI 实际返回给网关的状态码；「未发出」= 网关自己拦下、排队超时或客户端断开，没发到 NovelAI"
      }
      class="ov-card"
    >
      <div class="xa-toolbar ov-sc__tools">
        <Seg<View>
          label="看哪一段"
          value={view}
          options={[
            { id: "gw", label: "网关→成员", title: "网关返回给成员客户端的" },
            { id: "up", label: "NovelAI→网关", title: "NovelAI 实际返回给网关的" },
          ]}
          onChange={setView}
        />
        <Seg<Range> label="时间范围" value={range} options={RANGES} onChange={setRange} />
      </div>
      <Body key={range} range={range} view={view} />
    </Card>
  );
}
