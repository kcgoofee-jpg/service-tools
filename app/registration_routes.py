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


@router.post("/quota")
async def quota(request: Request, body: Who):
    service = _service(request)
    key = await service.key_row_for(_checked(service, body))
    if key is None:
        raise HTTPException(404, "你还没有领取 Key，请先使用 /register。")
    gate = request.app.state.gate
    counter = await gate.db.get_counter(key["id"], gate.day())
    return JSONResponse({
        "enabled": bool(key["enabled"]), "expires_at": key["expires_at"],
        "daily_images": key["daily_images"], "images": counter["images"],
        "daily_v5": key["daily_v5"], "v5": counter["v5"],
        "image_model_scope": key["image_model_scope"],
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
    return JSONResponse({"active": await service.count_active(), "max": service.max_users,
                         "reset_at": service.reset_at}, headers={"Cache-Control": "no-store"})


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
