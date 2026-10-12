"""首页（landing.html）的契约测试：JS 找的东西必须真的存在，样式不能漏。

背景（2026-10-13 成员反馈）：
· 刷新时会闪一下「三步上手 / 使用须知」——登录态是等 /public/me 回来才被 JS 写上的。
  现在改成服务端首屏就写进 <body class="authed haskey">，这里把两条路都钉住。
· 页面是单文件内联 CSS/JS，很容易出现「JS 里用了某个 class，但样式里没有」这种漏项
  （已经抓到过 .invite-bind：邀请 Key 未绑定的提示条完全没样式）。这里做静态扫描，防止再漏。
"""
import re
from pathlib import Path

import httpx
import pytest
from fastapi import FastAPI

from app.routers import public_routes
from app.routers.public_routes import landing_html, router

LANDING = Path(__file__).resolve().parents[1] / "app" / "static" / "landing.html"

# 这些 class 由父级规则覆盖，不需要自己出现在选择器里
STYLED_BY_ANCESTOR = {"cp-use", "exp", "sc-cb", "sc-submit"}


def _split_source():
    src = LANDING.read_text(encoding="utf-8")
    css = re.search(r"<style>(.*?)</style>", src, re.S).group(1)
    body = src[src.index("</style>"):]
    return src, css, body


def test_every_id_referenced_by_js_exists():
    """$('x') / getElementById('x') 引用的 id 必须在 HTML 里（或由 JS 自己创建）。"""
    src, _css, body = _split_source()
    referenced = set(re.findall(r"\$\('([\w-]+)'\)", body)) | set(re.findall(r"getElementById\('([\w-]+)'\)", body))
    created_by_js = set(re.findall(r"id=[\"']([\w-]+)[\"']", body))          # JS 动态插入的节点
    html_ids = set(re.findall(r"\sid=[\"']([\w-]+)[\"']", src))
    missing = referenced - html_ids - created_by_js
    assert not missing, f"JS 引用了不存在的元素：{sorted(missing)}"


def test_every_class_used_by_js_is_styled():
    """JS 里出现过的 class 必须在样式表里有定义（否则就是没样式的裸元素）。"""
    src, css, body = _split_source()
    defined = set(re.findall(r"\.([a-zA-Z][\w-]*)", css))
    used = set()
    for match in re.findall(r"class=[\"']([^\"']+)[\"']", body):
        used.update(match.split())
    for match in re.findall(r"className\s*=\s*['\"]([^'\"]+)['\"]", body):
        used.update(match.split())
    used |= set(re.findall(r"querySelectorAll?\(['\"]\.([\w-]+)", body))
    used |= set(re.findall(r"classList\.(?:add|remove|toggle)\(['\"]([\w-]+)['\"]", body))
    missing = used - defined - STYLED_BY_ANCESTOR
    assert not missing, f"JS 用了但样式里没定义的 class：{sorted(missing)}"


def test_body_class_rules_exist_for_both_html_and_body():
    """服务端写的是 <body class>，旧的 JS 写的也是 body；两处规则都要在（缺一个就会闪）。"""
    _src, css, _body = _split_source()
    assert "body.authed .guest-only" in css and "html.authed .guest-only" in css
    assert "body.haskey #algoNote" in css


def test_landing_html_guest_is_untouched():
    page = landing_html("")
    assert "<body>" in page or "<body " in page
    assert "class=\"authed" not in page
    assert public_routes._ME_CARD in page                       # 骨架屏不能给游客
    assert public_routes._ME_CARD_SKELETON not in page


def test_landing_html_member_gets_class_and_skeleton():
    page = landing_html("authed haskey")
    assert '<body class="authed haskey">' in page
    assert page.count('id="meCard"') == 1
    assert public_routes._ME_CARD not in page                    # 换成骨架屏
    assert public_routes._ME_CARD_SKELETON in page
    # 只替换一次，不能把后面的内容也吃掉
    assert "三步上手" in page and "使用须知" in page


def _app(registrar=None):
    app = FastAPI()
    app.include_router(router)
    app.state.registrar = registrar
    return app


@pytest.mark.asyncio
async def test_index_guest_gets_plain_page():
    transport = httpx.ASGITransport(app=_app())
    async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
        response = await client.get("/")
    assert response.status_code == 200
    assert "class=\"authed" not in response.text
    assert response.headers["vary"].lower() == "cookie"          # 共享缓存不能串号
    assert "frame-ancestors 'none'" in response.headers["content-security-policy"]


@pytest.mark.asyncio
async def test_index_member_gets_first_paint_class(monkeypatch):
    """有会话时首屏就是成员版：不再等 JS 才隐藏新用户教程。"""
    async def fake_state(_request):
        return "123456789", {"token": "nai-abc"}

    monkeypatch.setattr("app.registration_routes.member_state", fake_state)
    transport = httpx.ASGITransport(app=_app())
    async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
        response = await client.get("/")
    assert response.status_code == 200
    assert '<body class="authed haskey">' in response.text
    assert public_routes._ME_CARD_SKELETON in response.text


@pytest.mark.asyncio
async def test_index_member_without_key_has_no_haskey(monkeypatch):
    async def fake_state(_request):
        return "123456789", None

    monkeypatch.setattr("app.registration_routes.member_state", fake_state)
    transport = httpx.ASGITransport(app=_app())
    async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
        response = await client.get("/")
    assert '<body class="authed">' in response.text
    assert "haskey" not in response.text.split("<body")[1].split(">")[0]
