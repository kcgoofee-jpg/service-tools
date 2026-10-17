/**
 * 总览：要处理的事 → 今日出图 / 今天在用的成员 → 各功能用量。
 * 数据都跟着全局刷新节拍自动更新（没有刷新按钮）。
 */
import { num } from "../../../lib/format";
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead, Stat } from "./_kit";
import { FeatureUsage } from "./overview/FeatureUsage";
import { buildReminders, computeVerdict } from "./overview/reminders";
import { StatusCodes } from "./overview/StatusCodes";
import type { BotResp, BugsResp, Overview as OverviewData } from "./overview/types";
import { Verdict } from "./overview/Verdict";
import type { JobsResponse } from "./system/jobs-core";
import "./overview.css";

/** 一行小字里的几段：每段不拆开（不让「测试」折成「测 / 试」），段与段之间可以换行 */
const Parts = (props: { items: string[] }) => (
  <>
    {props.items.map((t, i) => (
      <span key={t}>
        {i > 0 && " · "}
        <span style={{ whiteSpace: "nowrap" }}>{t}</span>
      </span>
    ))}
  </>
);

const Of = (props: { children: string | number }) => <small class="ov-of">/ {props.children}</small>;

export default function Overview() {
  const ov = useAdmin<OverviewData>("/admin/api/overview");
  const bugs = useAdmin<BugsResp>("/admin/api/errors");
  const bot = useAdmin<BotResp>("/admin/api/discord-bot");
  const jobs = useAdmin<JobsResponse>("/admin/api/jobs");

  const d = ov.data;
  if (!d) {
    return (
      <div class="xa-page">
        <SectionHead title="总览" />
        {ov.error ? <LoadError error={ov.error} /> : <Loading rows={4} />}
      </div>
    );
  }

  const verdict = computeVerdict(
    buildReminders({
      overview: d,
      bot: bot.data,
      bugs: bugs.data,
      jobs: jobs.data,
      now: Date.now() / 1000,
    }),
  );
  const members = d.members_total ?? 0;

  return (
    <div class="xa-page">
      <SectionHead title="总览" />
      <Verdict verdict={verdict} />

      <div class="stats ov-stats">
        <Stat
          label="今日出图"
          value={<>{num(d.today.images)} 张</>}
          hint={
            <Parts
              items={[
                `成员 ${num(d.today.member_images)}`,
                `站长 / 测试 ${num(d.today.images - d.today.member_images)}`,
                `V5 ${num(d.today.v5)}${d.v5_limit ? ` / ${num(d.v5_limit)}` : ""}`,
              ]}
            />
          }
        />
        <Stat
          label="今天在用的成员"
          value={
            <>
              {d.active_members ?? 0}
              {members > 0 && (
                <>
                  {" "}
                  <Of>{members}</Of>
                </>
              )}
            </>
          }
          hint={`有效 Key ${d.keys_active} / ${d.keys_total}`}
        />
      </div>

      <StatusCodes />

      <FeatureUsage rows={d.feature_usage} logSince={d.log_since} />
    </div>
  );
}
