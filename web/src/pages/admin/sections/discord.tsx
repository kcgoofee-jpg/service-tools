/** Discord：机器人状态、公告、捐赠 Key。封禁黑名单在「成员 › 更多」，成员问卷在「日志 › 问卷」。 */
import { useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead } from "./_kit";
import Announce from "./discord/announce";
import BotStatusCard from "./discord/bot";
import type { BotData, Donations } from "./discord/core";
import DonationsCard from "./discord/donations";
import { Sect } from "./system/forms";
import "./discord.css";

export default function Discord() {
  const bot = useAdmin<BotData>("/admin/api/discord-bot");
  const don = useAdmin<Donations>("/admin/api/donations");
  return (
    <Sect>
      <SectionHead title="Discord" desc="奶妹（机器人）、站点公告和捐赠" />
      {bot.loading && !bot.data && <Loading rows={3} />}
      {!bot.data && bot.error && <LoadError error={bot.error} />}
      {bot.data && <BotStatusCard data={bot.data} />}
      <Announce />
      <DonationsCard data={don.data} />
    </Sect>
  );
}
