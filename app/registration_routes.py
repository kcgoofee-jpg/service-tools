"""Private bot bridge and public OAuth callback; never render API keys to a browser."""
from __future__ import annotations

import hmac

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse, Response
from pydantic import BaseModel

from .action_log import log_action
from .policy import gen_key
from .registration import RegistrationError

router = APIRouter(prefix="/self-register")


class Intent(BaseModel):
    discord_id: str
    guild_id: str
    name: str = ""          # Discord 用户名，仅用于候补名单展示


def _service(request: Request):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "自助领 Key 尚未配置")
    given = request.headers.get("Authorization", "")
    if not hmac.compare_digest(given.encode(), ("Bearer " + service.bridge_secret).encode()):
        gate = getattr(request.app.state, "gate", None)
        if gate is not None and request.client:          # 猜桥接密钥也计入无效请求限流
            getattr(gate, "record_auth_failure", lambda _ip: None)(request.client.host)
        raise HTTPException(401, "未授权")
    return service


def _admin_actor(service, body) -> None:
    """管理类操作需要“发起者”在管理员白名单（ADMIN_DISCORD_IDS）里；白名单为空则一律拒绝（默认安全）。"""
    actor = getattr(body, "actor_id", "")
    if not service.admin_ids or actor not in service.admin_ids:
        raise HTTPException(403, "没有管理员权限。")


class Who(BaseModel):
    discord_id: str
    guild_id: str
    actor_id: str = ""        # 实际发出命令的 Discord 用户（由机器人填写，管理类操作据此校验）


async def _log_bot(request: Request, who: str, action: str, target: str = "", detail: str = "") -> None:
    db = getattr(getattr(request.app.state, "gate", None), "db", None)
    if db is not None:
        await log_action(db, f"Discord:{who}", action, target, detail)


def _checked(service, body: Who) -> str:
    if body.guild_id != service.command_guild or not body.discord_id.isdecimal():
        raise HTTPException(403, "请在指定服务器使用该命令。")
    return body.discord_id


@router.post("/info")
async def info(request: Request, body: Who):
    """机器人 /help 与 /quota 使用：注册是否开放、新成员默认功能、记录声明、上游状态。"""
    service = _service(request)
    _checked(service, body)
    gate = request.app.state.gate
    from . import features as feature_defs
    from .audit import audit_flags, audit_notice
    cfg = await service.settings()
    flags = await feature_defs.global_flags(gate.db)
    defaults = cfg["features"] if cfg["features"] is not None else [n for n in feature_defs.FEATURES if flags[n]]
    return JSONResponse({
        "open": cfg["open"], "active": await service.count_active(), "max": cfg["max_users"],
        "default_features": [{"id": n, "label": feature_defs.FEATURES[n], "on": flags[n]} for n in defaults],
        "daily_images": cfg["daily_images"], "daily_v5": cfg["daily_v5"],
        "notice": audit_notice(*(await audit_flags(gate.db, gate.settings))),
        "upstream": gate.upstream_health(),
    }, headers={"Cache-Control": "no-store"})


class Ops(Who):
    action: str
    value: str = ""
    target: str = ""      # 目标成员的 Discord ID（grant）
    feature: str = ""


@router.post("/ops")
async def admin_ops(request: Request, body: Ops):
    """管理员命令（Discord 端已限制为"管理服务器"权限）：开关注册、名额、授权功能、记录开关。"""
    service = _service(request)
    _checked(service, body)
    _admin_actor(service, body)
    gate = request.app.state.gate
    from . import features as feature_defs, ops
    on = body.value.lower() in ("1", "on", "true", "开")
    target = f"Discord:{body.target}" if body.target else ""
    params = " ".join(x for x in (body.feature, body.value) if x)
    try:
        response = await _run_op(request, body, service, gate, on)
    except HTTPException as exc:
        db = getattr(gate, "db", None)
        if db is not None:
            await log_action(db, f"Discord:{body.actor_id}", f"机器人·{body.action}", target,
                             f"失败（{exc.status_code}）：{exc.detail}"[:200], ok=False)
        raise
    await _log_bot(request, body.actor_id, f"机器人·{body.action}", target, params)
    return response


