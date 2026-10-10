"""UPSTREAM_TLS_IMPERSONATE：默认关闭时上游客户端与原来完全一致；开启时经 curl_cffi 传输层；导入失败回退 httpx。"""
import asyncio
import json
import sys
import types
from types import SimpleNamespace

import httpx
import pytest

from app import nai as nai_mod
from app import tls_impersonate as tls
from app.nai import NaiClient


def _client(tls_value=None):
    return NaiClient(["pst-fixture-a", "pst-fixture-b", "pst-fixture-c"], "https://image.invalid",
                     "https://text.invalid", "https://text.invalid", db=None, day_fn=lambda: "2026-10-10",
                     v5_daily_limits=[], allow_anlas=[], image_min_interval=0, tls_impersonate=tls_value)


@pytest.fixture(autouse=True)
def _restore_profiles(monkeypatch):
    # 开启开关会就地改写 BROWSER_PROFILES；每个测试用副本，结束后自动还原
    monkeypatch.setattr(nai_mod, "BROWSER_PROFILES", [dict(p) for p in nai_mod.BROWSER_PROFILES])


# ---------------- 假 curl_cffi ----------------

class FakeRequestException(Exception):
    def __init__(self, msg, code=0, response=None):
        super().__init__(msg)
        self.code = code


class FakeHeaders:
    def __init__(self, items):
        self._items = items

    def multi_items(self):
        return list(self._items)


class FakeCurlResponse:
    def __init__(self, status, headers, chunks, *, finished=True):
        self.status_code = status
        self.headers = FakeHeaders(headers)
        self.http_version = 3
        self.reason = "OK"
        self._chunks = chunks
        self.quit_now = asyncio.Event()
        self.curl = object()
        loop = asyncio.get_running_loop()
        self.astream_task = loop.create_future()
        if finished:
            self.astream_task.set_result(None)

    async def aiter_content(self):
        for chunk in self._chunks:
            if isinstance(chunk, Exception):
                raise chunk
            yield chunk


class FakeAsyncSession:
    instances: list = []

    def __init__(self, **kwargs):
        self.kwargs = kwargs
        self.calls = []
        self.removed = []
        self.closed = False
        self.reply = None
        self.acurl = SimpleNamespace(remove_handle=self.removed.append)
        FakeAsyncSession.instances.append(self)

    async def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        reply = self.reply() if callable(self.reply) else self.reply
        if isinstance(reply, Exception):
            raise reply
        return reply

    async def close(self):
        self.closed = True


class FakeCurl:
    def impersonate(self, target, default_headers=True):
        return 0 if target.startswith("chrome") else 1

    def close(self):
        pass


@pytest.fixture
def fake_curl_cffi(monkeypatch):
    FakeAsyncSession.instances = []
    root = types.ModuleType("curl_cffi")
    root.__version__ = "0.16.3"
    root.Curl = FakeCurl
    req = types.ModuleType("curl_cffi.requests")
    req.AsyncSession = FakeAsyncSession
    imp = types.ModuleType("curl_cffi.requests.impersonate")
    imp.resolve_latest_browser_type = lambda v: "chrome136" if v == "chrome" else v
    exc = types.ModuleType("curl_cffi.requests.exceptions")
    exc.RequestException = FakeRequestException
    root.requests = req
    req.impersonate = imp
    req.exceptions = exc
    for name, mod in {"curl_cffi": root, "curl_cffi.requests": req,
                      "curl_cffi.requests.impersonate": imp,
                      "curl_cffi.requests.exceptions": exc}.items():
        monkeypatch.setitem(sys.modules, name, mod)
    return FakeAsyncSession


# ---------------- (a) 关闭：与原来一致 ----------------

