"""巡检脚本的纯逻辑：结论分级、队列判定、告警去重（不碰网络与数据库）。"""
import importlib.util
import io
import json
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]


def _load():
    spec = importlib.util.spec_from_file_location("patrol", ROOT / "deploy/ops/patrol.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


patrol = _load()


def test_report_worst_and_problems():
    rep = patrol.Report()
    assert rep.worst == patrol.OK                     # 什么都没有 = 正常
    rep.add(patrol.OK, "系统", "容器运行中")
    assert rep.worst == patrol.OK and not rep.problems()
    rep.add(patrol.WARN, "注册", "名额承诺超过容量")
    rep.add(patrol.RED, "系统", "/healthz 无响应")
    assert rep.worst == patrol.RED
    assert [r[1] for r in rep.problems()] == ["注册", "系统"]


def test_status_of_queue_severity():
    assert patrol.status_of_queue({})[0] == patrol.WARN                       # 读不到 = 注意
    assert patrol.status_of_queue({"global": {"active": 1, "waiting": 0}})[0] == patrol.OK
    assert patrol.status_of_queue({"global": {"active": 1, "waiting": 25}})[0] == patrol.WARN
    assert patrol.status_of_queue({"global": {}, "image_cooldown_remaining": 30})[0] == patrol.WARN
    assert patrol.status_of_queue({"global": {"active": 2, "waiting": 3},
                                   "image_cooldown_remaining": 0})[0] == patrol.OK


def test_exit_code_rank():
    assert patrol.RANK == {patrol.OK: 0, patrol.WARN: 1, patrol.RED: 2}


def test_maybe_alert_dedupes_and_cools_down(tmp_path, monkeypatch):
    """同一问题在冷却期内只发一次；问题变了要能再发。"""
    sent = []
    monkeypatch.setattr(patrol, "STATE_DIR", tmp_path)
    monkeypatch.setattr(patrol, "COOLDOWN", 1800)
    monkeypatch.setattr(patrol, "send_alert", lambda text: (sent.append(text), True)[1])

    rep = patrol.Report()
    rep.add(patrol.WARN, "注册", "名额承诺超过容量")
    patrol.maybe_alert(rep, "第一次")
    patrol.maybe_alert(rep, "同一问题，30 分钟内不重复")
    assert len(sent) == 1

    rep2 = patrol.Report()
    rep2.add(patrol.RED, "系统", "/healthz 无响应")
    patrol.maybe_alert(rep2, "换了问题")
    assert len(sent) == 2

    # 冷却到期后同一问题可以再发
    mark = tmp_path / "last_alert.json"
    data = json.loads(mark.read_text())
    mark.write_text(json.dumps({"digest": data["digest"], "at": time.time() - 3600}))
    patrol.maybe_alert(rep2, "冷却过后")
    assert len(sent) == 3


def test_maybe_alert_skips_when_send_fails(tmp_path, monkeypatch):
    monkeypatch.setattr(patrol, "STATE_DIR", tmp_path)
    monkeypatch.setattr(patrol, "send_alert", lambda text: False)
    rep = patrol.Report()
    rep.add(patrol.WARN, "系统", "读不到容器状态")
    patrol.maybe_alert(rep, "发送失败")
    assert not (tmp_path / "last_alert.json").exists()      # 失败不写冷却，下轮还会重试


@pytest.mark.parametrize("raw,expected", [("12", 12), ("abc", 0), (None, 0), ("79.6", 79)])
def test_num_tolerates_junk(raw, expected):
    assert patrol.num(raw, 0) == expected


def test_send_alert_uses_discord_compatible_user_agent(monkeypatch):
    """Discord 前面有 Cloudflare：默认的 Python-urllib UA 会被 403（error code 1010）。

    2026-10-13 线上实测告警发不出去就是这个原因，所以这里把请求头锁死。
    """
    captured = []

    class Response:
        status = 200

        def read(self):
            return b'{"id": "123"}'

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

    def fake_urlopen(request, timeout=10):
        captured.append(request)
        return Response()

    monkeypatch.setattr(patrol.urllib.request, "urlopen", fake_urlopen)
    env = {"DISCORD_BOT_TOKEN": "t" * 60, "ALERT_USER_ID": "123456", "ALERT_CHANNEL_ID": ""}
    monkeypatch.setattr(patrol, "env_value", lambda key: env.get(key, ""))

    assert patrol.send_alert("测试告警") is True
    assert len(captured) == 2                       # 先建私信频道，再发消息
    for request in captured:
        headers = {key.lower(): value for key, value in request.headers.items()}
        assert headers["user-agent"].startswith("DiscordBot (")
        assert headers["accept"] == "application/json"
        assert headers["authorization"].startswith("Bot ")


def test_send_alert_reports_cloudflare_block(monkeypatch, capsys):
    """403 + 1010 要在 stderr 里说清是 UA 被拦，否则下次还会查半天。"""
    def fake_urlopen(request, timeout=10):
        raise patrol.urllib.error.HTTPError("https://discord.com", 403, "Forbidden", {},
                                            io.BytesIO(b"error code: 1010"))

    monkeypatch.setattr(patrol.urllib.request, "urlopen", fake_urlopen)
    env = {"DISCORD_BOT_TOKEN": "t" * 60, "ALERT_USER_ID": "123456", "ALERT_CHANNEL_ID": ""}
    monkeypatch.setattr(patrol, "env_value", lambda key: env.get(key, ""))

    assert patrol.send_alert("测试") is False
    assert "1010" in capsys.readouterr().err


def test_send_alert_without_config(monkeypatch, capsys):
    monkeypatch.setattr(patrol, "env_value", lambda key: "")
    assert patrol.send_alert("测试") is False
    assert "未配置告警通道" in capsys.readouterr().err
