from pathlib import Path
import re
import subprocess
import tempfile
import pytest

from app.status_stats import tracked


def test_status_stats_tracks_official_nai_and_v1_paths():
    # 常用出图与客户端路径均须被 status_stats 统计
    assert tracked("/ai/generate-image")
    assert tracked("/user/data")
    assert tracked("/v1/chat/completions")
    # /nai/ai/ 前缀由部分生图客户端（如 SillyTavern / 官方客户端）使用，双挂载在 main.py，必须纳入追踪
    assert tracked("/nai/ai/generate-image")
    assert tracked("/nai/ai/upscale")
    assert tracked("/nai/ai/encode-vibe")

    # 非成员调用（健康检查、静态文件、管理后台）不计入状态码统计
    assert not tracked("/healthz")
    assert not tracked("/public/live")
    assert not tracked("/status")
    assert not tracked("/admin")
    assert not tracked("/static/landing.html")


def test_landing_html_clean_and_valid():
    landing_path = Path("app/static/landing.html")
    assert landing_path.exists()
    content = landing_path.read_text(encoding="utf-8")

    # 死代码 bjMinute 已清理
    assert "bjMinute" not in content

    # 提取脚本并通过 node 语法校验
    scripts = re.findall(r"<script>(.*?)</script>", content, re.DOTALL)
    assert len(scripts) >= 2
    for s in scripts:
        with tempfile.NamedTemporaryFile("w", suffix=".js") as tmp:
            tmp.write(s)
            tmp.flush()
            res = subprocess.run(["node", "--check", tmp.name], capture_output=True, text=True)
            assert res.returncode == 0, f"JS syntax error: {res.stderr}"


def test_index_html_clean_and_valid():
    index_path = Path("app/static/index.html")
    assert index_path.exists()
    content = index_path.read_text(encoding="utf-8")

    # 死代码 memberLimitRatio / srcBadge 已清理
    assert "memberLimitRatio" not in content
    assert "function srcBadge" not in content

    # ensureMembers 确保完整载入成员与 Key 关联
    assert "async function ensureMembers(){" in content
    assert "await loadMembers()" in content

    # 新建 Key 功能权限单选框默认与 B5 规范及 openCreate 行为一致（custom 默认，而非 legacy）
    assert 'name="f_feat_mode" value="custom" checked' in content

    # 提取脚本并通过 node 语法校验
    scripts = re.findall(r"<script>(.*?)</script>", content, re.DOTALL)
    assert len(scripts) >= 1
    for s in scripts:
        with tempfile.NamedTemporaryFile("w", suffix=".js") as tmp:
            tmp.write(s)
            tmp.flush()
            res = subprocess.run(["node", "--check", tmp.name], capture_output=True, text=True)
            assert res.returncode == 0, f"JS syntax error: {res.stderr}"
