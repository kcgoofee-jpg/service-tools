/**
 * 成员问卷（/survey）。身份来自 Discord 登录会话：先取 /public/me，登录后用 me.key 作为 Bearer 提交到
 * /member/survey（新旧网关都认这个头）。Key 只在这一次提交里用，页面不显示。
 */
import { useEffect, useId, useRef, useState } from "preact/hooks";
import { ApiError, getJSON } from "../../lib/api";
import { bootData } from "../../lib/boot";
import { Button, Card, Notice, Skeleton } from "../../ui";
import { displayName } from "../landing/helpers";
import { SiteFooter, Topbar } from "../landing/Topbar";
import type { Me, MeMember, Status } from "../landing/types";
import "../landing/landing.css";
import "./survey.css";
import {
  type Answers,
  type Choice,
  DONATE,
  EMPTY,
  IMPROVE,
  payload,
  UNDERSTAND,
  validate,
} from "./questions";

const isMe = (v: unknown): v is Me =>
  typeof v === "object" && v !== null && typeof (v as { logged_in?: unknown }).logged_in === "boolean";

/** 单选 / 多选题的一个选项：整行可点，选中高亮，键盘焦点可见。 */
function Option(props: {
  type: "radio" | "checkbox";
  name: string;
  choice: Choice;
  checked: boolean;
  required?: boolean;
  onChange: (checked: boolean) => void;
}) {
  const common = {
    name: props.name,
    value: props.choice.value,
    checked: props.checked,
    onChange: (e: Event) => props.onChange((e.target as HTMLInputElement).checked),
  };
  return (
    // biome-ignore lint/a11y/noLabelWithoutControl: input 在下面的条件分支里（radio / checkbox 两种类型），包在 label 内
    <label class="sv-opt">
      {props.type === "radio" ? (
        <input type="radio" required={props.required === true} {...common} />
      ) : (
        <input type="checkbox" {...common} />
      )}
      <span>{props.choice.label}</span>
    </label>
  );
}

async function submit(key: string, a: Answers): Promise<void> {
  let res: Response;
  try {
    res = await fetch("/member/survey", {
      method: "POST",
      credentials: "same-origin",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json",
        Authorization: `Bearer ${key}`,
      },
      body: JSON.stringify(payload(a)),
    });
  } catch {
    throw new ApiError(0, "网络错误，稍后再试");
  }
  if (res.ok) return;
  let msg = "提交失败";
  try {
    const b = (await res.json()) as { detail?: string; message?: string; error?: { message?: string } };
    msg = b.detail ?? b.message ?? b.error?.message ?? msg;
  } catch {
    /* 非 JSON 的错误页 */
  }
  if (res.status === 401) msg = "登录状态已过期或 Key 已更换，请刷新页面后重新登录再提交。";
  throw new ApiError(res.status, msg);
}

