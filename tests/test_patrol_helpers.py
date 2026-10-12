"""巡检脚本里的纯逻辑（不连生产也能测）：告警降噪与 docker 时间解析。

2026-10-13 两次踩坑：
1. 告警去重按整段文本 → 次数一变就算新问题 → 同一件事每 10 分钟响一次；
2. docker 的 StartedAt 是 RFC3339Nano（9 位小数），fromisoformat 直接抛 ValueError →
   重启历史为空 → 部署窗口里的前端失败被当成故障。
"""
import importlib.util
import pathlib
import time

SCRIPT = pathlib.Path(__file__).resolve().parent.parent / "deploy" / "ops" / "patrol.py"
spec = importlib.util.spec_from_file_location("patrol_ops", SCRIPT)
patrol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(patrol)


def test_docker_time_parses_nanoseconds_and_plain_forms():
    expected = time.mktime(time.strptime("2026-10-13 01:43:42", "%Y-%m-%d %H:%M:%S"))
    for text in ("2026-10-12T17:43:42.835488585Z",      # docker 真实输出（纳秒）
                 "2026-10-12T17:43:42.835488Z",
                 "2026-10-12T17:43:42Z"):
        assert abs(patrol.parse_docker_time(text) - expected) < 1.5
    assert patrol.parse_docker_time("") == 0.0
    assert patrol.parse_docker_time("garbage") == 0.0


def test_known_items_are_shown_but_never_alert():
    rep = patrol.Report()
    rep.known("数据", "V5 超卖（按设计）")
    rep.add("warn", "通道", "文本成功率 50%", key="text-channel")
    assert rep.worst == "warn"                       # known 行是 ok 级，不抬高严重度
    assert [t for _, _, t in rep.problems()] == ["文本成功率 50%"]
    assert rep.keys == ["text-channel"]              # 告警去重只看标识，不含数字


def test_alert_keys_are_stable_when_numbers_change():
    first, second = patrol.Report(), patrol.Report()
    first.add("warn", "数据", "今日 112 张，上限 100", key="hourly")
    second.add("warn", "数据", "今日 350 张，上限 100", key="hourly")
    assert first.keys == second.keys                 # 次数变了也不该重复告警


def test_unknown_rows_without_key_fall_back_to_text():
    rep = patrol.Report()
    rep.add("red", "系统", "容器没运行")
    assert rep.keys == ["容器没运行"] and rep.worst == "red"
