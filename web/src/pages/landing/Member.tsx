import { BookOpen, Eye, EyeOff, FileArchive, Ticket } from "lucide-preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { ApiError, sendJSON } from "../../lib/api";
import { dayOf } from "../../lib/format";
import { Badge, Button, Card, CopyField, Dialog, toast } from "../../ui";
import { useStore } from "./ctx";
import { couponLeft, maskKey, quotaRows, resetHour } from "./helpers";
import type { Coupon, MeMember } from "./types";
import { UsageMeter } from "./Usage";

/* ---------------- 今日额度（登录成员首屏第一块） ---------------- */

/** 全站最近 60 分钟用量：滚动窗口，不是整点重置。单独成组件：live 每秒变化，只重绘这一行。 */
function HourlyMeter() {
  const { live } = useStore();
  const g = live.value?.guard;
  const cap = g?.hour_cap ?? 0;
  if (!(cap > 0)) return null;
  return (
    <UsageMeter
      label="全站最近 60 分钟（滚动窗口）"
      used={g?.hour ?? 0}
      cap={cap}
      hint="按滚动窗口计算，不是整点重置；额度随时间逐张恢复。"
    />
  );
}

function Timing({ me }: { me: MeMember }) {
  const t = me.timing;
  if (!t?.count || (!t.avg_dur_s && !t.avg_wait_s)) return null;
  return (
    <dl class="lp-facts">
      {t.avg_dur_s ? (
        <div>
          <dt>我的平均出图耗时</dt>
          <dd>
            {t.avg_dur_s} 秒
            {t.site_avg_dur_s ? <span class="muted">（全站 {t.site_avg_dur_s} 秒）</span> : null}
          </dd>
        </div>
      ) : null}
      {t.avg_wait_s ? (
        <div>
          <dt>我的平均排队等待</dt>
          <dd>{t.avg_wait_s} 秒</dd>
        </div>
      ) : null}
    </dl>
  );
}

export function QuotaCard({ me }: { me: MeMember }) {
  return (
    <Card title="我今天的额度" sub={`每日 ${resetHour(me)} 点重置`} class="lp-quota">
      <div class="lp-meters">
        {quotaRows(me).map((r) => (
          <UsageMeter
            key={r.id}
            label={r.label}
            used={r.used}
            cap={r.cap}
            {...(r.id === "v5" ? { base: "v5" as const } : {})}
            hint={r.hint}
          />
        ))}
        <HourlyMeter />
      </div>
      <Timing me={me} />
    </Card>
  );
}

/** 已登录但还没有 Key */
export function NoKeyCard({ me }: { me: MeMember }) {
  const { meta } = useStore();
  const closed = meta.value.registration?.open === false;
  const name = me.discord?.display_name || me.discord?.username || "成员";
  return (
    <Card title={`你好，${name}`} class="lp-quota">
      <div class="stack">
        {closed ? (
          <p>你还没有 Key。注册当前已关闭，开放时间看站长公告。</p>
        ) : (
          <>
            <p>你还没有 Key。在 Discord 服务器任意频道输入下面的命令即可领取。</p>
            <CopyField value="/register" label="命令" />
          </>
        )}
      </div>
    </Card>
  );
}

/* ---------------- Key ---------------- */

export function KeyCard({ me }: { me: MeMember }) {
  const [shown, setShown] = useState(false);
  const key = me.key ?? "";
  const site = location.origin;
  return (
    <Card
      title="我的 Key"
      sub={me.expires_at ? `有效期至 ${dayOf(me.expires_at)}` : "长期有效"}
      class="lp-keycard"
    >
      <div class="stack">
        <div class="lp-keyrow">
          <CopyField label="Key" value={key} display={shown ? key : maskKey(key)} />
          <Button size="sm" aria-label={shown ? "隐藏 Key" : "显示完整 Key"} onClick={() => setShown(!shown)}>
            {shown ? <EyeOff size={14} aria-hidden="true" /> : <Eye size={14} aria-hidden="true" />}
            {shown ? "隐藏" : "显示"}
          </Button>
        </div>
        <div class="lp-field">
          <span class="lp-field__label">接入地址（填进客户端）</span>
          <CopyField label="接入地址" value={site} />
        </div>
        <p class="muted small">
          Key 泄露了？在 Discord 输入 <code>/resetkey</code> 换新 Key，旧的随即作废。
        </p>
      </div>
    </Card>
  );
}

