"""文本模型白名单与上游 403 的区分（2026-10-13 站长实测踩到的坑）。

现场：站长用小百盒插件把 LLM 指向我们（`model: nai-glm-4-6`），我们返回
「502 上游拒绝了请求（HTTP 403），该账号已临时隔离保护」。查上游真值：
`nai-glm-4-6` / `clio-v1` / `nai-xialong` **上游根本不存在**，NovelAI 对「模型不存在」回的是 403，
被我们的账号保护逻辑当成「账号被拒」——既给成员误导，又会污染共享账号的健康度。
（实测能用的是 llama-3-erato-v1 与 kayra-v1；glm-4-6 只能走上游的 OpenAI 兼容接口。）
"""
import pathlib

import pytest

from app import main as gate_main
from app import nai
from app.routers import text_routes


def test_whitelist_only_contains_upstream_verified_models():
    assert text_routes.TEXT_MODELS == ["llama-3-erato-v1", "kayra-v1"]
    for retired in ("clio-v1", "nai-glm-4-6", "nai-xialong"):
        assert retired not in text_routes.TEXT_MODELS
        assert retired in text_routes.TEXT_MODELS_RETIRED
        assert text_routes.TEXT_MODELS_RETIRED[retired]        # 每条都要有给成员看的原因


@pytest.mark.parametrize("marker", [
    '{"statusCode":403,"message":"model \'nai-glm-4-6\' doesn\'t exist"}',
    '{"statusCode":403,"message":"model \'glm-4-6\' is only available via OpenAI-compatible API"}',
    '{"statusCode":400,"message":"model must be a valid enum value"}',
])
def test_upstream_model_level_rejections_are_recognised(marker):
    assert nai._is_model_level_403(marker) is True


def test_member_never_sees_upstream_error_text():
    """403 的响应体我们只用来分类，绝不回显给成员（老政策：不泄露上游原文）。"""
    source = pathlib.Path("app/nai.py").read_text(encoding="utf-8")
    assert "_model_403_reason" not in source          # 那个把上游原文拼进提示的函数已删掉
    assert "上游不认识" in source and "上游不认识这个模型：请换用本站白名单里的模型" in source


def test_account_level_403_is_not_mistaken_for_model_error():
    # 真正的账号/风控 403（Cloudflare 页面、没有模型字样）不能被误判
    assert nai._is_model_level_403("<html><title>403 Forbidden</title></html>") is False
    assert nai._is_model_level_403("") is False


class _Key(dict):
    """_require_text_model 只用到 key["is_admin"] 和 record()。"""

    def __init__(self, admin: bool):
        super().__init__(is_admin=admin, id=1, name="t")


@pytest.mark.asyncio
async def test_retired_model_is_rejected_locally_even_for_admin(monkeypatch):
    """站长号也一样本地拒绝：连测试也别往上打，否则又是一条会被误读的 403。"""
    recorded = []
    monkeypatch.setattr(gate_main, "record", lambda *a, **kw: recorded.append((a, kw)))
    with pytest.raises(Exception) as excinfo:
        gate_main._require_text_model(_Key(admin=True), "nai-glm-4-6", "text")
    assert "上游没有这个模型名" in str(excinfo.value)
    assert recorded and recorded[0][1].get("detail") == "model retired upstream"


@pytest.mark.asyncio
async def test_unknown_model_still_blocked_for_members(monkeypatch):
    monkeypatch.setattr(gate_main, "record", lambda *a, **kw: None)
    with pytest.raises(Exception):
        gate_main._require_text_model(_Key(admin=False), "gpt-4o", "text")
    # 站长（例如要试新模型）仍可放行未知名字
    gate_main._require_text_model(_Key(admin=True), "some-new-model", "text")
