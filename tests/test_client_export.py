import csv
import io
import time
import zipfile

import pytest

from app import client_export
from app.database import Database


@pytest.mark.asyncio
async def test_export_zip_has_sorted_requests_with_device_features(tmp_path):
    db = Database(str(tmp_path / "x.sqlite"))
    await db.connect()
    try:
        k = await db.create_key({"name": "成员A", "token": "nai-secret-token-a", "daily_images": 150, "daily_anlas": 0,
                                 "daily_v5": 0, "monthly_anlas": 0, "daily_text_tokens": 0, "rpm": 10,
                                 "allow_anlas": False, "allow_img2img": False, "exclude_global_v5": False,
                                 "image_model_scope": "all"})
        now = time.time()
        for dt, st in ((90, "ok"), (200, "rejected")):
            await db._db.execute("INSERT INTO usage_log (ts,key_id,key_name,kind,status,images,src,client) "
                                 "VALUES (?,?,?,?,?,?,?,?)", (now - dt, k["id"], "成员A", "image", st, 1, "1.2.*.*", "UA"))
        await db._db.commit()
        blob = client_export.build_zip(await client_export.collect(db, 30))
    finally:
        await db.close()
    z = zipfile.ZipFile(io.BytesIO(blob))
    assert set(z.namelist()) == {"README.txt", "requests.csv", "keys.csv"}
    rows = list(csv.DictReader(io.StringIO(z.read("requests.csv").decode("utf-8-sig"))))
    assert [r["status"] for r in rows] == ["rejected", "ok"]               # 按时间升序
    keys = list(csv.DictReader(io.StringIO(z.read("keys.csv").decode("utf-8-sig"))))
    assert keys[0]["client_kinds"] == "1" and keys[0]["ok_images"] == "1"
    assert b"nai-secret-token-a" not in blob


def test_csv_formula_injection_is_neutralised():
    from app.client_export import safe_cell
    assert safe_cell('=HYPERLINK("http://x","a")').startswith("'=")
    assert safe_cell("@SUM(1)") == "'@SUM(1)" and safe_cell("+cmd") == "'+cmd"
    assert safe_cell("Mozilla/5.0") == "Mozilla/5.0" and safe_cell(12) == 12