@pytest.mark.asyncio
async def test_off_uses_plain_httpx_and_never_touches_curl_cffi(monkeypatch):
    monkeypatch.setitem(sys.modules, "curl_cffi", None)      # 关闭时连导入都不应发生
    before = [dict(p) for p in nai_mod.BROWSER_PROFILES]
    for value in (None, "", "   "):
        client = _client(value)
        assert client.tls_target is None and client.tls_error is None and client.tls_requested is None
        await client.start()
        try:
            assert type(client._client._transport) is httpx.AsyncHTTPTransport
            assert client._client.trust_env is True and client._client.follow_redirects is True
        finally:
            await client.close()
    assert nai_mod.BROWSER_PROFILES == before
    assert "Chrome/129" in client.pool[0].browser_profile["user_agent"]


# ---------------- (b) 开启：经 curl 传输层 ----------------

@pytest.mark.asyncio
async def test_on_routes_through_curl_transport_with_streaming(fake_curl_cffi):
    client = _client("chrome")
    assert client.tls_target == "chrome136" and client.tls_error is None
    # 请求头里的 Chrome 版本对齐到 TLS 目标
    profile = client.pool[0].browser_profile
    assert "Chrome/136.0.0.0" in profile["user_agent"] and "Windows" in profile["user_agent"]
    assert profile["sec_ch_ua"] == '"Chromium";v="136", "Google Chrome";v="136", "Not.A/Brand";v="99"'
    assert '"macOS"' == client.pool[2].browser_profile["sec_ch_ua_platform"]
    assert "Chrome/136" in nai_mod.default_browser_headers("t")["User-Agent"]

    await client.start()
    assert isinstance(client._client._transport, tls.CurlCffiTransport)
    assert client._client.trust_env is False
    resp_obj = FakeCurlResponse(200, [("Content-Type", "text/event-stream"), ("Content-Encoding", "gzip"),
                                      ("Content-Length", "999"), ("X-Req", "1")],
                                [b"data: a\n\n", b"data: b\n\n"])
    req = client._client.build_request("POST", "https://image.invalid/ai/generate-image-stream",
                                       json={"input": "x"}, headers=client._headers(client.pool[0], "text/event-stream"))
    session = None
    try:
        # 会话延迟到第一次请求才建
        assert fake_curl_cffi.instances == []
        client._client._transport._get_session().reply = resp_obj
        session = fake_curl_cffi.instances[0]
        assert session.kwargs["impersonate"] == "chrome136"
        assert session.kwargs["default_headers"] is False and session.kwargs["allow_redirects"] is False
        resp = await client._client.send(req, stream=True)
        assert resp.status_code == 200 and resp.http_version == "HTTP/2"
        assert resp.headers["x-req"] == "1" and resp.headers["content-type"] == "text/event-stream"
        assert "content-encoding" not in resp.headers and "content-length" not in resp.headers
        body = b"".join([c async for c in resp.aiter_bytes()])
        assert body == b"data: a\n\ndata: b\n\n"
        await resp.aclose()
    finally:
        await client.close()
    method, url, kw = session.calls[0]
    assert method == "POST" and url.endswith("/ai/generate-image-stream")
    sent = {k.lower(): v for k, v in kw["headers"]}
    assert sent["authorization"] == "Bearer pst-fixture-a" and "Chrome/136" in sent["user-agent"]
    assert sent["content-type"] == "application/json"
    for dropped in ("host", "content-length", "accept-encoding", "connection"):
        assert dropped not in sent
    assert json.loads(kw["data"]) == {"input": "x"}
    assert kw["stream"] is True and kw["accept_encoding"] == tls.CHROME_ACCEPT_ENCODING
    assert kw["timeout"] == (15, 300)
    assert session.closed is True


