/**
 * 后端调用。错误统一成 ApiError：网关的错误体是 {statusCode, message, error:{message, status, request_id}}
 * （与 NovelAI 官方格式一致，客户端直接显示 message）。
 */
export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public requestId?: string,
  ) {
    super(message);
  }
}

const NOTICE_PREFIX = "猫头鹰公益站提醒：";

async function parse(res: Response): Promise<unknown> {
  const text = await res.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  if (!res.ok) {
    const b = body as {
      message?: string;
      detail?: string;
      error?: { message?: string; request_id?: string };
    } | null;
    let msg = b?.error?.message ?? b?.message ?? b?.detail ?? `请求失败（HTTP ${res.status}）`;
    if (msg.startsWith(NOTICE_PREFIX)) msg = msg.slice(NOTICE_PREFIX.length);
    throw new ApiError(res.status, msg, b?.error?.request_id);
  }
  return body;
}

export async function getJSON<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, {
    credentials: "same-origin",
    headers: { Accept: "application/json" },
    ...init,
  });
  return (await parse(res)) as T;
}

export async function sendJSON<T>(method: string, url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    credentials: "same-origin",
    headers: {
      Accept: "application/json",
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
    },
    body: body === undefined ? null : JSON.stringify(body),
  });
  return (await parse(res)) as T;
}

/** 前端报错上报（与旧版同一接口与字段：page / msg / src / line / stack；只报本站脚本的错误）。 */
export function installErrorReporter(page: "landing" | "admin" | "survey"): void {
  let sent = 0;
  const report = (msg: string, src?: string, line?: number, stack?: string) => {
    if (sent++ > 5 || !msg || msg === "Script error.") return;
    const body = JSON.stringify({
      page,
      msg: msg.slice(0, 300),
      src: (src ?? location.pathname).slice(0, 200),
      line: line ?? 0,
      stack: (stack ?? "").slice(0, 1500),
    });
    navigator.sendBeacon?.("/public/client-error", new Blob([body], { type: "application/json" }));
  };
  addEventListener("error", (e) => report(e.message, e.filename, e.lineno, e.error?.stack));
  addEventListener("unhandledrejection", (e) =>
    report(String(e.reason?.message ?? e.reason), undefined, undefined, e.reason?.stack),
  );
}
