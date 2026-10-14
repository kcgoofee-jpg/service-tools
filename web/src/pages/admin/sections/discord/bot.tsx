/** 机器人（奶妹）：状态（心跳）、跑图分享设置、最近动作。 */
import { useState } from "preact/hooks";
import { fullTime, rel, shortTime } from "../../../../lib/format";
import { Badge, Card, Notice } from "../../../../ui";
import { act } from "../../Admin";
import { Stat } from "../_kit";
import { NumField, SaveBar, SwitchRow, TableWrap, TextField, useForm, withTone } from "../system/forms";
import { BOT_SPECS, type BotData, botBody, EVENT_TONE, safeDiscordUrl } from "./core";

const URL = "/admin/api/discord-bot";

function StatusCard(props: { data: BotData }) {
  const { status: st, config: c } = props.data;
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
        <Stat
          label="AI 评论"
          value={`${st.ai_today ?? 0} / ${c.gallery_ai_daily}`}
          hint={st.ai_ready ? "今天已发 / 每日上限 · API Key 就绪" : "今天已发 / 每日上限 · 未配置 API Key"}
        />
      </div>
    </Card>
  );
}

function ConfigCard(props: { data: BotData }) {
  const { status: st, config: c } = props.data;
  const form = useForm(BOT_SPECS, c as unknown as Record<string, unknown>);
  const [saving, setSaving] = useState(false);
  const save = async () => {
    setSaving(true);
    const ok = await act("PUT", URL, botBody(form.changes()), "已保存，机器人一分钟内生效");
    setSaving(false);
    if (ok) form.commit();
  };
  const aiOn = form.raw("gallery_ai") === true;
  return (
    <Card
      title="跑图分享设置"
      sub="新帖自动点赞、看图写评论；露骨图片只发「捂眼睛跑开」。改完机器人一分钟内生效，不用重启。"
    >
      <form {...form.formProps} onSubmit={(e) => e.preventDefault()}>
        <SwitchRow
          title="新帖自动点赞"
          desc="新帖自动点 ❤️。请谨慎开启：高频的机器人自动互动可能被 Discord 视作滥用"
          checked={form.raw("gallery_like") === true}
          onChange={(v) => form.set("gallery_like", v)}
        />
        <SwitchRow
          title="看图写评论"
          desc="用多模态大模型对新帖图片生成简评"
          checked={aiOn}
          onChange={(v) => form.set("gallery_ai", v)}
        />
        {aiOn && !st.ai_ready && (
          <Notice tone="warn">机器人在服务器上没有配置 AI API Key，即使开启也不会生成评论。</Notice>
        )}
        <div class="form-grid" style={{ marginTop: "var(--sp-4)" }}>
          <TextField form={form} name="gallery_forum" />
          <NumField form={form} name="gallery_ai_daily" />
          <TextField form={form} name="gallery_ai_model" />
        </div>
        <SaveBar form={form} onSave={save} saving={saving} />
      </form>
    </Card>
  );
}

function EventsCard(props: { data: BotData }) {
  const ev = props.data.events;
  return (
    <Card title="最近动作" sub="最近 50 条，跑图分享有新帖时出现">
      {!ev.length ? (
        <p class="muted small">还没有记录。</p>
      ) : (
        <TableWrap label="机器人最近的动作">
          <table class="table">
            <caption class="sr-only">机器人最近的动作</caption>
            <thead>
              <tr>
                <th scope="col">时间</th>
                <th scope="col">动作</th>
                <th scope="col">帖子</th>
                <th scope="col">作者</th>
                <th scope="col">内容</th>
              </tr>
            </thead>
            <tbody>
              {ev.map((e, i) => {
                const href = e.url ? safeDiscordUrl(e.url) : null;
                return (
                  <tr key={`${e.at}-${i}`}>
                    <td class="nowrap" title={fullTime(e.at)}>
                      {shortTime(e.at)}
                    </td>
                    <td>
                      <Badge {...withTone(EVENT_TONE[e.kind])}>{props.data.kinds[e.kind] ?? e.kind}</Badge>
                    </td>
                    <td>
                      {href ? (
                        <a href={href} target="_blank" rel="noopener noreferrer">
                          {e.title || "帖子"}
                        </a>
                      ) : (
                        e.title
                      )}
                    </td>
                    <td>{e.author}</td>
                    <td class="muted">{e.detail}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </TableWrap>
      )}
    </Card>
  );
}

export default function BotCards(props: { data: BotData }) {
  return (
    <>
      <StatusCard data={props.data} />
      <ConfigCard data={props.data} />
      <EventsCard data={props.data} />
    </>
  );
}
