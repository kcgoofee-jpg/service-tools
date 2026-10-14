import { useEffect, useId, useRef, useState } from "preact/hooks";
import { ApiError, sendJSON } from "../../lib/api";
import { Button, Card, toast } from "../../ui";

type Errors = { token?: string; message?: string };

/** 校验文案与旧版首页一致（后端另有 20–4000 字符 / 500 字留言的硬限制，错误直接显示后端的说法）。 */
export function validateDonation(token: string, message: string): Errors {
  const errs: Errors = {};
  if (token.trim().length < 20) errs.token = "Token 好像不完整：请从 NovelAI 官网 Account 页整段复制";
  if (!message.trim()) errs.message = "请写一句留言 / 使用要求（站长好接进合适的池子）";
  return errs;
}

export function Donate() {
  const [open, setOpen] = useState(false);
  const [token, setToken] = useState("");
  const [contact, setContact] = useState("");
  const [message, setMessage] = useState("");
  const [errors, setErrors] = useState<Errors>({});
  const [busy, setBusy] = useState(false);
  const tokenRef = useRef<HTMLTextAreaElement>(null);
  const messageRef = useRef<HTMLTextAreaElement>(null);
  const formId = useId();
  const tokenErr = useId();
  const messageErr = useId();

  useEffect(() => {
    if (open) tokenRef.current?.focus();
  }, [open]);

  const close = () => {
    setOpen(false);
    // 收起后把焦点还给触发按钮
    setTimeout(() => document.getElementById("donate-toggle")?.focus(), 0);
  };

  const submit = async (e: Event) => {
    e.preventDefault();
    const errs = validateDonation(token, message);
    setErrors(errs);
    if (errs.token) return tokenRef.current?.focus();
    if (errs.message) return messageRef.current?.focus();
    setBusy(true);
    try {
      await sendJSON("POST", "/public/donate", {
        token: token.trim(),
        message: message.trim(),
        contact: contact.trim(),
      });
      setToken("");
      setContact("");
      setMessage("");
      close();
      toast("已收到，谢谢！站长会在后台验证后接入。");
    } catch (x) {
      toast(x instanceof ApiError ? x.message : "网络异常，稍后再试", { error: true });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card
      id="donate"
      tone="accent"
      title="捐赠 NovelAI 账号"
      class="lp-donate"
      actions={
        !open && (
          <Button
            id="donate-toggle"
            variant="primary"
            aria-expanded={false}
            aria-controls={formId}
            onClick={() => setOpen(true)}
          >
            我要捐赠
          </Button>
        )
      }
    >
      <p class="lp-donate__intro">
        本站所有出图额度都来自大家捐赠的账号。<b>V4.5 无限出图</b>
        的账号最帮得上忙；可以附一句使用要求（比如「只用
        4.5」「周末别用」）。提交只给站长看，人工验证后才接入。
      </p>
      {open && (
        <form id={formId} class="lp-form" onSubmit={submit} autocomplete="off" noValidate>
          <label class="field">
            <span class="field__label">NovelAI Token（必填）</span>
            <textarea
              ref={tokenRef}
              class="textarea"
              name="token"
              rows={3}
              spellcheck={false}
              autocomplete="off"
              placeholder="粘贴整段 Token（约几百个字符）"
              value={token}
              aria-invalid={errors.token ? "true" : undefined}
              aria-describedby={`${errors.token ? `${tokenErr} ` : ""}${formId}-token-hint`}
              onInput={(e) => setToken((e.target as HTMLTextAreaElement).value)}
            />
            <span class="field__hint" id={`${formId}-token-hint`}>
              登录 novelai.net → Account 页整段复制，只给站长看。
            </span>
            {errors.token && (
              <span class="lp-err" id={tokenErr} role="alert">
                {errors.token}
              </span>
            )}
          </label>
          <label class="field">
            <span class="field__label">联系方式（选填）</span>
            <input
              class="input"
              name="contact"
              maxLength={100}
              autocomplete="off"
              placeholder="Discord ID / QQ，方便站长有疑问时找你"
              value={contact}
              onInput={(e) => setContact((e.target as HTMLInputElement).value)}
            />
          </label>
          <label class="field">
            <span class="field__label">留言 / 使用要求（必填）</span>
            <textarea
              ref={messageRef}
              class="textarea"
              name="message"
              rows={2}
              maxLength={500}
              placeholder="比如：只用 V4.5 / 周末别用 / 高峰期留给别人…"
              value={message}
              aria-invalid={errors.message ? "true" : undefined}
              aria-describedby={errors.message ? messageErr : undefined}
              onInput={(e) => setMessage((e.target as HTMLTextAreaElement).value)}
            />
            {errors.message && (
              <span class="lp-err" id={messageErr} role="alert">
                {errors.message}
              </span>
            )}
          </label>
          <div class="row">
            <Button type="submit" variant="primary" disabled={busy}>
              {busy ? "提交中…" : "提交给站长"}
            </Button>
            <Button onClick={close} disabled={busy}>
              收起
            </Button>
          </div>
        </form>
      )}
    </Card>
  );
}