@pytest.mark.asyncio
async def test_on_proxy_goes_to_curl_and_early_close_aborts_transfer(fake_curl_cffi):
    client = NaiClient(["pst-a"], "https://image.invalid", "", "", db=None, day_fn=lambda: "d",
                       v5_daily_limits=[], allow_anlas=[], proxy="socks5h://127.0.0.1:1080",
                       tls_impersonate="chrome131")
    await client.start()
    try:
        session = client._client._transport._get_session()
        assert session.kwargs["proxy"] == "socks5h://127.0.0.1:1080"
        assert client._client._mounts == {}              # 没有 httpx 代理传输层绕过 curl
        pending = FakeCurlResponse(200, [], [b"first"], finished=False)
        session.reply = pending
        resp = await client._client.send(client._client.build_request("GET", "https://image.invalid/x"), stream=True)
        async for _ in resp.aiter_bytes():
            break
        close = asyncio.ensure_future(resp.aclose())
        await asyncio.sleep(0)
        assert pending.quit_now.is_set() and session.removed == [pending.curl]
        pending.astream_task.set_result(None)
        await close
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_on_maps_curl_errors_to_httpx(fake_curl_cffi):
    client = _client("chrome")
    await client.start()
    try:
        session = client._client._transport._get_session()
        cases = [(28, "Connection timed out after 15001 milliseconds", httpx.ConnectTimeout),
                 (28, "Operation too slow", httpx.ReadTimeout),
                 (7, "Failed to connect", httpx.ConnectError),
                 (6, "Could not resolve host", httpx.ConnectError),
                 (97, "proxy", httpx.ProxyError),
                 (56, "Recv failure", httpx.ReadError)]
        for code, msg, expected in cases:
            session.reply = FakeRequestException(msg, code)
            with pytest.raises(expected):
                await client._client.get("https://image.invalid/user/subscription")
        # 流式中途出错：一律按读错误（nai.py 据此判断「可能已扣费」）
        session.reply = lambda: FakeCurlResponse(200, [], [b"x", FakeRequestException("Operation too slow", 28)])
        resp = await client._client.send(client._client.build_request("GET", "https://image.invalid/s"), stream=True)
        with pytest.raises(httpx.ReadTimeout):
            async for _ in resp.aiter_bytes():
                pass
        session.reply = lambda: FakeCurlResponse(200, [], [FakeRequestException("Connection reset", 7)])
        resp = await client._client.send(client._client.build_request("GET", "https://image.invalid/s"), stream=True)
        with pytest.raises(httpx.ReadError):
            await resp.aread()
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_verify_token_works_through_adapter(fake_curl_cffi):
    client = _client("chrome")
    await client.start()
    try:
        session = client._client._transport._get_session()
        session.reply = lambda: FakeCurlResponse(200, [("content-type", "application/json")],
                                                 [b'{"tier": 3, "active": true}'])
        assert await client.verify_token("pst-new") == {"ok": True, "tier": 3}
        assert session.calls[-1][2]["timeout"] == (12, 12)
    finally:
        await client.close()


# ---------------- (c) 导入失败 / 版本过旧 / 目标不支持：回退 httpx ----------------

@pytest.mark.asyncio
async def test_import_failure_falls_back_to_httpx(monkeypatch, caplog):
    monkeypatch.setitem(sys.modules, "curl_cffi", None)
    before = [dict(p) for p in nai_mod.BROWSER_PROFILES]
    with caplog.at_level("WARNING"):
        client = _client("chrome")
    assert client.tls_target is None and "导入失败" in client.tls_error
    assert any("UPSTREAM_TLS_IMPERSONATE" in r.getMessage() for r in caplog.records)
    assert nai_mod.BROWSER_PROFILES == before
    await client.start()
    try:
        assert type(client._client._transport) is httpx.AsyncHTTPTransport
    finally:
        await client.close()


def test_old_version_and_unknown_target_fall_back(fake_curl_cffi, monkeypatch):
    client = _client("firefox999")
    assert client.tls_target is None and "不支持" in client.tls_error
    monkeypatch.setattr(sys.modules["curl_cffi"], "__version__", "0.13.0")
    client = _client("chrome")
    assert client.tls_target is None and "过旧" in client.tls_error
    assert "Chrome/129" in client.pool[0].browser_profile["user_agent"]


# ---------------- 其它 ----------------

