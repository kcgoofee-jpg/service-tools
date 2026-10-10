import time

import pytest

from app import status_page as sp
from app.database import Database


@pytest.mark.asyncio
async def test_incident_lifecycle_banner_bars_and_history(tmp_path):
    # 站长 10/10：状态页照 status.claude.com——未解决事件在页首横幅，组件条按当天最严重事件着色，历史按天列
    db = Database(str(tmp_path / "s.sqlite"))
    await db.connect()
    try:
        await sp.ensure_schema(db)
        now = time.time()
        a = await sp.create_incident(db, "部署期间出图接口报错", "major", ["api"], "investigating", "正在调查", now - 3600)
        await sp.add_update(db, a, "identified", "已定位：快照锁库", now - 3000)
        await sp.add_update(db, a, "resolved", "已修复", now - 1800)
        b = await sp.create_incident(db, "V5 额度口径偏差", "minor", ["api"], "monitoring", "已上线修复，观察 0 点复盘", now - 600)
        from app import status_stats
        status_stats._PENDING.clear()
        status_stats.record(200, now - 60); status_stats.record(500, now - 60)   # 和线上一样经 status_stats 写入（hour = 整点秒数）
        await status_stats.flush(db)
        snap = await sp.snapshot(db, now)
        api = next(c for c in snap["components"] if c["key"] == "api")
        assert [i["id"] for i in snap["active"]] == [b] and api["status"] == "性能下降"
        assert api["days"][-1]["level"] == 2                     # 今天最严重的是 major（已解决的也算进当天颜色）
        assert api["days"][0]["level"] is None                   # 记录开始之前：灰色无数据
        assert 0 < api["uptime"] < 100
        page = sp.render(snap)
        assert "V5 额度口径偏差" in page and "观察中" in page and "已定位" in page and "所有服务运行正常" not in page
        await sp.add_update(db, b, "resolved", "复盘结果正确", now)
        page = sp.render(await sp.snapshot(db, now + 1))
        assert "所有服务运行正常" in page and "历史事件" in page
        bot = next(c for c in (await sp.snapshot(db, now + 1))["components"] if c["key"] == "bot")
        assert bot["status"] == "正常运行" and bot["uptime"] == 100.0
    finally:
        await db.close()


def test_render_escapes_text():
    snap = {"now": time.time(), "components": [], "incidents": [], "active": [
        {"id": 1, "title": "<script>x</script>", "impact": "minor", "components": ["api"], "started_at": time.time(),
         "resolved_at": None, "updates": [{"status": "investigating", "body": "<b>", "at": time.time()}]}]}
    page = sp.render(snap)
    assert "<script>x" not in page and "&lt;script&gt;" in page