/* ---------------- 重置券 ---------------- */

export function Coupons({ coupons }: { coupons: Coupon[] }) {
  const { reloadMe } = useStore();
  const [target, setTarget] = useState<Coupon | null>(null);
  const [busy, setBusy] = useState(false);
  const last = useRef<Coupon | null>(null);
  if (target) last.current = target;

  const use = async () => {
    if (!target) return;
    setBusy(true);
    try {
      await sendJSON("POST", `/public/me/coupons/${encodeURIComponent(String(target.id))}/use`);
      toast("已重置，今天的 V5 与 Anlas 额度已恢复。");
    } catch (e) {
      toast(e instanceof ApiError ? e.message : "没有成功，请稍后再试", { error: true });
    } finally {
      setBusy(false);
      setTarget(null);
      void reloadMe();
    }
  };

  if (!coupons.length) return null;
  return (
    <Card title="重置券" sub="重置今天的 V5 与 Anlas 额度，一张用一次">
      <ul class="lp-coupons">
        {coupons.map((c) => (
          <li key={c.id} class="lp-coupon">
            <Ticket size={20} aria-hidden="true" class="lp-coupon__ic" />
            <div class="lp-coupon__body">
              <p>还有 {couponLeft(c.expires_at)}过期</p>
              {c.note && <p class="muted small">「{c.note}」</p>}
            </div>
            <Button variant="primary" size="sm" onClick={() => setTarget(c)}>
              使用
            </Button>
          </li>
        ))}
      </ul>
      <Dialog
        open={!!target}
        title="使用重置券"
        onClose={() => !busy && setTarget(null)}
        footer={
          <>
            <Button variant="ghost" onClick={() => setTarget(null)} disabled={busy}>
              取消
            </Button>
            <Button variant="primary" onClick={use} disabled={busy}>
              {busy ? "重置中…" : "确定使用"}
            </Button>
          </>
        }
      >
        <p>
          现在重置今天的 V5 与 Anlas 额度？
          {last.current?.note ? `（「${last.current.note}」）` : ""}
        </p>
        <p class="muted small">一张券只能用一次，用掉后不能撤销。</p>
      </Dialog>
    </Card>
  );
}

/* ---------------- 作品备份 ---------------- */

export function Backup({ me }: { me: MeMember }) {
  const days = me.image_retention_days ?? 0;
  const stored = me.images_stored ?? 0;
  const [locked, setLocked] = useState(false);
  useEffect(() => {
    if (!locked) return;
    // 打包较重：点一次后两个按钮都锁一分钟，避免连点撞上服务端限流
    const t = setTimeout(() => setLocked(false), 60_000);
    return () => clearTimeout(t);
  }, [locked]);
  if (days <= 0 || stored <= 0) return null;
  const click = (e: Event) => {
    if (locked) {
      e.preventDefault();
      return;
    }
    toast("正在打包，下载马上开始…");
    setLocked(true);
  };
  const a11y = locked ? { "aria-disabled": "true" as const, tabIndex: -1 } : {};
  return (
    <Card
      title={
        <>
          作品备份 <Badge tone="accent">beta</Badge>
        </>
      }
      sub={`可以打包你最近 ${days} 天的 ${stored} 张原图，用来找回客户端里误删的图。这不是长期相册，请自己保存好；打包较重，每分钟限 1 次。`}
    >
      <div class="lp-export">
        <a class="btn" href="/public/my-export?format=epub" onClick={click} {...a11y}>
          <BookOpen size={16} aria-hidden="true" /> EPUB 画册
        </a>
        <a class="btn" href="/public/my-export?format=zip" onClick={click} {...a11y}>
          <FileArchive size={16} aria-hidden="true" /> 原图 ZIP
        </a>
      </div>
      <p class="muted small lp-export__hint">
        EPUB：一图一页，附提示词和参数，图书 App 都能打开；ZIP：PNG 原图 + prompts.txt，适合存档或二次编辑。
      </p>
    </Card>
  );
}

/* ---------------- 问卷入口 ---------------- */

export function SurveyLink() {
  return (
    <Card title="使用体验问卷" sub="大约 1 分钟，帮站长决定下一步优化什么。">
      <a class="btn" href="/survey">
        去填写
      </a>
    </Card>
  );
}
