/**
 * 成员相关的写操作。统一返回 {ok, data} / {ok:false, error}：
 * 原生 <dialog> 盖在 toast 上面，所以出错要把原因交给对话框自己显示，成功再由调用方 toast。
 * 成功后触发一次全局刷新（tick）。
 */
import { ApiError, sendJSON } from "../../../../lib/api";
import { tick } from "../../Admin";
import type { Coupon, KeyRow, Role } from "./types";

export type Res<T = unknown> = { ok: true; data: T } | { ok: false; error: string };

export async function call<T = unknown>(
  method: "POST" | "PUT" | "PATCH" | "DELETE",
  url: string,
  body?: unknown,
): Promise<Res<T>> {
  try {
    const data = await sendJSON<T>(method, url, body);
    tick.value++;
    return { ok: true, data };
  } catch (e) {
    // 登录过期：触发一次刷新，数据钩子读到 401 会回到登录页
    if (e instanceof ApiError && e.status === 401) tick.value++;
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

const A = "/admin/api";

// 后台操作不再私信成员（成员私信出口已删）。旧网关默认会私信，所以这几个请求仍明确带上 notify=0；
// 等后端不再认这个参数后可以一起删。
export const disableKey = (id: number) => call("PATCH", `${A}/keys/${id}`, { enabled: false, notify: false });

export const removeMember = (id: number, ban: boolean) =>
  call("DELETE", `${A}/keys/${id}?ban=${ban ? "true" : "false"}&notify=0`);

export const regenerateKey = (id: number) =>
  call<{ token: string }>("POST", `${A}/keys/${id}/regenerate?notify=0`);

export const resetDailyQuota = (id: number) =>
  call("POST", `${A}/keys/${id}/reset-daily-image-quota?notify=0`);

export const switchRole = (id: number, role: Role) => call("POST", `${A}/keys/${id}/role`, { role });

export const setQuotaAuto = (id: number) =>
  call("PATCH", `${A}/keys/${id}`, { quota: "auto", notify: false });

export const addTag = (id: number, tag: string, note: string) =>
  call("PUT", `${A}/keys/${id}/tags`, { tag, note });
export const removeTag = (id: number, tag: string) =>
  call("DELETE", `${A}/keys/${id}/tags/${encodeURIComponent(tag)}`);
export const clearShare = (id: number) => call("POST", `${A}/keys/${id}/share-clear`);

export const giveCoupon = (id: number, days: number, note: string) =>
  call<{ coupon: Coupon }>("POST", `${A}/keys/${id}/coupons`, { kind: "reset", days, note });

export const bindDiscord = (id: number, discordId: string, username: string) =>
  call("POST", `${A}/keys/${id}/bind-discord`, { discord_id: discordId, username });

export const createInvite = () => call<{ key: KeyRow }>("POST", `${A}/keys/invite`, {});

export const resetAllKeys = () =>
  call<{ message?: string }>("POST", `${A}/ops/reset-all-keys`, { confirm: "确认重置" });

export const addBan = (discordId: string, reason: string) =>
  call("POST", `${A}/bans`, { discord_id: discordId, reason });
export const removeBan = (discordId: string) => call("DELETE", `${A}/bans/${encodeURIComponent(discordId)}`);

export const gift = (body: { discord_id: string; username?: string; reason: string; coupon_days: number }) =>
  call<{ new_key?: boolean; key_id?: number; key?: string }>("POST", `${A}/gift`, body);