@pytest.mark.parametrize("major,expected", [
    (128, '"Chromium";v="128", "Not;A=Brand";v="24", "Google Chrome";v="128"'),
    (129, '"Google Chrome";v="129", "Not=A?Brand";v="8", "Chromium";v="129"'),
    (131, '"Google Chrome";v="131", "Chromium";v="131", "Not_A Brand";v="24"'),
    (133, '"Not(A:Brand";v="99", "Google Chrome";v="133", "Chromium";v="133"'),
    (136, '"Chromium";v="136", "Google Chrome";v="136", "Not.A/Brand";v="99"'),
])
def test_sec_ch_ua_matches_real_chrome(major, expected):
    assert tls.chrome_sec_ch_ua(major) == expected


def test_original_profiles_still_match_the_algorithm():
    for p in nai_mod._BASE_BROWSER_PROFILES:
        major = int(p["user_agent"].split("Chrome/")[1].split(".")[0])
        assert p["sec_ch_ua"] == tls.chrome_sec_ch_ua(major)


def test_risk_check_reports_state(fake_curl_cffi):
    from app.risk_check import _tls_fingerprint_item
    off = _tls_fingerprint_item(SimpleNamespace(nai=SimpleNamespace(pool=[])))
    assert off["status"] == "warn" and "Chrome" in off["detail"]
    client = _client("chrome")
    on = _tls_fingerprint_item(SimpleNamespace(nai=client))
    assert on["status"] == "ok" and "chrome136" in on["detail"]
    client.pool[0].browser_profile["user_agent"] = "Mozilla/5.0 Chrome/120.0.0.0"
    assert _tls_fingerprint_item(SimpleNamespace(nai=client))["status"] == "warn"
    failed = SimpleNamespace(tls_target=None, tls_requested="chrome", tls_error="curl_cffi 导入失败（ImportError）")
    item = _tls_fingerprint_item(SimpleNamespace(nai=failed))
    assert item["status"] == "warn" and "回退" in item["detail"] and item["evidence"] == [failed.tls_error]


@pytest.mark.asyncio
async def test_anlas_fetch_account_uses_given_client():
    from app.anlas_pool import fetch_account
    seen = []

    def handler(request):
        seen.append(request)
        return httpx.Response(200, json={"trainingStepsLeft": {"fixedTrainingStepsLeft": 100,
                                                               "purchasedTrainingSteps": 5},
                                         "expiresAt": 1800000000})
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as shared:
        account = await fetch_account("https://image.invalid", "pst-a", client=shared)
    assert account == {"anlas": 105, "refill_at": 1800000000.0}
    assert seen[0].headers["authorization"] == "Bearer pst-a"


@pytest.mark.asyncio
async def test_real_curl_cffi_against_local_server():
    """真 curl_cffi（装了才跑）：本机 HTTP 服务，验证流式 body、状态码、请求头经过适配层。"""
    pytest.importorskip("curl_cffi")
    target, error = tls.resolve_target("chrome")
    if target is None:
        pytest.skip(error)
    got = {}

    async def handle(reader, writer):
        head = await reader.readuntil(b"\r\n\r\n")
        got["head"] = head.decode()
        length = int(next((l.split(":")[1] for l in got["head"].split("\r\n")
                           if l.lower().startswith("content-length")), "0"))
        got["body"] = await reader.readexactly(length) if length else b""
        writer.write(b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nX-Up: yes\r\n"
                     b"Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n")
        await writer.drain()
        writer.close()

    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    try:
        async with httpx.AsyncClient(transport=tls.CurlCffiTransport(target), trust_env=False,
                                     timeout=httpx.Timeout(5)) as client:
            resp = await client.post(f"http://127.0.0.1:{port}/p", json={"k": 1},
                                     headers={"User-Agent": "UA-test", "X-Mine": "1"})
    finally:
        server.close()
        await server.wait_closed()
    assert resp.status_code == 201 and resp.text == "hello world" and resp.headers["x-up"] == "yes"
    head = got["head"].lower()
    assert "user-agent: ua-test" in head and "x-mine: 1" in head
    assert "accept-encoding: gzip, deflate, br, zstd" in head
    assert "upgrade-insecure-requests" not in head and "sec-fetch-user" not in head
    assert json.loads(got["body"]) == {"k": 1}