async def _run_op(request: Request, body, service, gate, on: bool):
    from . import features as feature_defs, ops
    if body.action == "open":
        await ops.set_registration(gate.db, {"open": on}, gate, service)
        return JSONResponse({"message": "已开放领 Key。" if on else "已关闭领 Key（已领取的人不受影响）。"})
    if body.action == "limit":
        if not body.value.isdecimal():
            raise HTTPException(422, "请输入数字")
        await ops.set_registration(gate.db, {"max_users": int(body.value)}, gate, service)
        cap = await ops.v5_capacity(gate.db, gate.settings, service)
        tip = f"\n⚠ {cap['message']}（全站 V5 在网页后台「设置」修改）" if cap["short"] else ""
        return JSONResponse({"message": f"名额上限已设为 {int(body.value) or '不限'}。" + tip})
    if body.action == "grant":
        if body.feature not in feature_defs.FEATURES:
            raise HTTPException(422, "未知功能：" + ",".join(feature_defs.FEATURES))
        key = await service.key_row_for(body.target)
        if key is None:
            raise HTTPException(404, "该成员还没有领取 Key。")
        current = feature_defs.key_features(key)
        names = set(feature_defs.FEATURES if current is None else current)
        (names.add if on else names.discard)(body.feature)
        await gate.db.update_key(key["id"], {"features": feature_defs.dump(names)})
        return JSONResponse({"message": f"已{'开通' if on else '关闭'}：{feature_defs.FEATURES[body.feature]}。"})
    if body.action == "ban":
        if not body.target.isdecimal():
            raise HTTPException(422, "缺少目标成员")
        await service.ban(body.target)
        return JSONResponse({"message": "已永久禁止该账号领取，并撤销其 Key 与身份组。"})
    if body.action == "unban":
        done = await service.unban(body.target)
        return JSONResponse({"message": "已解除禁止。" if done else "该账号不在禁止名单中。"})
    if body.action == "audit":
        result = await ops.set_audit(gate, {"prompts": on, "thumbs": on, "notify": True})
        return JSONResponse({"message": "已开启生成记录并通知成员。" if on else "已关闭生成记录并通知成员。",
                             **result})
    raise HTTPException(422, "未知操作")


@router.post("/quota")
async def quota(request: Request, body: Who):
    service = _service(request)
    key = await service.key_row_for(_checked(service, body))
    if key is None:
        raise HTTPException(404, "你还没有领取 Key，请先使用 /register。")
    gate = request.app.state.gate
    from . import features as feature_defs
    flags = await feature_defs.global_flags(gate.db)
    own = feature_defs.key_features(key)
    granted = set(feature_defs.FEATURES if own is None else own)
    counter = await gate.db.get_counter(key["id"], gate.day())
    return JSONResponse({
        "enabled": bool(key["enabled"]), "expires_at": key["expires_at"],
        "daily_images": key["daily_images"], "images": counter["images"],
        "daily_images_base": _base(gate, key),
        "daily_v5": key["daily_v5"], "v5": counter["v5"],
        "image_model_scope": key["image_model_scope"],
        "features": [{"id": n, "label": feature_defs.FEATURES[n], "on": n in granted and flags[n]}
                     for n in feature_defs.FEATURES],
    }, headers={"Cache-Control": "no-store"})


def _base(gate, key) -> int:
    guard = getattr(gate, "guard", None)
    base = guard.values["base_daily_images"] if guard is not None else 0
    return base if base and key["daily_images"] and base < key["daily_images"] else 0


@router.post("/resetkey")
async def resetkey(request: Request, body: Who):
    service = _service(request)
    key = await service.key_row_for(_checked(service, body))
    if key is None:
        raise HTTPException(404, "你还没有领取 Key，请先使用 /register。")
    token = gen_key("nai")
    await request.app.state.gate.db.rotate_key_token(key["id"], token)
    await _log_bot(request, body.discord_id, "成员重置 Key（/resetkey）", f"Key #{key['id']} {key['name']}")
    return JSONResponse({"key": token}, headers={"Cache-Control": "no-store"})


@router.post("/revoke")
async def revoke(request: Request, body: Who):
    service = _service(request)
    _checked(service, body)
    _admin_actor(service, body)
    if not await service.revoke(body.discord_id):
        raise HTTPException(404, "该用户没有已领取的 Key。")
    await _log_bot(request, body.actor_id, "机器人·revoke（撤销 Key、释放名额）", f"Discord:{body.discord_id}")
    return JSONResponse({"ok": True}, headers={"Cache-Control": "no-store"})


