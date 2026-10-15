/** 服务器状态：磁盘、上游健康、告警渠道、生成记录与原图存储（清空原图）。 */
import { useState } from "preact/hooks";
import { duration } from "../../../../lib/format";
import { Badge, Button, Card, Meter, Notice, toast } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading, Stat } from "../_kit";
import { bytesText, mbText } from "./core";
import { ConfirmDialog, Kv, withTone } from "./forms";

export type ServerStatus = {
  alerts: { configured: boolean; sent: number };
  audit: {
    prompts: boolean;
    thumbs: boolean;
    retention_days: number;
    image_cap_mb: number;
    image_bytes: number;
    image_keep_hours: number;
  };
  upstream: { status: string; recent: number; failed: number; image_cooldown_seconds: number };
  protection: {
    auth_fail_max: number;
    auth_fail_window: number;
    auth_block_seconds: number;
    login_max_attempts: number;
  };
  disk: { free_mb: number; total_mb: number };
};

const UP_LABEL: Record<string, { text: string; tone: "ok" | "warn" | undefined }> = {
  ok: { text: "正常", tone: "ok" },
  degraded: { text: "不稳定", tone: "warn" },
  idle: { text: "空闲", tone: undefined },
};

export function ServerCard() {
  const st = useAdmin<ServerStatus>("/admin/api/status");
  if (st.loading && !st.data)
    return (
      <Card title="服务器状态">
        <Loading rows={2} />
      </Card>
    );
  if (!st.data)
    return (
      <Card title="服务器状态">
        <LoadError error={st.error ?? "没有数据"} />
      </Card>
    );
  const s = st.data;
  const used = Math.max(0, s.disk.total_mb - s.disk.free_mb);
  const freeRatio = s.disk.total_mb > 0 ? s.disk.free_mb / s.disk.total_mb : 1;
  const up = UP_LABEL[s.upstream.status] ?? { text: s.upstream.status, tone: undefined };
  return (
    <Card title="服务器状态" sub="上游 Token 在「上游」页管理，其余敏感配置在服务器的 .env。">
      <div class="stack-sm">
        <Meter
          label="磁盘使用"
          value={used}
          max={s.disk.total_mb}
          text={`已用 ${Math.round((used / Math.max(1, s.disk.total_mb)) * 100)}% · 剩余 ${mbText(s.disk.free_mb)} / 共 ${mbText(s.disk.total_mb)}`}
          {...(freeRatio < 0.1
            ? { tone: "danger" as const }
            : freeRatio < 0.2
              ? { tone: "warn" as const }
              : {})}
          hint="原图和日志会越来越多；剩余低于 20% 变黄、低于 10% 变红"
        />
        <div class="stats">
          <Stat
            label="上游健康"
            value={<Badge {...withTone(up.tone)}>{up.text}</Badge>}
            hint={`最近 10 分钟 ${s.upstream.recent} 次请求，失败 ${s.upstream.failed} 次${
              s.upstream.image_cooldown_seconds > 0
                ? ` · 图片冷却 ${duration(s.upstream.image_cooldown_seconds)}`
                : ""
            }`}
          />
        </div>
        <Kv
          rows={[
            [
              "暴力破解防护",
              `Key 鉴权失败 ${s.protection.auth_fail_max} 次 / ${s.protection.auth_fail_window} 秒后封禁 ${s.protection.auth_block_seconds} 秒；后台登录最多尝试 ${s.protection.login_max_attempts} 次`,
            ],
          ]}
        />
      </div>
    </Card>
  );
}

export function AlertsCard() {
  const { data } = useAdmin<ServerStatus>("/admin/api/status");
  const [open, setOpen] = useState(false);
  if (!data) return null;
  const a = data.alerts;
  return (
    <Card
      title="告警渠道"
      sub="只有重大事故才发告警：上游 Token 失效、账号可能被封控、服务停了、磁盘快满。其余要处理的事列在「总览」里，不打扰。"
      actions={
        <Badge {...withTone(a.configured ? "ok" : "warn")}>{a.configured ? "已配置" : "未配置"}</Badge>
      }
    >
      <div class="stack-sm">
        <p class="small">
          {a.configured
            ? `已发送 ${a.sent} 条告警。`
            : "在服务器的 .env 里设置 ALERT_USER_ID / ALERT_CHANNEL_ID / ALERT_WEBHOOK_URL 后可接收异常通知。"}
        </p>
        <div class="actions">
          <Button disabled={!a.configured} onClick={() => setOpen(true)}>
            发送测试告警
          </Button>
        </div>
      </div>
      <ConfirmDialog
        open={open}
        title="发送测试告警？"
        confirmLabel="发送"
        tone="primary"
        onClose={() => setOpen(false)}
        onConfirm={async () => {
          const ok = await act("POST", "/admin/api/alerts/test", undefined, "测试告警已发出，请检查告警渠道");
          if (ok) setOpen(false);
        }}
      >
        <p>会向告警渠道发一条真实的测试消息（10 秒内只能发一次）。</p>
      </ConfirmDialog>
    </Card>
  );
}

export function StorageCard() {
  const { data } = useAdmin<ServerStatus>("/admin/api/status");
  const [open, setOpen] = useState(false);
  if (!data) return null;
  const a = data.audit;
  return (
    <Card
      title="清空原图"
      sub="开关、保留时长和容量上限在「领 Key 与功能」里设置；这里只管一次性清空，只删原图，提示词和参数保留。"
    >
      <div class="actions">
        <Button variant="danger" disabled={a.image_bytes <= 0} onClick={() => setOpen(true)}>
          清空服务器上的全部原图
        </Button>
      </div>
      <ConfirmDialog
        open={open}
        title="清空全部原图？"
        confirmLabel="清空"
        tone="danger"
        requireText="清空"
        onClose={() => setOpen(false)}
        onConfirm={async () => {
          const r = await act<{ purged: number }>(
            "POST",
            "/admin/api/audit/purge-images",
            { confirm: "清空" },
            "",
          );
          if (r) {
            toast(`已清空 ${r.purged} 张原图`);
            setOpen(false);
          }
        }}
      >
        <Notice tone="danger">
          将删除服务器上的全部原图（当前占用 {bytesText(a.image_bytes)}
          ），且无法恢复。提示词和参数保留。
        </Notice>
      </ConfirmDialog>
    </Card>
  );
}
