"""疑似同一人（小号）识别：行为信号才算数，只靠网段相同不标记。"""
import time

import pytest

from app import alt_guard
from app.database import Database

BASE = {"daily_images": 150, "daily_anlas": 0, "daily_v5": 0, "monthly_anlas": 0, "daily_text_tokens": 0,
        "rpm": 10, "allow_anlas": False, "allow_img2img": False, "exclude_global_v5": False, "image_model_scope": "all"}


async def _keys(db, *names):
    return [(await db.create_key({"name": n, "token": f"nai-{n}", **BASE}))["id"] for n in names]


async def _log(db, kid, ts, status="ok", detail="", src="10.1.*.*", client="ClientX/1.0"):
    await db._db.execute("INSERT INTO usage_log(ts, key_id, key_name, kind, model, status, images, detail, client, src) "
                         "VALUES (?,?,?,?,?,?,?,?,?,?)", (ts, kid, f"k{kid}", "image", "m", status, 1, detail, client, src))


@pytest.mark.asyncio
async def test_handoff_after_limit_flags_pair(tmp_path):
    db = Database(str(tmp_path / "a.sqlite"))
    await db.connect()
    try:
        main, alt, other = await _keys(db, "main", "alt", "other")
        now = time.time()
        for day in range(2):                       # 两次：主号撞上限 → 5 分钟后小号上线
            t0 = now - 86400 * day - 7200
            for i in range(5):
                await _log(db, main, t0 + i * 60)
            await _log(db, main, t0 + 400, "rejected", "402 已达今日 V5 额度（36 张/天）")
            for i in range(5):
                await _log(db, alt, t0 + 700 + i * 60)
        for i in range(5):                          # 无关的第三个人
            await _log(db, other, now - 50000 + i * 60, client="Other/2", src="20.2.*.*")
        await db._db.commit()
        links = await alt_guard.scan(db, now)
        assert [p["keys"] for p in links] == [sorted([main, alt])]
        assert links[0]["signals"]["handoff"] == 2 and "用完一把马上换另一把" in alt_guard.describe(links[0]["signals"])
    finally:
        await db.close()


@pytest.mark.asyncio
async def test_same_network_alone_is_not_flagged(tmp_path):
    # 室友 / 同一校园网：网段、客户端相同，但各用各的（时间不挨着、没有撞上限后换号）
    db = Database(str(tmp_path / "b.sqlite"))
    await db.connect()
    try:
        a, b = await _keys(db, "roommate-a", "roommate-b")
        now = time.time()
        for kid, ts in ((a, now - 80000), (b, now - 40000)):
            await db._db.execute("INSERT INTO key_sources(key_id, net_hash, label, first_seen, last_seen, hits) "
                                 "VALUES (?,?,?,?,?,1)", (kid, "net-1", "10.1.*.*", ts, ts))
            for i in range(3):
                await _log(db, kid, ts + i * 60)
        await db._db.commit()
        assert await alt_guard.scan(db, now) == []
    finally:
        await db.close()