@router.post("/slots")
async def slots(request: Request, body: Who):
    service = _service(request)
    _checked(service, body)
    _admin_actor(service, body)
    cfg = await service.settings()
    wl = await service.waitlist()
    return JSONResponse({"active": await service.count_active(), "max": cfg["max_users"],
                         "open": cfg["open"], "reset_at": service.reset_at,
                         "waitlist": len(wl), "invited": sum(1 for w in wl if w["invited_at"])},
                        headers={"Cache-Control": "no-store"})


@router.post("/intent")
async def intent(request: Request, body: Intent):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "自助领 Key 尚未配置")
    given = request.headers.get("Authorization", "")
    if not hmac.compare_digest(given, "Bearer " + service.bridge_secret):
        raise HTTPException(401, "未授权")
    try:
        url = await service.begin(body.discord_id, body.guild_id, body.name[:80])
    except RegistrationError as exc:
        await _log_bot(request, body.discord_id, "领取 Key 失败（/register）", "", str(exc)[:200])
        raise HTTPException(403, str(exc)) from exc
    return JSONResponse({"url": url}, headers={"Cache-Control": "no-store"})


@router.get("/callback")
async def callback(request: Request, code: str = "", state: str = "", error: str = ""):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "自助领 Key 尚未配置")
    headers = {"Cache-Control": "no-store", "Referrer-Policy": "no-referrer",
               "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'"}
    if error:
        return HTMLResponse("Discord 授权未完成，请重新使用 /register。", status_code=400, headers=headers)
    who = (service.pending.get(state) or ("未知",))[0]
    try:
        await service.finish(code, state)
    except RegistrationError as exc:
        await _log_bot(request, who, "领取 Key 失败（授权回调）", "", str(exc)[:200])
        return HTMLResponse(str(exc), status_code=403, headers=headers)
    return HTMLResponse("领取成功。Key 和网址已发送到你的 Discord 私信，请勿分享 Key。", headers=headers)


class BotReport(BaseModel):
    status: dict | None = None
    event: dict | None = None


@router.get("/bot/config")
async def bot_config_get(request: Request):
    """机器人每分钟来读一次配置（后台「Discord」页可改，不用重启机器人）。"""
    _service(request)
    from . import bot_config
    return JSONResponse(await bot_config.load(request.app.state.gate.db), headers={"Cache-Control": "no-store"})


@router.post("/bot/report")
async def bot_report(request: Request, body: BotReport):
    """机器人上报心跳和动作（点赞 / 评论），后台显示在线状态和最近记录。"""
    _service(request)
    from . import bot_config
    await bot_config.report(request.app.state.gate.db, body.status, body.event)
    return {"ok": True}


# ============ 网页「用 Discord 登录」（成员端，复用现有 OAuth；不粘贴 Key 即可看状态） ============
import hashlib
import hmac as _hmac
import secrets
import time as _time

from fastapi.responses import RedirectResponse

member_router = APIRouter()
MEMBER_COOKIE = "owl_member"
STATE_COOKIE = "owl_login_state"
SESSION_DAYS = 30


def _member_secret(request: Request) -> str:
    from .admin import _secret
    return _secret(request) + ":member-login"


def _sign_member(request: Request, discord_id: str, exp: int) -> str:
    payload = f"{discord_id}:{exp}"
    sig = _hmac.new(_member_secret(request).encode(), payload.encode(), hashlib.sha256).hexdigest()
    return payload + "." + sig


def _member_session(request: Request) -> Optional[str]:
    raw = request.cookies.get(MEMBER_COOKIE, "")
    if "." not in raw:
        return None
    payload, sig = raw.rsplit(".", 1)
    good = _hmac.new(_member_secret(request).encode(), payload.encode(), hashlib.sha256).hexdigest()
    try:
        if not _hmac.compare_digest(sig, good):
            return None
        discord_id, exp = payload.rsplit(":", 1)
        return discord_id if int(exp) > _time.time() else None
    except (ValueError, TypeError):
        return None


def _set_cookie(response, name: str, value: str, max_age: int) -> None:
    response.set_cookie(name, value, max_age=max_age, httponly=True, secure=True,
                        samesite="lax", path="/")


@member_router.get("/login")
async def login_start(request: Request):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "登录尚未配置")
    state = secrets.token_urlsafe(24)
    resp = RedirectResponse(service.web_login_url(state))
    _set_cookie(resp, STATE_COOKIE, state, 600)
    return resp


