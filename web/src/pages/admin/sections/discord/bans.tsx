/** 封禁黑名单：满 3 天未活动自动回收或手动拉黑的账号；可搜索、手动加入、移出。 */
import { useEffect, useState } from "preact/hooks";
import { rel } from "../../../../lib/format";
import { Button, Card } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { ConfirmDialog, TableWrap, TextField, useBusy, useForm } from "../system/forms";
import { BAN_SPECS, type Ban, safeAvatar } from "./core";

function Avatar(props: { ban: Ban }) {
  const b = props.ban;
  const name = b.display_name || b.username || b.discord_id;
  const src = safeAvatar(b.avatar);
  return (
    <span class="avatar" aria-hidden="true">
      {[...name][0]}
      {src && <img src={src} alt="" width={28} height={28} referrerpolicy="no-referrer" loading="lazy" />}
    </span>
  );
}

function AddBan() {
  const form = useForm(BAN_SPECS, { discord_id: "", reason: "" });
  const [pending, setPending] = useState(false);
  const submit = async () => {
    const id = String(form.raw("discord_id")).trim();
    if (!form.valid || !id) return;
    setPending(true);
    const reason = String(form.raw("reason")).trim();
    const ok = await act(
      "POST",
      "/admin/api/bans",
      { discord_id: id, reason: reason || "站长手动封禁" },
      `已将 ${id} 加入黑名单`,
    );
    setPending(false);
    if (ok) form.reset();
  };
  return (
    <form
      class="form-section"
      {...form.formProps}
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <h3 class="form-section__title">手动加入黑名单</h3>
      <div class="form-grid form-grid--wide">
        <TextField form={form} name="discord_id" inputMode="numeric" />
        <TextField form={form} name="reason" />
      </div>
      <div class="savebar">
        <Button
          type="submit"
          variant="danger"
          disabled={pending || !form.dirty || !form.valid || !String(form.raw("discord_id")).trim()}
        >
          {pending ? "处理中…" : "加入黑名单"}
        </Button>
        <span class="muted small">加入后该账号无法再用 /register 领 Key。</span>
      </div>
    </form>
  );
}

export default function BansCard() {
  const [q, setQ] = useState("");
  const [applied, setApplied] = useState("");
  useEffect(() => {
    const t = setTimeout(() => setApplied(q.trim()), 300);
    return () => clearTimeout(t);
  }, [q]);
  const { data, error, loading } = useAdmin<{ bans: Ban[]; total: number }>(
    `/admin/api/bans${applied ? `?q=${encodeURIComponent(applied)}` : ""}`,
  );
  const [unban, setUnban] = useState<Ban | null>(null);
  const [searching, setSearching] = useState(false);
  useBusy(searching);
  const bans = data?.bans ?? [];
  return (
    <Card title="封禁黑名单" sub={data ? `共 ${data.total} 个账号` : undefined}>
      <div class="stack-sm">
        <div class="field">
          <label class="field__label" for="ban-search">
            搜索
          </label>
          <input
            id="ban-search"
            class="input"
            type="search"
            placeholder="Discord ID、用户名、昵称或封禁理由"
            value={q}
            onFocus={() => setSearching(true)}
            onBlur={() => setSearching(false)}
            onInput={(e) => setQ((e.target as HTMLInputElement).value)}
          />
        </div>
        {loading && !data && <Loading rows={2} />}
        {!data && error && <LoadError error={error} />}
        {data && !bans.length && (
          <p class="muted small">{applied ? "没有匹配的记录。" : "当前没有封禁记录。"}</p>
        )}
        {bans.length > 0 && (
          <TableWrap label="封禁黑名单">
            <table class="table">
              <caption class="sr-only">封禁黑名单</caption>
              <thead>
                <tr>
                  <th scope="col">账号</th>
                  <th scope="col">封禁原因与时间</th>
                  <th scope="col">
                    <span class="sr-only">操作</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {bans.map((b) => {
                  const title = b.display_name || b.username || b.discord_id;
                  return (
                    <tr key={b.discord_id}>
                      <td>
                        <div class="ban-who">
                          <Avatar ban={b} />
                          <div>
                            <div class="ban-name">{title}</div>
                            <div class="muted small mono">
                              {b.discord_id}
                              {b.username && b.username !== title ? ` · @${b.username}` : ""}
                            </div>
                          </div>
                        </div>
                      </td>
                      <td>
                        <div>{b.reason || "满 3 天未活动自动回收，或手动封禁"}</div>
                        <div class="muted small">
                          {rel(b.created_at)}
                          {b.banned_by ? ` · 经手：${b.banned_by}` : ""}
                        </div>
                      </td>
                      <td class="num">
                        <Button size="sm" onClick={() => setUnban(b)}>
                          移出黑名单
                        </Button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </TableWrap>
        )}
        <AddBan />
      </div>
      <ConfirmDialog
        open={unban !== null}
        title="移出黑名单？"
        confirmLabel="移出"
        onClose={() => setUnban(null)}
        onConfirm={async () => {
          if (!unban) return;
          const name = unban.display_name || unban.username || unban.discord_id;
          const ok = await act(
            "DELETE",
            `/admin/api/bans/${encodeURIComponent(unban.discord_id)}`,
            undefined,
            `已将「${name}」移出黑名单`,
          );
          if (ok) setUnban(null);
        }}
      >
        <p>
          「{unban?.display_name || unban?.username || unban?.discord_id}」移出后，可以重新在 Discord 里用
          /register 领取 Key。
        </p>
      </ConfirmDialog>
    </Card>
  );
}
