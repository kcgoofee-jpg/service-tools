/**
 * 总览：结论与待办 → 关键数字 → 状态码分布 / 近 7 天走势 → 各功能用量 → 系统状态。
 * 数据都跟着全局刷新节拍自动更新（没有刷新按钮）。
 */
import { useState } from "preact/hooks";
import { num } from "../../../lib/format";
import { Badge } from "../../../ui";
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead, Stat } from "./_kit";
import type { MembersResp } from "./members/types";
import { FeatureUsage } from "./overview/FeatureUsage";
import {
  buildReminders,
  computeVerdict,
  hideReminder,
  readHidden,
  restoreReminders,
} from "./overview/reminders";
import { StatusCodes } from "./overview/StatusCodes";
import { SystemStatus } from "./overview/SystemStatus";
import { Trend } from "./overview/Trend";
import type {
  BotResp,
  BugsResp,
  LiveResp,
  ModulesResp,
  Overview as OverviewData,
  StatusResp,
} from "./overview/types";
import { Verdict } from "./overview/Verdict";
import "./overview.css";

const Of = (props: { children: string | number }) => <small class="ov-of">/ {props.children}</small>;

export default function Overview() {
  const ov = useAdmin<OverviewData>("/admin/api/overview");
  const status = useAdmin<StatusResp>("/admin/api/status");
  const live = useAdmin<LiveResp>("/public/live");
  const bugs = useAdmin<BugsResp>("/admin/api/errors");
  const bot = useAdmin<BotResp>("/admin/api/discord-bot");
  const mods = useAdmin<ModulesResp>("/admin/api/modules");
  const mem = useAdmin<MembersResp>("/admin/api/members");
  const [hidden, setHidden] = useState(() => readHidden());

  const d = ov.data;
  if (!d) {
    return (
      <div class="xa-page">
        <SectionHead title="总览" />
        {ov.error ? <LoadError error={ov.error} /> : <Loading rows={4} />}
      </div>
    );
  }

  const now = Date.now() / 1000;
  const reminders = buildReminders({
    overview: d,
    status: status.data,
    bot: bot.data,
    bugs: bugs.data,
    modules: mods.data,
    members: mem.data?.members ?? null,
    now,
  });
  // 隐藏记录每次渲染都重读一次，过了 24 小时的自动失效
  const verdict = computeVerdict(reminders, { ...readHidden(), ...hidden });

  const guard = live.data?.guard;
  const hourCap = guard?.hour_cap ?? 0;
  const hour = guard?.hour;
  const atCap = hourCap > 0 && (hour ?? 0) >= hourCap;
  const v5Limit = d.v5_limit || live.data?.quota?.v5_cap || 0;
  const totals = d.feature_usage.reduce(
    (a, f) => ({ rej: a.rej + (f.today.rejected || 0), err: a.err + (f.today.error || 0) }),
    { rej: 0, err: 0 },
  );
  const members = d.members_total ?? 0;
  const active = d.active_members ?? 0;

  return (
    <div class="xa-page">
      <SectionHead title="总览" />
      <Verdict
        verdict={verdict}
        onHide={(k) => setHidden(hideReminder(k))}
        onRestore={() => setHidden(restoreReminders())}
      />

      <div class="stats ov-stats">
        <Stat
          label="今日出图（全部）"
          value={
            <>
              {num(d.today.images)} <Of>张</Of>
            </>
          }
          hint={`成员 ${num(d.today.member_images)} · 站长 / 测试 ${num(d.today.images - d.today.member_images)} · V5 ${num(d.today.v5)}${v5Limit ? ` / ${num(v5Limit)}` : ""}`}
        />
        <Stat
          label="今天在用的成员"
          value={
            <>
              {active}
              {members > 0 && <Of>{members}</Of>}
            </>
          }
          hint={`有效 Key ${d.keys_active} / ${d.keys_total}`}
        />
        <Stat
          label="最近 60 分钟（滚动窗口）出图"
          value={
            <>
              {hour ?? "—"}
              {hourCap > 0 && <Of>{hourCap}</Of>}
            </>
          }
          hint={atCap ? <Badge tone="warn">已到上限，新请求要等几分钟</Badge> : "每小时上限用来保护上游账号"}
        />
        <Stat
          label="今日被拒 / 失败"
          value={
            <>
              {num(totals.rej)} <Of>{num(totals.err)}</Of>
            </>
          }
          hint={`本月 Anlas ${num(d.month.anlas)}${d.anlas_budget > 0 ? ` / ${num(d.anlas_budget)}` : ""}`}
        />
      </div>

      <div class="grid-2 ov-grid">
        <StatusCodes />
        <Trend week={d.week} />
      </div>

      <FeatureUsage rows={d.feature_usage} logSince={d.log_since} />

      {status.data && <SystemStatus s={status.data} />}
    </div>
  );
}
