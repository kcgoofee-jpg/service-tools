/** Discord：机器人状态与跑图分享设置、公告、捐赠 Key、封禁黑名单、成员问卷。 */
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead } from "./_kit";
import Announce from "./discord/announce";
import BansCard from "./discord/bans";
import BotCards from "./discord/bot";
import type { BotData, Donations } from "./discord/core";
import DonationsCard from "./discord/donations";
import SurveyCard from "./discord/survey";
import { useBadge } from "./system/badge";
import { Sect } from "./system/forms";
import "./discord.css";

export default function Discord() {
  const bot = useAdmin<BotData>("/admin/api/discord-bot");
  const don = useAdmin<Donations>("/admin/api/donations");
  // 侧栏角标：机器人离线显示「!」，有待处理的捐赠再带上数量
  const offline = bot.data ? !bot.data.status.online : null;
  const pending = don.data ? don.data.unhandled.length : null;
  const badge =
    offline === null && pending === null
      ? null
      : [offline ? "!" : "", pending ? String(pending) : ""].filter(Boolean).join(" ") || null;
  useBadge("discord", bot.data || don.data ? badge : undefined);

  return (
    <Sect>
      <SectionHead title="Discord" desc="奶妹（机器人）、站点公告、捐赠和封禁名单" />
      {bot.loading && !bot.data && <Loading rows={3} />}
      {!bot.data && bot.error && <LoadError error={bot.error} />}
      {bot.data && <BotCards data={bot.data} />}
      <Announce />
      <DonationsCard data={don.data} />
      <BansCard />
      <SurveyCard />
    </Sect>
  );
}
