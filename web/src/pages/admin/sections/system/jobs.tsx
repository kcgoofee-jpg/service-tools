/**
 * 定时任务：服务端按固定周期自动跑的任务，每个任务上次的结果。只看不管——没有开关、没有历史、没有「立即运行」。
 * 宽屏是表格，窄屏改成一条条卡片（与日志页同一断点）。根元素 id="jobs"，总览提醒链接到 /admin/system#jobs。
 */
import { useEffect } from "preact/hooks";
import { fullTime, num, rel } from "../../../../lib/format";
import { Badge, Card, cx } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError, Loading, TableWrap } from "../_kit";
import { splitTail } from "../logs/feedbackStats";
import { NARROW, useMedia } from "../members/shared";
import { withTone } from "./forms";
import { JOB_STATE_TEXT, type Job, type JobState, type JobsResponse, jobState } from "./jobs-core";
import "./jobs.css";

const ID = "jobs";
const TITLE = "定时任务";

const TONE: Record<JobState, "ok" | "danger" | "warn" | undefined> = {
  ok: "ok",
  error: "danger",
  stale: "warn",
  never: undefined,
};

function StateBadge(props: { state: JobState }) {
  return <Badge {...withTone(TONE[props.state])}>{JOB_STATE_TEXT[props.state]}</Badge>;
}

/** 上次运行：相对时间，悬停看绝对时间 */
function LastRun(props: { job: Job; now: number }) {
  const at = props.job.last?.at;
  return at ? <span title={fullTime(at)}>{rel(at, props.now)}</span> : <span class="muted">—</span>;
}

const msText = (job: Job): string => (job.last ? `${num(Math.round(job.last.ms))} ms` : "—");

/** 一句摘要（出错时是错误文字，红色）。末尾几个字绑在一起不折行，最后一行不会只剩一两个字。 */
function Summary(props: { job: Job; state: JobState }) {
  const text = props.job.last?.summary ?? "";
  if (!text) return <span class="muted">—</span>;
  const [head, tail] = splitTail(text, 6);
  return (
    <span class={cx("jobs__sum", props.state === "error" && "jobs__err")}>
      {head}
      {tail && <span class="nowrap">{tail}</span>}
    </span>
  );
}

function JobsTable(props: { jobs: Job[]; now: number }) {
  return (
    <TableWrap label={TITLE}>
      <table class="table jobs-tbl">
        <caption class="sr-only">定时任务的周期、上次运行时间、结果、摘要和耗时</caption>
        <thead>
          <tr>
            <th scope="col">任务</th>
            <th scope="col">周期</th>
            <th scope="col">上次运行</th>
            <th scope="col">结果</th>
            <th scope="col">摘要</th>
            <th scope="col" class="num">
              耗时
            </th>
          </tr>
        </thead>
        <tbody>
          {props.jobs.map((j) => {
            const s = jobState(j, props.now);
            return (
              <tr key={j.name}>
                <th scope="row">{j.title}</th>
                <td class="nowrap">{j.every}</td>
                <td class="nowrap">
                  <LastRun job={j} now={props.now} />
                </td>
                <td class="nowrap">
                  <StateBadge state={s} />
                </td>
                <td class="wrap">
                  <Summary job={j} state={s} />
                </td>
                <td class="num nowrap jobs__ms">{msText(j)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </TableWrap>
  );
}

function JobsList(props: { jobs: Job[]; now: number }) {
  return (
    <ul class="jobs-list" aria-label={TITLE}>
      {props.jobs.map((j) => {
        const s = jobState(j, props.now);
        return (
          <li key={j.name} class="jobs-item">
            <div class="jobs-item__top">
              <b>{j.title}</b>
              <StateBadge state={s} />
            </div>
            {j.last?.summary && (
              <p>
                <Summary job={j} state={s} />
              </p>
            )}
            {/* 每段不拆开，段与段之间可以换行 */}
            <p class="muted">
              <span class="nowrap">{j.every}</span>
              {j.last && (
                <>
                  {" · "}
                  <span class="nowrap">
                    上次 <LastRun job={j} now={props.now} />
                  </span>
                  {" · "}
                  <span class="nowrap tnum">耗时 {msText(j)}</span>
                </>
              )}
            </p>
          </li>
        );
      })}
    </ul>
  );
}

/** 从别处点 /admin/system#jobs 进来：分区是懒加载的，浏览器自己滚不到，等数据到了再滚过去 */
function useScrollToHash(ready: boolean): void {
  useEffect(() => {
    if (!ready || location.hash !== `#${ID}`) return;
    const t = setTimeout(() => {
      document.getElementById(ID)?.scrollIntoView({
        block: "start",
        behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth",
      });
    }, 60);
    return () => clearTimeout(t);
  }, [ready]);
}

export default function JobsCard() {
  const { data, error, loading } = useAdmin<JobsResponse>("/admin/api/jobs");
  const narrow = useMedia(NARROW);
  useScrollToHash(!!data);

  if (loading && !data)
    return (
      <Card id={ID} title={TITLE}>
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card id={ID} title={TITLE}>
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );

  const now = Date.now() / 1000;
  return (
    <Card id={ID} title={TITLE} sub="服务端按固定周期自动运行；出错或长时间没跑时，总览会提醒。">
      <div class="stack-sm">
        {data.jobs.length === 0 ? (
          <p class="muted small">还没有登记任何定时任务。</p>
        ) : narrow ? (
          <JobsList jobs={data.jobs} now={now} />
        ) : (
          <JobsTable jobs={data.jobs} now={now} />
        )}
        {/* 开关只有「额度与保护」那一个，这里只指路 */}
        <p class="muted small jobs__foot">
          动态额度开关在<a href="/admin/limits">「额度与保护」</a>
        </p>
      </div>
    </Card>
  );
}
