/**
 * 日志：生成记录 / 用量日志 / 操作日志 / Bug 追踪 / 成员反馈 / 问卷。
 * 每个子页是一条路由 /admin/logs/<tab>，筛选条件放在查询串里，都可以直接收藏或分享链接。
 */
import type { ComponentType } from "preact";
import { useEffect } from "preact/hooks";
import { useLocation } from "preact-iso";
import { Badge, cx } from "../../../ui";
import { SectionHead } from "./_kit";
import { Actions } from "./logs/Actions";
import { Audit } from "./logs/Audit";
import { Bugs } from "./logs/Bugs";
import { Feedback } from "./logs/Feedback";
import { Survey } from "./logs/Survey";
import { Usage } from "./logs/Usage";
import { useOpenBugs } from "./members/shared";
import "./logs.css";

type Tab = { id: string; label: string; view: ComponentType };
const TABS: Tab[] = [
  { id: "audit", label: "生成记录", view: Audit },
  { id: "usage", label: "用量日志", view: Usage },
  { id: "actions", label: "操作日志", view: Actions },
  { id: "bugs", label: "Bug 追踪", view: Bugs },
  { id: "feedback", label: "成员反馈", view: Feedback },
  { id: "survey", label: "问卷", view: Survey },
];

export default function Logs() {
  const { path, route } = useLocation();
  const open = useOpenBugs();
  const seg = path.split("/")[3] ?? "";
  const tab = TABS.find((t) => t.id === seg);
  // /admin/logs 或不认识的子路径：回到第一个子页（replace，不留历史）
  useEffect(() => {
    if (!tab) route("/admin/logs/audit", true);
  }, [tab, route]);
  const View = tab?.view;
  return (
    <div class="xa-page">
      <SectionHead title="日志" />
      <nav class="lg-tabs" aria-label="日志类型">
        {TABS.map((t) => (
          <a
            key={t.id}
            href={`/admin/logs/${t.id}`}
            class={cx("lg-tab", t.id === tab?.id && "is-active")}
            aria-current={t.id === tab?.id ? "page" : undefined}
          >
            {t.label}
            {t.id === "bugs" && open > 0 && <Badge tone="danger">{open}</Badge>}
          </a>
        ))}
      </nav>
      {View && <View key={tab.id} />}
    </div>
  );
}
