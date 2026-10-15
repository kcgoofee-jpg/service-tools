/** 机器人（奶妹）：状态（心跳）。 */
import { fullTime, rel } from "../../../../lib/format";
import { Badge, Card } from "../../../../ui";
import { Stat } from "../_kit";
import type { BotData } from "./core";

export default function BotStatusCard(props: { data: BotData }) {
  const st = props.data.status;
  return (
    <Card
      title="机器人状态"
      sub={
        st.seen_at
          ? `最近心跳 ${fullTime(st.seen_at)}（${rel(st.seen_at)}）${st.ready_at ? ` · 启动于 ${fullTime(st.ready_at)}` : ""}`
          : "还没有收到心跳（机器人每分钟上报一次，3 分钟没收到算离线）"
      }
      actions={
        <Badge tone={st.online ? "ok" : "danger"} dot>
          {st.online ? "在线" : "离线"}
        </Badge>
      }
    >
      <div class="stats">
        <Stat label="账号" value={st.user || "—"} hint={st.guild ? `服务器 ${st.guild}` : undefined} />
        <Stat
          label="网关延迟"
          value={st.latency_ms != null ? `${st.latency_ms} ms` : "—"}
          hint={st.version ? `版本 ${st.version}` : undefined}
        />
      </div>
    </Card>
  );
}
