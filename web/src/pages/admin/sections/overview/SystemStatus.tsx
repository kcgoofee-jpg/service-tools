/** 系统状态（默认折叠）：上游健康、告警渠道、生成记录、暴力破解防护、磁盘。 */
import { useState } from "preact/hooks";
import { ApiError, sendJSON } from "../../../../lib/api";
import { Badge, Button, Meter, toast } from "../../../../ui";
import type { StatusResp } from "./types";

const gb = (mb: number): string => (mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb} MB`);

export function diskUsage(d: StatusResp["disk"]): {
  usedPct: number;
  freePct: number;
  tone: "warn" | "danger" | undefined;
} {
  const used = Math.max(0, d.total_mb - d.free_mb);
  const usedPct = d.total_mb > 0 ? Math.round((used / d.total_mb) * 100) : 0;
  const freePct = 100 - usedPct;
  return { usedPct, freePct, tone: freePct < 10 ? "danger" : freePct < 20 ? "warn" : undefined };
}

export function SystemStatus(props: { s: StatusResp }) {
  const { s } = props;
  const [testing, setTesting] = useState(false);
  const { usedPct, freePct, tone } = diskUsage(s.disk);
  const up = s.upstream;
  const recording = s.audit.prompts || s.audit.thumbs;
  const sendTest = async () => {
    setTesting(true);
    try {
      await sendJSON("POST", "/admin/api/alerts/test");
      toast("测试告警已发送，请检查告警渠道");
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) toast("尚未配置告警渠道", { error: true });
      else toast(e instanceof Error ? e.message : String(e), { error: true });
    } finally {
      setTesting(false);
    }
  };
  return (
    <details class="card ov-fold">
      <summary class="ov-fold__sum">
        <h2 class="card__title">系统状态</h2>
        <Badge tone={up.status === "degraded" ? "warn" : "ok"} dot>
          上游{up.status === "degraded" ? "不稳定" : up.status === "ok" ? "正常" : "空闲"}
        </Badge>
        {!s.alerts.configured && <Badge tone="warn">告警未配置</Badge>}
        {freePct < 20 && <Badge tone={tone === "danger" ? "danger" : "warn"}>磁盘剩 {freePct}%</Badge>}
      </summary>
      <dl class="ov-sys">
        <div>
          <dt>上游健康</dt>
          <dd>
            最近 {up.recent} 次请求，失败 {up.failed} 次
            {up.image_cooldown_seconds > 0 && ` · 图片冷却 ${up.image_cooldown_seconds} 秒`}
          </dd>
        </div>
        <div>
          <dt>告警渠道</dt>
          <dd class="ov-sys__row">
            <span>
              {s.alerts.configured
                ? `已配置，已发送 ${s.alerts.sent} 条告警`
                : "未配置；设置 ALERT_* 环境变量后可接收异常通知"}
            </span>
            <Button size="sm" onClick={sendTest} disabled={testing}>
              发送测试告警
            </Button>
          </dd>
        </div>
        <div>
          <dt>生成记录</dt>
          <dd>
            {recording ? "记录中" : "已关闭"} · 提示词 {s.audit.prompts ? "开" : "关"}（
            {s.audit.retention_days > 0 ? `${s.audit.retention_days} 天` : "长期"}）· 原图{" "}
            {s.audit.thumbs ? "开（按容量清理）" : "关"}
          </dd>
        </div>
        <div>
          <dt>暴力破解防护</dt>
          <dd>
            Key 鉴权失败 {s.protection.auth_fail_max} 次 / {s.protection.auth_fail_window} 秒后封禁{" "}
            {s.protection.auth_block_seconds} 秒；后台登录最多尝试 {s.protection.login_max_attempts} 次
          </dd>
        </div>
        <div>
          <dt>磁盘空间</dt>
          <dd>
            <Meter
              label={`剩余 ${gb(s.disk.free_mb)}`}
              value={usedPct}
              max={100}
              text={`已用 ${usedPct}% · 共 ${gb(s.disk.total_mb)}`}
              {...(tone ? { tone } : {})}
            />
          </dd>
        </div>
      </dl>
    </details>
  );
}
