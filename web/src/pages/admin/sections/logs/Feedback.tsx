/** 成员反馈：Discord /反馈 弹窗收集的 5 道单选题。汇总数字 + 明细表，可搜索、按评价筛选、导出 CSV。 */
import { Download } from "lucide-preact";
import { useState } from "preact/hooks";
import { fullTime } from "../../../../lib/format";
import { Badge, Button, Skeleton } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Stat } from "../_kit";
import { downloadFile, Empty, NARROW, TableScroll, useDebounced, useMedia } from "../members/shared";
import { feedbackStats, filterFeedback, type ScoreFilter, scoreTone } from "./feedbackStats";
import { useQueryState } from "./query";
import type { FeedbackItem, FeedbackResp } from "./types";

const PAGE = 50;

function Answer(props: { q: string; v: string | undefined }) {
  const { q, v } = props;
  if (!v) return <span class="muted">—</span>;
  if (q === "score") {
    const tone = scoreTone(v);
    return <Badge {...(tone ? { tone } : {})}>{v}</Badge>;
  }
  if (q === "pain" && v === "出图失败或报错") return <Badge tone="danger">{v}</Badge>;
  if (q === "pain" && v.startsWith("排队太久")) return <Badge tone="warn">{v}</Badge>;
  return <>{v}</>;
}

function Who(props: { it: FeedbackItem }) {
  const { it } = props;
  const label = it.key_name || it.username || it.discord_id || "—";
  return it.key_id ? (
    <a href={`/admin/logs/usage?key=${it.key_id}`} title="查看这位成员的用量日志">
      {label}
    </a>
  ) : (
    label
  );
}

export function Feedback() {
  const { query, set } = useQueryState();
  const narrow = useMedia(NARROW);
  const { data, error } = useAdmin<FeedbackResp>("/admin/api/feedback");
  const score: ScoreFilter = query.score === "good" || query.score === "bad" ? query.score : "";
  const [search, setSearch] = useState("");
  const dq = useDebounced(search, 200);
  const [limit, setLimit] = useState(PAGE);
  const [exporting, setExporting] = useState(false);

  if (error && !data) return <LoadError error={error} />;
  if (!data) return <Skeleton h="240px" />;

  const items = data.items ?? [];
  const qs = data.questions ?? [];
  const s = feedbackStats(items);
  const rows = filterFeedback(items, score, dq);
  const shown = rows.slice(0, limit);

  return (
    <div class="stack">
      {s.total > 0 && (
        <div class="stats">
          <Stat label="问卷总数" value={s.total} hint="全部历史提交" />
          <Stat
            label="好评满意率"
            value={`${s.satRate}%`}
            hint={`${s.satisfied}/${s.total} 人选了「非常满意」或「满意」`}
          />
          <Stat
            label="主要困扰 Top 1"
            value={<span class="lg-statv">{s.topPain ? s.topPain[0] : "暂无"}</span>}
            hint={s.topPain ? `${s.topPain[1]} 人提到` : ""}
          />
          <Stat
            label="优先期望 Top 1"
            value={<span class="lg-statv">{s.topWish ? s.topWish[0] : "暂无"}</span>}
            hint={s.topWish ? `${s.topWish[1]} 人希望` : ""}
          />
        </div>
      )}

      <div class="xa-toolbar">
        <label class="lg-ctl">
          <span class="small muted">搜索</span>
          <input
            class="input"
            type="search"
            placeholder="成员或选项"
            value={search}
            onInput={(e) => setSearch(e.currentTarget.value)}
          />
        </label>
        <label class="lg-ctl">
          <span class="small muted">评价</span>
          <select class="select" value={score} onChange={(e) => set({ score: e.currentTarget.value })}>
            <option value="">全部评价</option>
            <option value="good">满意 / 好评</option>
            <option value="bad">一般 / 不满</option>
          </select>
        </label>
        <span class="spacer" />
        <Button
          disabled={exporting}
          title="下载全部反馈（CSV）"
          onClick={async () => {
            setExporting(true);
            await downloadFile("/admin/api/feedback/export", "owl-feedback.csv");
            setExporting(false);
          }}
        >
          <Download size={15} aria-hidden="true" />
          导出 CSV
        </Button>
      </div>

      <div class="card lg-listcard">
        {rows.length === 0 ? (
          <Empty title={items.length ? "未找到符合条件的反馈" : "还没有反馈"}>
            {items.length ? "换个关键字或评价试试。" : "成员在 Discord 里用 /反馈 提交后会出现在这里。"}
          </Empty>
        ) : narrow ? (
          <ul class="lg-cards" aria-label="成员反馈">
            {shown.map((it) => (
              <li key={it.id} class="lg-card">
                <div class="lg-card__top">
                  <b>
                    <Who it={it} />
                  </b>
                  <span class="small muted">{fullTime(it.ts)}</span>
                </div>
                <dl class="lg-qa">
                  {qs.map((q) => (
                    <div key={q.id}>
                      <dt class="small muted">{q.label}</dt>
                      <dd>
                        <Answer q={q.id} v={it.answers?.[q.id]} />
                      </dd>
                    </div>
                  ))}
                </dl>
              </li>
            ))}
          </ul>
        ) : (
          <TableScroll label="成员反馈">
            <table class="table lg-table">
              <caption class="sr-only">成员反馈</caption>
              <thead>
                <tr>
                  <th scope="col">时间</th>
                  <th scope="col">成员</th>
                  {qs.map((q) => (
                    <th scope="col" key={q.id}>
                      {q.label}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {shown.map((it) => (
                  <tr key={it.id}>
                    <td class="nowrap">{fullTime(it.ts)}</td>
                    <td>
                      <Who it={it} />
                    </td>
                    {qs.map((q) => (
                      <td key={q.id} class="lg-ans">
                        <Answer q={q.id} v={it.answers?.[q.id]} />
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </TableScroll>
        )}
        {rows.length > shown.length && (
          <div class="lg-more">
            <Button onClick={() => setLimit(limit + PAGE)}>
              显示更多（还有 {rows.length - shown.length} 条）
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
