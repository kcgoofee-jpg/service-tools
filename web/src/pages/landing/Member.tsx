import { Eye, EyeOff, Ticket } from "lucide-preact";
import { useRef, useState } from "preact/hooks";
import { ApiError, sendJSON } from "../../lib/api";
import { dayOf } from "../../lib/format";
import { Button, Card, CopyField, Dialog, toast } from "../../ui";
import { useStore } from "./ctx";
import { couponLeft, maskKey, quotaRows, resetHour } from "./helpers";
import type { Coupon, MeMember } from "./types";
import { UsageMeter } from "./Usage";

/* ---------------- 今日额度（登录成员首屏第一块） ---------------- */

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
      </div>
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
          <CopyField label="Key" value={key} display={shown ? key : maskKey(key)} wrap />
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