function Form({ me }: { me: MeMember }) {
  const [a, setA] = useState<Answers>(EMPTY);
  const [errs, setErrs] = useState<{ understand?: string; donate?: string }>({});
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  const [fail, setFail] = useState("");
  const ids = { u: useId(), uErr: useId(), uHint: useId(), d: useId(), dErr: useId(), dHint: useId() };
  const understandRef = useRef<HTMLFieldSetElement>(null);
  const donateRef = useRef<HTMLFieldSetElement>(null);
  const set = (patch: Partial<Answers>) => {
    setA((cur) => ({ ...cur, ...patch }));
    setDone(false);
  };

  const onSubmit = async (e: Event) => {
    e.preventDefault();
    const found = validate(a);
    setErrs(found);
    setFail("");
    const first = found.understand ? understandRef.current : found.donate ? donateRef.current : null;
    if (first) {
      first.querySelector<HTMLInputElement>("input")?.focus();
      return;
    }
    setBusy(true);
    try {
      await submit(me.key ?? "", a);
      setDone(true);
    } catch (x) {
      setFail(x instanceof Error ? x.message : "提交失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="stack" onSubmit={onSubmit} noValidate>
      <div class="card sv-q">
        <label class="field">
          <span class="field__label">1. 你主要用哪个客户端接入？（选填）</span>
          <input
            class="input"
            type="text"
            name="client"
            maxLength={60}
            autocomplete="off"
            placeholder="客户端名字"
            value={a.client}
            onInput={(e) => set({ client: (e.target as HTMLInputElement).value })}
          />
          <span class="field__hint">输入具体名字和版本更好，例如「柏宝绘 1.2」「Aaalice NAI 启动器」</span>
        </label>
      </div>

      <fieldset
        class="card sv-q"
        ref={understandRef}
        aria-describedby={`${ids.uHint}${errs.understand ? ` ${ids.uErr}` : ""}`}
      >
        <legend class="sv-legend">
          2. 你是否理解公益站限额的原理和初衷？<span class="sv-req">（必答）</span>
        </legend>
        <p class="sv-hint" id={ids.uHint}>
          限额是为了让一个共享账号撑住所有人：全站每日总量按时段平滑放行、每人每天有保底、超出只在空闲时放行。
        </p>
        {UNDERSTAND.map((c) => (
          <Option
            key={c.value}
            type="radio"
            name="understand"
            choice={c}
            required
            checked={a.understand === c.value}
            onChange={() => {
              set({ understand: c.value });
              setErrs((x) => ({ ...x, understand: "" }));
            }}
          />
        ))}
        {errs.understand && (
          <p class="lp-err" id={ids.uErr} role="alert">
            {errs.understand}
          </p>
        )}
      </fieldset>

      <fieldset class="card sv-q">
        <legend class="sv-legend">3. 你最希望改进哪些地方？（可多选）</legend>
        <p class="sv-hint">不是要更多额度——额度受上游账号物理限制。</p>
        {IMPROVE.map((c) => (
          <Option
            key={c.value}
            type="checkbox"
            name="improve"
            choice={c}
            checked={a.improve.includes(c.value)}
            onChange={(on) =>
              set({ improve: on ? [...a.improve, c.value] : a.improve.filter((v) => v !== c.value) })
            }
          />
        ))}
        <label class="field sv-other">
          <span class="field__label">其他想法（选填）</span>
          <textarea
            class="textarea"
            name="improve_other"
            maxLength={300}
            placeholder="具体说一处让你不爽的体验，最好带当时的截图或报错编号"
            value={a.improveOther}
            onInput={(e) => set({ improveOther: (e.target as HTMLTextAreaElement).value })}
          />
        </label>
      </fieldset>

      <fieldset
        class="card sv-q"
        ref={donateRef}
        aria-describedby={`${ids.dHint}${errs.donate ? ` ${ids.dErr}` : ""}`}
      >
        <legend class="sv-legend">
          4. 如果你自己购买了 NovelAI，愿意把 V4.5 的无限生成额度共享给大家用吗？
          <span class="sv-req">（必答）</span>
        </legend>
        <p class="sv-hint" id={ids.dHint}>
          V4.5 对订阅者是无限量的：捐出来只是让你的账号在站里多扛一部分 V4.5 生成，不影响你自己用 V5
          和付费功能，也不需要把整个账号交出来（接入方式可以再聊）。站里现在的算力瓶颈就在这——多一份
          V4.5，大家每天就多一份额度。
        </p>
        {DONATE.map((c) => (
          <Option
            key={c.value}
            type="radio"
            name="donate"
            choice={c}
            required
            checked={a.donate === c.value}
            onChange={() => {
              set({ donate: c.value });
              setErrs((x) => ({ ...x, donate: "" }));
            }}
          />
        ))}
        {errs.donate && (
          <p class="lp-err" id={ids.dErr} role="alert">
            {errs.donate}
          </p>
        )}
      </fieldset>

      <div class="card sv-q stack">
        <p class="muted small">
          身份来自你的 Discord 登录（{displayName(me)}），一人一份；再次提交会覆盖之前的回答。
        </p>
        {fail && <Notice tone="danger">{fail}</Notice>}
        {done && (
          <p class="sv-ok" role="status">
            已提交，谢谢你的反馈！
          </p>
        )}
        <div>
          <Button type="submit" variant="primary" size="lg" disabled={busy}>
            {busy ? "提交中…" : done ? "更新我的回答" : "提交问卷"}
          </Button>
        </div>
      </div>
    </form>
  );
}

export function Survey() {
  const boot = bootData<{ me?: unknown; status?: unknown }>();
  const [me, setMe] = useState<Me | undefined>(isMe(boot?.me) ? boot.me : undefined);
  const [webLogin, setWebLogin] = useState<boolean | undefined>(
    (boot?.status as Status | undefined)?.web_login,
  );

  useEffect(() => {
    if (me === undefined) {
      getJSON<Me>("/public/me", { cache: "no-store" })
        .then(setMe)
        .catch(() => setMe({ logged_in: false }));
    }
    if (webLogin === undefined) {
      getJSON<Status>("/public/status")
        .then((s) => setWebLogin(s.web_login === true))
        .catch(() => setWebLogin(false));
    }
  }, [me, webLogin]);

  const member = me?.logged_in ? me : null;
  return (
    <>
      <Topbar
        user={
          member
            ? { name: displayName(member), id: member.discord?.id, avatar: member.discord?.avatar }
            : undefined
        }
        loginOpen={webLogin === true && !member}
      />
      <main class="page sv-main">
        <header class="sv-head">
          <h1>成员问卷</h1>
          <p class="muted">大约 1 分钟。你的回答只有站长能看到，用于决定站里下一步优化什么。</p>
        </header>
        {me === undefined ? (
          <div class="stack" aria-busy="true">
            <span class="sr-only">正在读取…</span>
            <Skeleton h="120px" />
            <Skeleton h="220px" />
          </div>
        ) : !member ? (
          <Card title="请先登录">
            <div class="stack">
              <p>问卷用你的 Discord 登录身份识别，一人一份。</p>
              {webLogin === true ? (
                <div>
                  <a
                    class="btn btn--primary btn--lg"
                    href="/login"
                    onClick={() => {
                      try {
                        // 登录回调只会回到首页：首页看到这个标记会把人送回问卷
                        sessionStorage.setItem("owl-next", "/survey");
                      } catch {
                        /* 隐私模式：登录后自己再点进来 */
                      }
                    }}
                  >
                    用 Discord 登录
                  </a>
                </div>
              ) : (
                <p class="muted small">网页登录暂未开放，开放后再来填写。</p>
              )}
            </div>
          </Card>
        ) : !member.has_key ? (
          <Notice>你还没有 Key，所以暂时不能填写问卷。在 Discord 任意频道输入 /register 领取后再来。</Notice>
        ) : (
          <Form me={member} />
        )}
      </main>
      <SiteFooter note="问卷结果只有站长可见" />
    </>
  );
}
