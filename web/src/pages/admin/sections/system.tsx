/** 系统：服务器状态、定时任务、告警、生成记录与原图、状态页事件、后台密码。 */
import { SectionHead } from "./_kit";
import { Sect } from "./system/forms";
import Incidents from "./system/incidents";
import JobsCard from "./system/jobs";
import PasswordCard from "./system/password";
import { AlertsCard, ServerCard, StorageCard } from "./system/server";
import "./system.css";

export default function System() {
  return (
    <Sect>
      <SectionHead title="系统" desc="服务器状态、定时任务、状态页事件和后台密码" />
      <ServerCard />
      <JobsCard />
      <AlertsCard />
      <StorageCard />
      <Incidents />
      <PasswordCard />
    </Sect>
  );
}
