/**
 * 总览 / 成员 / 日志 三个分区共用的小零件（组件库没有的：分段按钮、确认框、分页、下载、窗口宽度）。
 * 只用 ui 组件库和 tokens.css 的变量；样式在 shared.css（类名前缀 xa-）。
 */
import { ChevronLeft, ChevronRight } from "lucide-preact";
import type { ComponentChildren } from "preact";
import { useEffect, useId, useRef, useState } from "preact/hooks";
import { ApiError } from "../../../../lib/api";
import { Button, cx, Dialog, Field, toast } from "../../../../ui";
import { busy, useAdmin } from "../../Admin";
import "./shared.css";

/* ---------------- hooks ---------------- */

/** 打开对话框 / 编辑表单期间暂停全局自动刷新（避免冲掉正在填的内容）。组件挂载期间有效。 */
export function useBusy(active = true): void {
  useEffect(() => {
    if (!active) return;
    busy.value++;
    return () => {
      busy.value--;
    };
  }, [active]);
}

/** 窗口宽度断点：只改布局（表格 ↔ 卡片），不改字号。 */
export function useMedia(query: string): boolean {
  const [match, setMatch] = useState(() => typeof matchMedia === "function" && matchMedia(query).matches);
  useEffect(() => {
    if (typeof matchMedia !== "function") return;
    const mq = matchMedia(query);
    const on = () => setMatch(mq.matches);
    on();
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, [query]);
  return match;
}
/** 表格改成卡片的宽度。侧栏在 860px 以上常驻，内容区比视口窄约 240px，所以表格要到 1100px 以上才放得下。 */
export const NARROW = "(max-width: 1100px)";

export function useDebounced<T>(value: T, ms = 250): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

/**
 * 带「正在换页」状态的取数：url 变化时旧数据保留（不闪烁），pending 为 true 直到新数据到达。
 * 仍然跟着全局刷新节拍。
 */
export function usePageData<T>(url: string): { data: T | null; error: string | null; pending: boolean } {
  const r = useAdmin<T>(url);
  const mark = useRef<{ url: string; data: T | null }>({ url, data: r.data });
  if (mark.current.url !== url) mark.current = { url, data: r.data };
  const pending = !r.error && (r.loading || r.data === mark.current.data);
  return { data: r.data, error: r.error, pending };
}

/** 未处理 Bug 数（日志页子导航的角标用）。侧栏「日志」角标由外壳的 BadgePoller 统一维护，这里只读。 */
export function useOpenBugs(): number {
  const { data } = useAdmin<{ open: number }>("/admin/api/errors");
  return data?.open ?? 0;
}

/* ---------------- 分段按钮 ---------------- */
export function Seg<T extends string>(props: {
  label: string;
  value: T;
  options: Array<{ id: T; label: ComponentChildren; title?: string; disabled?: boolean }>;
  onChange: (v: T) => void;
  class?: string;
}) {
  return (
    // biome-ignore lint/a11y/useSemanticElements: 分段按钮是一排并列按钮，用 fieldset 的默认版式不合适；role=group + aria-label 即标准写法
    <div class={cx("xa-seg", props.class)} role="group" aria-label={props.label}>
      {props.options.map((o) => (
        <button
          key={o.id}
          type="button"
          class="xa-seg__btn"
          aria-pressed={o.id === props.value}
          title={o.title}
          disabled={o.disabled}
          onClick={() => props.onChange(o.id)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/* ---------------- 确认框 ---------------- */
/**
 * 挂载即打开（父组件用 `{target && <ConfirmDialog …/>}` 控制）。
 * onConfirm 返回字符串 = 失败原因（显示在框里，保持打开）；返回 undefined = 成功，自动关闭。
 * 原生 <dialog> 在最上层，会盖住 toast，所以错误必须显示在框里。
 */
export function ConfirmDialog(props: {
  title: string;
  children: ComponentChildren;
  confirmText: string;
  danger?: boolean;
  /** 需要手动输入的确认词（高危操作） */
  phrase?: string;
  onConfirm: () => Promise<string | undefined> | string | undefined;
  onClose: () => void;
}) {
  useBusy();
  const [pending, setPending] = useState(false);
  const [err, setErr] = useState("");
  const [typed, setTyped] = useState("");
  const ok = !props.phrase || typed.trim() === props.phrase;
  const run = async () => {
    if (pending || !ok) return;
    setPending(true);
    setErr("");
    try {
      const r = await props.onConfirm();
      if (r === undefined) props.onClose();
      else setErr(r);
    } finally {
      setPending(false);
    }
  };
  return (
    <Dialog
      open
      title={props.title}
      onClose={props.onClose}
      footer={
        <>
          <Button onClick={props.onClose}>取消</Button>
          <Button variant={props.danger ? "danger" : "primary"} disabled={pending || !ok} onClick={run}>
            {pending ? "处理中…" : props.confirmText}
          </Button>
        </>
      }
    >
      <div class="xa-confirm">
        {props.children}
        {err && (
          <p class="xa-formerr" role="alert">
            {err}
          </p>
        )}
        {props.phrase && (
          <Field label={`请输入「${props.phrase}」确认`}>
            <input
              class="input"
              value={typed}
              autoComplete="off"
              onInput={(e) => setTyped((e.target as HTMLInputElement).value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  void run();
                }
              }}
            />
          </Field>
        )}
      </div>
    </Dialog>
  );
}

/* ---------------- 分页 ---------------- */
export function Pager(props: {
  page: number;
  pages: number;
  total: number;
  perPage: number;
  sizes?: number[];
  onPage: (p: number) => void;
  onSize?: (n: number) => void;
}) {
  const { page, pages, total, perPage } = props;
  const from = total ? (page - 1) * perPage + 1 : 0;
  const to = Math.min(total, page * perPage);
  const nums: Array<number | "gap-l" | "gap-r"> = [];
  for (let i = 1; i <= pages; i++) {
    if (i === 1 || i === pages || Math.abs(i - page) <= 1) {
      const last = nums[nums.length - 1];
      if (typeof last === "number" && i - last > 1) nums.push(i < page ? "gap-l" : "gap-r");
      nums.push(i);
    }
  }
  const jumpId = useId();
  return (
    <div class="xa-pager">
      <span class="xa-pager__info tabular">
        {from}–{to} / 共 {total}
      </span>
      {props.sizes && props.onSize && (
        <label class="xa-pager__size">
          每页
          <select
            class="select"
            value={perPage}
            onChange={(e) => props.onSize?.(Number((e.target as HTMLSelectElement).value))}
          >
            {props.sizes.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
      )}
      <span class="spacer" />
      {pages > 1 && (
        <nav class="xa-pager__nav" aria-label="分页">
          <Button
            size="sm"
            icon
            aria-label="上一页"
            disabled={page <= 1}
            onClick={() => props.onPage(page - 1)}
          >
            <ChevronLeft size={16} aria-hidden="true" />
          </Button>
          {nums.map((n) =>
            typeof n === "number" ? (
              <Button
                key={n}
                size="sm"
                variant={n === page ? "primary" : "default"}
                aria-label={`第 ${n} 页`}
                aria-current={n === page ? "page" : undefined}
                onClick={() => props.onPage(n)}
              >
                {n}
              </Button>
            ) : (
              <span key={n} class="muted" aria-hidden="true">
                …
              </span>
            ),
          )}
          <Button
            size="sm"
            icon
            aria-label="下一页"
            disabled={page >= pages}
            onClick={() => props.onPage(page + 1)}
          >
            <ChevronRight size={16} aria-hidden="true" />
          </Button>
          {pages > 10 && (
            <label class="xa-pager__jump" for={jumpId}>
              前往
              <input
                id={jumpId}
                class="input"
                type="number"
                min={1}
                max={pages}
                inputMode="numeric"
                onKeyDown={(e) => {
                  if (e.key !== "Enter") return;
                  const v = Math.max(1, Math.min(pages, Number((e.target as HTMLInputElement).value) || 1));
                  props.onPage(v);
                }}
              />
              页
            </label>
          )}
        </nav>
      )}
    </div>
  );
}

/**
 * 表格的滚动外框：表头粘在框顶（sticky），内容多时框内滚动。
 * 可滚动区域必须能用键盘聚焦（axe: scrollable-region-focusable），所以给 tabindex 和名称。
 */
export function TableScroll(props: {
  label: string;
  class?: string | undefined;
  children: ComponentChildren;
}) {
  return (
    // biome-ignore lint/a11y/noNoninteractiveTabindex: 可滚动区域需要键盘可聚焦，才能用方向键滚动
    <section class={cx("table-wrap xa-tbl", props.class)} tabIndex={0} aria-label={props.label}>
      {props.children}
    </section>
  );
}

/* ---------------- 零散 ---------------- */
export function Empty(props: { title: string; children?: ComponentChildren }) {
  return (
    <div class="xa-empty">
      <p class="xa-empty__title">{props.title}</p>
      {props.children && <p class="muted small">{props.children}</p>}
    </div>
  );
}

/** 头像：有 Discord 头像地址就显示，加载失败退回首字母。 */
export function Avatar(props: { name: string; src?: string | null | undefined; size?: number }) {
  const [failed, setFailed] = useState(false);
  const size = props.size ?? 32;
  const initial = [...(props.name || "?")][0] ?? "?";
  const ok = props.src?.startsWith("https://cdn.discordapp.com/") && !failed;
  return (
    <span class="xa-avatar" style={{ width: `${size}px`, height: `${size}px` }} aria-hidden="true">
      {initial}
      {ok && (
        <img
          src={props.src ?? ""}
          alt=""
          width={size}
          height={size}
          loading="lazy"
          referrerpolicy="no-referrer"
          onError={() => setFailed(true)}
        />
      )}
    </span>
  );
}

/** 一条用量小条：文字写明数字，条本身只是装饰。max=0 表示不限。 */
export function MiniBar(props: {
  label: string;
  used: number;
  max: number;
  tone?: "v5" | "warn";
  unlimited?: string;
}) {
  const pct = props.max > 0 ? Math.min(100, (props.used / props.max) * 100) : 0;
  const full = props.max > 0 && props.used >= props.max;
  return (
    <div class="xa-minibar">
      <span class="xa-minibar__l">{props.label}</span>
      <span class="xa-minibar__track" aria-hidden="true">
        <span
          class={cx("xa-minibar__fill", props.tone && `is-${props.tone}`, full && "is-full")}
          style={{ width: `${pct}%` }}
        />
      </span>
      <span class="xa-minibar__n tabular">
        {props.used}
        <span class="muted"> / {props.max > 0 ? props.max : (props.unlimited ?? "不限")}</span>
      </span>
    </div>
  );
}

/* ---------------- 下载（带错误提示） ---------------- */
function filenameOf(res: Response, fallback: string): string {
  const cd = res.headers.get("Content-Disposition") ?? "";
  const star = /filename\*=UTF-8''([^;]+)/i.exec(cd);
  if (star?.[1]) {
    try {
      return decodeURIComponent(star[1]);
    } catch {
      /* 退回下一种写法 */
    }
  }
  const plain = /filename="?([^";]+)"?/i.exec(cd);
  return plain?.[1] ?? fallback;
}

/** 导出文件：失败时（如 429 导出太频繁）用 toast 显示后端给的原因，而不是跳到一页 JSON。 */
export async function downloadFile(url: string, fallbackName: string): Promise<void> {
  try {
    const res = await fetch(url, { credentials: "same-origin" });
    if (!res.ok) {
      let msg = `导出失败（HTTP ${res.status}）`;
      try {
        const b = (await res.json()) as { message?: string; detail?: string; error?: { message?: string } };
        msg = b.error?.message ?? b.message ?? b.detail ?? msg;
      } catch {
        /* 保持默认文案 */
      }
      throw new ApiError(res.status, msg);
    }
    const blob = await res.blob();
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = filenameOf(res, fallbackName);
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 10_000);
  } catch (e) {
    toast(e instanceof Error ? e.message : String(e), { error: true });
  }
}

/**
 * 表单对话框的「防误关」：有未保存的修改时，Esc / 点遮罩不直接关，而是在框里问一句。
 * 原生 <dialog> 在 Esc 时已经关掉了，所以这里换 key 让 <Dialog> 重新挂载（表单状态在父组件里，不丢）。
 */
export function useCloseGuard(dirty: boolean, onClose: () => void) {
  const [gen, setGen] = useState(0);
  const [asking, setAsking] = useState(false);
  return {
    gen,
    asking,
    tryClose: () => {
      if (!dirty) return onClose();
      setAsking(true);
      setGen((g) => g + 1);
    },
    keep: () => setAsking(false),
    discard: onClose,
  };
}
