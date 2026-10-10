import csv
import io

import pytest

from app import feedback
from app.database import Database


@pytest.mark.asyncio
async def test_feedback_submit_cooldown_and_csv(tmp_path):
    db = Database(str(tmp_path / "f.sqlite"))
    await db.connect()
    try:
        with pytest.raises(ValueError):
            await feedback.submit(db, "1", "a", None, {"score": "  "})          # 空表单
        msg = await feedback.submit(db, "1", "a", None, {"score": "9", "pain": "排队久", "bogus": "x"})
        assert "收到" in msg
        with pytest.raises(ValueError):
            await feedback.submit(db, "1", "a", None, {"score": "8"})          # 10 分钟内重复
        await feedback.submit(db, "2", "b", None, {"score": "7" * 50})         # 超长截断
        items = await feedback.listing(db)
    finally:
        await db.close()
    assert len(items) == 2 and "bogus" not in items[1]["answers"]
    assert items[0]["answers"]["score"] == "7" * 10
    rows = list(csv.reader(io.StringIO(feedback.to_csv(items).decode("utf-8-sig"))))
    assert rows[1][1] == "1" and "排队久" in rows[1]


def test_questions_fit_discord_modal_limits():
    assert 1 <= len(feedback.QUESTIONS) <= 5
    assert all(len(q["label"]) <= 45 and q["max"] <= 4000 for q in feedback.QUESTIONS)
    assert len({q["id"] for q in feedback.QUESTIONS}) == len(feedback.QUESTIONS)