@member_router.get("/login/callback")
async def login_callback(request: Request, code: str = "", state: str = ""):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "登录尚未配置")
    if not code or not state or state != request.cookies.get(STATE_COOKIE, ""):
        return RedirectResponse("/?login=failed")
    try:
        who = await service.web_identify(code)
    except RegistrationError:
        return RedirectResponse("/?login=failed")
    resp = RedirectResponse("/")
    exp = int(_time.time()) + SESSION_DAYS * 86400
    _set_cookie(resp, MEMBER_COOKIE, _sign_member(request, who["id"], exp), SESSION_DAYS * 86400)
    resp.delete_cookie(STATE_COOKIE, path="/")
    await _log_bot(request, who["id"], "网页登录")
    return resp


@member_router.post("/logout")
async def logout(request: Request):
    resp = JSONResponse({"ok": True})
    resp.delete_cookie(MEMBER_COOKIE, path="/")
    return resp


@member_router.get("/public/me")
async def public_me(request: Request):
    """登录后的个人状态：不用粘贴 Key 就能看额度、排队、到期；并提供自己的 Key 供一键复制。"""
    discord_id = _member_session(request)
    if discord_id is None:
        return JSONResponse({"logged_in": False}, headers={"Cache-Control": "no-store"})
    service = getattr(request.app.state, "registrar", None)
    gate = request.app.state.gate
    reg = await service.registration_profile(discord_id) if service else None
    key = await service.key_row_for(discord_id) if service else None
    out: dict[str, Any] = {"logged_in": True, "discord": reg}
    if key is None or not key["enabled"]:
        out["has_key"] = False
    else:
        c = await gate.db.get_counter(key["id"], gate.day())
        guard = getattr(gate, "guard", None)
        qv = guard.queue_view(key["id"]) if guard is not None else {}
        try:
            img_days = int(float(await gate.db.get_setting("audit_image_retention_days", 3) or 3))
        except (TypeError, ValueError):
            img_days = 3
        out.update(has_key=True, key=key["token"], name=key["name"],
                   expires_at=key["expires_at"], image_scope=key["image_model_scope"],
                   today={"images": c["images"], "v5": c["v5"],
                          "daily_images": key["daily_images"], "daily_v5": key["daily_v5"]},
                   queue=qv.get("mine", []),
                   images_stored=await gate.db.audit_image_count(key["id"]), image_retention_days=img_days)
    return JSONResponse(out, headers={"Cache-Control": "no-store"})


_EXPORT_AT: dict[str, float] = {}      # 每人上次打包时间（内存）：打包较重，限 1 次 / 60 秒


@member_router.get("/public/my-export")
async def my_export(request: Request, format: str = "zip"):
    """成员自助打包自己的全部原图：format=epub（电子书）或 zip（原图）。单次一种。"""
    import time as _t
    from starlette.concurrency import run_in_threadpool
    from . import exporter
    discord_id = _member_session(request)
    if discord_id is None:
        raise HTTPException(401, "请先用 Discord 登录")
    if format not in ("zip", "epub"):
        raise HTTPException(400, "format 只能是 zip 或 epub")
    now = _t.time()
    if now - _EXPORT_AT.get(discord_id, 0) < 60:
        raise HTTPException(429, "打包有点重，请 1 分钟后再试")
    service = getattr(request.app.state, "registrar", None)
    key = await service.key_row_for(discord_id) if service is not None else None
    if key is None:
        raise HTTPException(404, "你还没有领取 Key")
    db = request.app.state.gate.db
    rows = await db.audit_images_for(key["id"])
    if not rows:
        raise HTTPException(404, "暂时没有可打包的原图（原图只保留最近几天）")
    _EXPORT_AT[discord_id] = now
    who = key["name"] or "我的作品"
    if format == "epub":
        blob = await run_in_threadpool(exporter.build_epub, rows, who)
        media, ext = "application/epub+zip", "epub"
    else:
        blob = await run_in_threadpool(exporter.build_zip, rows, who)
        media, ext = "application/zip", "zip"
    fname = f"owl-{len(rows)}.{ext}"
    return Response(blob, media_type=media,
                    headers={"Content-Disposition": f'attachment; filename="{fname}"', "Cache-Control": "no-store"})
