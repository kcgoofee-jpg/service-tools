/** 公告：首页的站点公告（HTML，带隔离预览）+ 让奶妹把一条消息发到 Discord 公告频道。 */
import { useState } from "preact/hooks";
import { Button, Card } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { ConfirmDialog, SaveBar, TextAreaField, useBusy, useForm } from "../system/forms";
import { ANNOUNCE_SPECS, DC_ANNOUNCE_MAX } from "./core";

function SiteAnnouncement() {
  const { data, error, loading } = useAdmin<{ html: string }>("/admin/api/announcement");
  const form = useForm(ANNOUNCE_SPECS, data);
  const [saving, setSaving] = useState(false);
  const [preview, setPreview] = useState(false);
  if (loading && !data)
    return (
      <Card title="站点公告">
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title="站点公告">
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );
  const html = String(form.raw("html"));
  const save = async () => {
    setSaving(true);
    // 公告允许清空：空字符串也合法，所以直接取原始输入
    const ok = await act("PUT", "/admin/api/announcement", { html }, "公告已保存");
    setSaving(false);
    if (ok) form.commit();
  };
  return (
    <Card
      title="站点公告"
      sub="成员打开首页时看到的内容。"
      actions={
        <Button variant="ghost" aria-pressed={preview} onClick={() => setPreview((p) => !p)}>
          {preview ? "收起预览" : "预览效果"}
        </Button>
      }
    >
      <form {...form.formProps} onSubmit={(e) => e.preventDefault()}>
        <TextAreaField form={form} name="html" rows={9} mono placeholder="<h2>欢迎</h2><p>……</p>" />
        {preview && (
          <div class="announce-preview">
            <div class="muted small">预览（仅供排版参考；脚本不会执行）</div>
            {/* 空 sandbox：不给脚本、不给表单、不同源——公告 HTML 可能是从别处粘贴来的 */}
            <iframe
              sandbox=""
              title="公告预览"
              class="announce-preview__frame"
              srcDoc={html || '<p style="font-family:system-ui;color:#888">（公告内容为空）</p>'}
            />
          </div>
        )}
        <SaveBar form={form} onSave={save} saving={saving} label="保存公告" />
      </form>
    </Card>
  );
}

function DiscordSend() {
  const [text, setText] = useState("");
  const [open, setOpen] = useState(false);
  const [focused, setFocused] = useState(false);
  useBusy(focused || text.length > 0);
  const t = text.trim();
  const tooLong = t.length > DC_ANNOUNCE_MAX;
  return (
    <Card title="让奶妹发 Discord 公告" sub="立即发一条消息到社区公告频道；纯文本，@ 不会真的提示任何人。">
      <div class="field">
        <label class="field__label" for="dc-announce">
          公告内容
        </label>
        <textarea
          id="dc-announce"
          class="textarea"
          rows={3}
          placeholder="例如：今晚 3-7 点安静时段照常，明早 7 点额度刷新。"
          value={text}
          aria-invalid={tooLong ? "true" : undefined}
          aria-describedby="dc-announce-h"
          onFocus={() => setFocused(true)}
          onBlur={() => setFocused(false)}
          onInput={(e) => setText((e.target as HTMLTextAreaElement).value)}
        />
        <span class="field__hint tnum" id="dc-announce-h">
          {t.length} / {DC_ANNOUNCE_MAX} 字
          {tooLong ? "：太长了，Discord 单条上限 2000 字符，这里留了余量" : ""}
        </span>
      </div>
      <div class="savebar">
        <Button variant="primary" disabled={!t || tooLong} onClick={() => setOpen(true)}>
          发送到公告频道
        </Button>
      </div>
      <ConfirmDialog
        open={open}
        title="发到公告频道？"
        confirmLabel="立即发送"
        onClose={() => setOpen(false)}
        onConfirm={async () => {
          const ok = await act(
            "POST",
            "/admin/api/announcement/send",
            { text: t },
            "已交给奶妹，公告频道几秒内可见",
          );
          if (ok) {
            setText("");
            setOpen(false);
          }
        }}
      >
        <p>奶妹会立即把下面这条消息发到公告频道，发出后无法撤回：</p>
        <blockquote class="quote">{t}</blockquote>
      </ConfirmDialog>
    </Card>
  );
}

export default function Announce() {
  return (
    <>
      <SiteAnnouncement />
      <DiscordSend />
    </>
  );
}
