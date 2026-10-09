"""Private bot bridge and public OAuth callback; never render API keys to a browser."""
from __future__ import annotations

import hmac

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse
from pydantic import BaseModel

from .policy import gen_key
from .registration import RegistrationError

router = APIRouter(prefix="/self-register")


class Intent(BaseModel):
    discord_id: str
    guild_id: str


def _service(request: Request):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "自助注册尚未配置")
    given = request.headers.get("Authorization", "")
    if not hmac.compare_digest(given, "Bearer " + service.bridge_secret):
        raise HTTPException(401, "未授权")
    return service


class Who(BaseModel):
    discord_id: str
    guild_id: str


def _checked(service, body: Who) -> str:
    if body.guild_id != service.command_guild or not body.discord_id.isdecimal():
        raise HTTPException(403, "请在指定服务器使用该命令。")
    return body.discord_id


@router.post("/info")
async def info(request: Request, body: Who):
    """机器人 /help 与 /status 使用：注册是否开放、新成员默认功能、记录声明、上游状态。"""
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
    gate = request.app.state.gate
    from . import features as feature_defs, ops
    on = body.value.lower() in ("1", "on", "true", "开")
    if body.action == "open":
        await ops.set_registration(gate.db, {"open": on})
        return JSONResponse({"message": "已开放注册。" if on else "已关闭注册（已领取的人不受影响）。"})
    if body.action == "limit":
        if not body.value.isdecimal():
            raise HTTPException(422, "请输入数字")
        await ops.set_registration(gate.db, {"max_users": int(body.value)})
        return JSONResponse({"message": f"名额上限已设为 {int(body.value) or '不限'}。"})
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
        "daily_v5": key["daily_v5"], "v5": counter["v5"],
        "image_model_scope": key["image_model_scope"],
        "features": [{"id": n, "label": feature_defs.FEATURES[n], "on": n in granted and flags[n]}
                     for n in feature_defs.FEATURES],
    }, headers={"Cache-Control": "no-store"})


@router.post("/resetkey")
async def resetkey(request: Request, body: Who):
    service = _service(request)
    key = await service.key_row_for(_checked(service, body))
    if key is None:
        raise HTTPException(404, "你还没有领取 Key，请先使用 /register。")
    token = gen_key("nai")
    await request.app.state.gate.db.rotate_key_token(key["id"], token)
    return JSONResponse({"key": token}, headers={"Cache-Control": "no-store"})


@router.post("/revoke")
async def revoke(request: Request, body: Who):
    service = _service(request)
    if not await service.revoke(_checked(service, body)):
        raise HTTPException(404, "该用户没有已领取的 Key。")
    return JSONResponse({"ok": True}, headers={"Cache-Control": "no-store"})


@router.post("/slots")
async def slots(request: Request, body: Who):
    service = _service(request)
    _checked(service, body)
    cfg = await service.settings()
    return JSONResponse({"active": await service.count_active(), "max": cfg["max_users"],
                         "open": cfg["open"], "reset_at": service.reset_at}, headers={"Cache-Control": "no-store"})


@router.post("/intent")
async def intent(request: Request, body: Intent):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "自助注册尚未配置")
    given = request.headers.get("Authorization", "")
    if not hmac.compare_digest(given, "Bearer " + service.bridge_secret):
        raise HTTPException(401, "未授权")
    try:
        url = await service.begin(body.discord_id, body.guild_id)
    except RegistrationError as exc:
        raise HTTPException(403, str(exc)) from exc
    return JSONResponse({"url": url}, headers={"Cache-Control": "no-store"})


@router.get("/callback")
async def callback(request: Request, code: str = "", state: str = "", error: str = ""):
    service = getattr(request.app.state, "registrar", None)
    if service is None:
        raise HTTPException(503, "自助注册尚未配置")
    headers = {"Cache-Control": "no-store", "Referrer-Policy": "no-referrer",
               "Content-Security-Policy": "default-src 'none'; style-src 'unsafe-inline'"}
    if error:
        return HTMLResponse("Discord 授权未完成，请重新使用 /register。", status_code=400, headers=headers)
    try:
        await service.finish(code, state)
    except RegistrationError as exc:
        return HTMLResponse(str(exc), status_code=403, headers=headers)
    return HTMLResponse("注册成功。API Key 和网址已发送到你的 Discord 私信，请勿分享 Key。", headers=headers)
