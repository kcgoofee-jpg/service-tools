/** 捐赠 Key：首页「我要捐赠」收到的号。验证可用后手动加进上游，点「已处理」前导航角标一直提醒。 */
import { useState } from "preact/hooks";
import { fullTime } from "../../../../lib/format";
import { Badge, Button, Card, CopyField, Dialog } from "../../../../ui";
import { act } from "../../Admin";
import { TableWrap, useBusy } from "../_kit";
import { type Donation, type Donations, maskToken } from "./core";

function DonationRow(props: { d: Donation; onHandle?: (d: Donation) => void }) {
  const d = props.d;
  return (
    <tr>
      <td class="nowrap" title={fullTime(d.ts)}>
        {fullTime(d.ts)}
      </td>
      <td class="wrap">{d.message || <span class="muted">—</span>}</td>
      <td class="mono">{d.contact || "—"}</td>
      <td>
        {/* 只显示头尾；复制按钮复制完整 Token，用来在「上游」页添加 */}
        <CopyField value={d.token} display={maskToken(d.token)} label="捐赠的 Token" />
      </td>
      <td>
        {props.onHandle ? (
          <Button size="sm" variant="primary" onClick={() => props.onHandle?.(d)}>
            已处理
          </Button>
        ) : (
          <span class="muted small">{d.handled_note || "已处理"}</span>
        )}
      </td>
    </tr>
  );
}

function Table(props: { rows: Donation[]; onHandle?: (d: Donation) => void; label: string }) {
  return (
    <TableWrap label={props.label}>
      <table class="table">
        <caption class="sr-only">{props.label}</caption>
        <thead>
          <tr>
            <th scope="col">时间</th>
            <th scope="col">留言 / 要求</th>
            <th scope="col">联系方式</th>
            <th scope="col">Token</th>
            <th scope="col">{props.onHandle ? "操作" : "备注"}</th>
          </tr>
        </thead>
        <tbody>
          {props.rows.map((d) => (
            <DonationRow key={d.id} d={d} {...(props.onHandle ? { onHandle: props.onHandle } : {})} />
          ))}
        </tbody>
      </table>
    </TableWrap>
  );
}

export default function DonationsCard(props: { data: Donations | null }) {
  const [target, setTarget] = useState<Donation | null>(null);
  const [note, setNote] = useState("");
  const [pending, setPending] = useState(false);
  useBusy(target !== null);
  const un = props.data?.unhandled ?? [];
  const ha = props.data?.handled ?? [];
  const close = () => {
    setTarget(null);
    setNote("");
  };
  return (
    <Card
      title={<>捐赠 Key {un.length > 0 && <Badge tone="danger">{un.length} 条待处理</Badge>}</>}
      sub="自己验证可用后手动加进「上游」；Token 只在后台显示，不写日志。"
    >
      {!un.length ? (
        <p class="muted small">没有待处理的捐赠。首页「我要捐赠」提交后会出现在这里。</p>
      ) : (
        <Table rows={un} onHandle={setTarget} label="待处理的捐赠" />
      )}
      {ha.length > 0 && (
        <details class="more" style={{ marginTop: "var(--sp-4)" }}>
          <summary class="small muted">已处理 {ha.length} 条</summary>
          <div style={{ marginTop: "var(--sp-3)" }}>
            <Table rows={ha} label="已处理的捐赠" />
          </div>
        </details>
      )}
      <Dialog
        open={target !== null}
        title="标记为已处理"
        onClose={close}
        footer={
          <>
            <Button onClick={close} disabled={pending}>
              取消
            </Button>
            <Button
              variant="primary"
              disabled={pending}
              onClick={async () => {
                if (!target) return;
                setPending(true);
                const ok = await act(
                  "POST",
                  `/admin/api/donations/${target.id}/handle`,
                  { note: note.trim() },
                  "已标记为已处理",
                );
                setPending(false);
                if (ok) close();
              }}
            >
              {pending ? "处理中…" : "标记已处理"}
            </Button>
          </>
        }
      >
        <div class="field">
          <label class="field__label" for="don-note">
            备注（可选）
          </label>
          <input
            id="don-note"
            class="input"
            type="text"
            maxLength={200}
            placeholder="例如：已验证接入 / 已联系 / 不可用"
            value={note}
            onInput={(e) => setNote((e.target as HTMLInputElement).value)}
          />
          <span class="field__hint">标记后导航角标不再为它提醒。</span>
        </div>
      </Dialog>
    </Card>
  );
}
