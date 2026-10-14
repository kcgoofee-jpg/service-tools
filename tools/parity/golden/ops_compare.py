"""运维命令的「并排对比」：同一份合成库上分别跑旧 Python 脚本和新 Rust `nai-ops`，比较输出。

和别的 golden 脚本不同，这个不生成夹具，而是需要先编译出二进制（所以放进批量跑 golden 的流程里也不会失败：
找不到二进制就跳过）。切换前在服务器上 / 本机各跑一次，确认两边一致：

    cargo build -p gate-ops
    uv run --python 3.12 --with-requirements requirements.txt python tools/parity/golden/ops_compare.py

比较的内容：
- patrol：`--json`（字节级）、`--oneline`、人读报告。旧版会查已删除的 waitlist / share_evidence，
  这里给旧版补两张空表让它能跑完，并从旧版输出里去掉「候补」那一段再比；
- snapshot + ro-sanitize：快照里每张表的全部行、建表 / 索引 / 触发器语句、自增序号逐项相同；
  脱敏后同样。Rust 版多清的两处（key_donations / discord_bans 头像）在这份数据里没有对应行，所以仍应一致；
- prompt-stats：stdout 与 HTML（旧版搜索框 JS 有 bug，已修，比较前把旧版那段换成新写法）。

数据全部合成。
"""
from __future__ import annotations

import asyncio
import gzip
import json
import os
import random
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
BIN = Path(os.environ.get("NAI_OPS_BIN", ROOT / "target" / "debug" / "nai-ops"))
if not BIN.exists():
    print(f"跳过：没有 {BIN}（先 cargo build -p gate-ops）")
    sys.exit(0)

WORK = Path(tempfile.mkdtemp(prefix="ops-compare-"))
os.environ["DATA_DIR"] = str(WORK)
os.environ["TZ"] = "Asia/Shanghai"
time.tzset()
sys.path.insert(0, str(ROOT))
from app.database import Database  # noqa: E402

FAILED: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(("PASS " if ok else "FAIL ") + name + (f"\n{detail}" if detail and not ok else ""))
    if not ok:
        FAILED.append(name)


def new_db(path: Path) -> sqlite3.Connection:
    async def create() -> None:
        db = Database(str(path), "Asia/Shanghai")
        await db.connect()
        await db.close()

    asyncio.run(create())
    return sqlite3.connect(path)


def run(cmd: list, env_extra: dict | None = None) -> tuple[int, str, str]:
    env = dict(os.environ, **(env_extra or {}))
    p = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, env=env)
    return p.returncode, p.stdout, p.stderr


# ---------------------------------------------------------------- patrol
def compare_patrol() -> None:
    work = WORK / "patrol"
    work.mkdir()
    db_path = work / "nai_gate.db"
    c = new_db(db_path)
    now = time.time()
    # 旧版会查的两张（线上已删除）表：补空表让它跑完；Rust 版根本不查
    c.execute("CREATE TABLE IF NOT EXISTS waitlist (id INTEGER)")
    c.execute("CREATE TABLE IF NOT EXISTS share_evidence (ts REAL, kind TEXT)")
    for i, (tok, test) in enumerate([("nai-synth-1", 0), ("nai-synth-2", 1), ("nai-synth-3", 0)], 1):
        c.execute("INSERT INTO api_keys(id,name,token,is_test,created_at,expires_at) VALUES (?,?,?,?,?,?)",
                  (i, f"synthetic-{i}", tok, test, 1.7e9, None if i != 3 else now + 5000))
    day = c.execute("SELECT date('now','+8 hours')").fetchone()[0]
    c.execute("INSERT INTO counters(key_id,day,images) VALUES (1,?,100)", (day,))
    c.execute("INSERT INTO upstream_token_counters(token_id,day,images) VALUES ('acct-a',?,450)", (day,))
    c.execute("INSERT INTO upstream_token_counters(token_id,day,images) VALUES ('acct-b',?,450)", (day,))
    settings = {"guard_account_daily_cap": "500", "quota_algo_last": json.dumps({"v5": {"percent": 1.5, "rate": 12.5}}),
                "register_open": "1", "register_daily_images": "20", "register_daily_v5": "3",
                "register_max_users": "50", "issue_hourly_cap": "2", "guard_last_upstream_429": str(now - 600),
                "guard_account_hourly_cap": "10"}
    for k, v in settings.items():
        c.execute("INSERT OR REPLACE INTO site_settings(key,value) VALUES (?,?)", (k, v))

    def log(dt, key, kind="image", status="ok", images=1, wait=0, reason="", client="", up=200, detail="", model="", rid=""):
        c.execute("INSERT INTO usage_log(ts,key_id,key_name,kind,model,status,images,wait_ms,reason,client,up_status,detail,rid)"
                  " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)", (now - dt, key, "s", kind, model, status, images, wait, reason, client, up, detail, rid))

    for dt, w in [(100, 1000), (90, 3000), (60, 5000)]:
        log(dt, 1, wait=w, client="TavernAI/1.0")
    log(500, 2, wait=130000, client="Mozilla/5.0 (test)")
    for dt in (7200, 7000):
        log(dt, 3)
    for i, r in enumerate(["paced"] * 5 + ["daily_cap", "queue_full", "queue_full"]):
        log(20000 + i, 1, status="rejected", images=0, reason=r, up=0)
    log(20100, 1, status="error", images=0, up=502)
    for i in range(10):
        log(600 - i, 1, kind="chat", status="ok" if i < 6 else "error", images=0, detail="上游返回错误：说明" * 8, rid="rid-synth-1")
    for i in range(2):
        log(300 + i, 1, status="error", images=0, up=500)
    for i, st in enumerate(["ok", "ok", "error"]):
        log(900 + i, 1, status=st, model="nai-diffusion-4-5-inpainting")
    for i in range(2):
        log(1200 + i, 1, status="error", images=0, up=429)
    for i in range(3):
        log(i, 1)
    for lvl, title, cnt, ls in [("error", "live: GET /public/live failed", 5, now - 100),
                                ("warn", "V5：每人额度 × 人数 ≤ 全站上限（示例）", 2, now - 10),
                                ("warn", "旧问题", 1, now - 3000), ("warn", "新问题", 3, now - 60), ("info", "信息", 1, now - 30)]:
        c.execute("INSERT INTO error_events(sig,source,level,title,detail,count,first_seen,last_seen) VALUES (?,?,?,?,?,?,?,?)",
                  (f"sig-{title}", "request", lvl, title, "", cnt, ls - 5, ls))
    c.execute("INSERT INTO discord_registrations(discord_id,key_id,created_at) VALUES ('900000000000000001',1,?)", (now - 100,))
    c.execute("INSERT INTO discord_registrations(discord_id,key_id,created_at) VALUES ('900000000000000003',3,?)", (now - 5000,))
    c.commit()
    c.close()
    (work / ".env").write_text("REGISTRATION_BRIDGE_SECRET=x\nREGISTER_DAILY_IMAGES=30\nREGISTER_MAX_USERS=50\n")

    def env(state: str) -> dict:
        return dict(NAI_GATE_DB=str(db_path), NAI_GATE_ENV=str(work / ".env"), PATROL_STATE_DIR=str(work / state),
                    NAI_GATE_HEALTH="http://127.0.0.1:1/healthz", NAI_GATE_QUEUE="http://127.0.0.1:1/queue-status")

    py_script = ROOT / "deploy" / "ops" / "patrol.py"

    def strip_waitlist(text: str) -> str:
        return text.replace(" · 候补 0", "")

    rc_py, out_py, err_py = run([sys.executable, py_script, "--json"], env("state_py"))
    rc_rs, out_rs, err_rs = run([BIN, "patrol", "--json"], env("state_rs"))
    check("patrol 退出码一致", rc_py == rc_rs and rc_py in (0, 1, 2), f"py={rc_py} rs={rc_rs}\n{err_py[-800:]}\n{err_rs[-800:]}")
    if rc_py in (0, 1, 2):
        py = json.loads(out_py)
        py["data"].pop("waitlist", None)
        for r in py["rows"]:
            r["text"] = strip_waitlist(r["text"])
        want = json.dumps(py, ensure_ascii=False, indent=2).strip()
        check("patrol --json 字节一致（去掉已删除的候补）", want == out_rs.strip(),
              f"--py--\n{want[:1500]}\n--rs--\n{out_rs[:1500]}")
    for flag in ("--oneline", None):
        a = run([sys.executable, py_script] + ([flag] if flag else []), env("state_py"))[1]
        b = run([BIN, "patrol"] + ([flag] if flag else []), env("state_rs"))[1]
        norm = lambda t: re.sub(r"(\d\d:\d\d)(:\d\d)?", r"\1", strip_waitlist(t))  # 时间只比到分钟
        check(f"patrol {flag or '人读报告'} 一致", norm(a) == norm(b), f"--py--\n{a}\n--rs--\n{b}")


# ---------------------------------------------------------------- snapshot / ro-sanitize
def dump_db(path: Path) -> dict:
    conn = sqlite3.connect(path)
    out = {"master": sorted(conn.execute("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'").fetchall()),
           "seq": sorted(conn.execute("SELECT name,seq FROM sqlite_sequence").fetchall())}
    for (t,) in conn.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall():
        out[t] = sorted(conn.execute(f'SELECT * FROM "{t}"').fetchall(), key=repr)
    conn.close()
    return out


def compare_snapshot() -> None:
    work = WORK / "snap"
    work.mkdir()
    src = work / "nai_gate.db"
    c = new_db(src)
    for i in range(1, 4):
        c.execute("INSERT INTO api_keys(id,name,token,created_at) VALUES (?,?,?,?)", (i, f"s{i}", f"nai-synth-{i}", 1.7e9))
        c.execute("INSERT INTO generation_audit(ts,key_id,key_name,prompt,negative,extra,thumb,image,image_type,image_file,image_size)"
                  " VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                  (1.79e9 + i, i, f"s{i}", "synthetic prompt", "neg", "{}", b"\x01\x02\x03" * 10, b"\xab" * 200000, "image/png", f"f{i}.png", 200000))
    c.execute("DELETE FROM generation_audit WHERE id=3")
    for i in range(30):
        c.execute("INSERT INTO usage_log(ts,key_id,key_name,kind,status,images) VALUES (?,?,?,?,?,?)", (1.79e9 + i, 1 + i % 3, "s", "image", "ok", 1))
    for k, v in {"register_open": "1", "key_source_salt": "salt", "admin_password_hash": "h", "x_secret": "s"}.items():
        c.execute("INSERT INTO site_settings(key,value) VALUES (?,?)", (k, v))
    c.execute("INSERT INTO error_events(sig,source,level,title,detail,count,first_seen,last_seen) VALUES ('s','request','error','t','Traceback',1,1,2)")
    c.execute("INSERT INTO discord_registrations(discord_id,key_id,created_at,avatar) VALUES ('900000000000000001',1,1.0,'avatar')")
    c.execute("DELETE FROM api_keys WHERE id=3")  # 触发器 → deleted_key_usage_flags
    c.commit()
    c.close()

    # 旧 snapshot.py 的 SRC 写死容器路径：替换后原样 exec
    py_out = work / "py.db"
    code = (ROOT / "deploy/ops/snapshot.py").read_text().replace('"/app/data/nai_gate.db"', repr(str(src)))
    saved = sys.argv
    sys.argv = ["-", str(py_out)]
    try:
        exec(compile(code, "snapshot.py", "exec"), {"__name__": "__main__"})
    finally:
        sys.argv = saved
    rs_gz = work / "rs.db.gz"
    rc, out, err = run([BIN, "snapshot", rs_gz, "--db", src])
    check("snapshot 退出码 0", rc == 0, err)
    rs_out = work / "rs.db"
    rs_out.write_bytes(gzip.decompress(rs_gz.read_bytes()))
    a, b = dump_db(py_out), dump_db(rs_out)
    bad = [k for k in a if a[k] != b.get(k)]
    check(f"snapshot 与旧版逐表一致（{len(a)} 项）", not bad and a.keys() == b.keys(), f"不同：{bad}")

    for name, p in (("py", py_out), ("rs", rs_out)):
        shutil.copy(p, work / f"{name}.san.db")
    saved = sys.argv
    sys.argv = ["-", str(work / "py.san.db")]
    try:
        exec(compile((ROOT / "deploy/ops/ro_sanitize.py").read_text(), "ro_sanitize.py", "exec"), {"__name__": "__main__"})
    finally:
        sys.argv = saved
    rc, out, err = run([BIN, "ro-sanitize", work / "rs.san.db"])
    check("ro-sanitize 输出 sanitized 2 keys", rc == 0 and out.strip() == "sanitized 2 keys", out + err)
    a, b = dump_db(work / "py.san.db"), dump_db(work / "rs.san.db")
    bad = [k for k in a if a[k] != b.get(k)]
    check(f"ro-sanitize 与旧版逐表一致（{len(a)} 项）", not bad and a.keys() == b.keys(), f"不同：{bad}")


# ---------------------------------------------------------------- prompt-stats
def compare_prompt_stats() -> None:
    work = WORK / "prompt"
    work.mkdir()
    db_path = work / "nai_gate.db"
    c = new_db(db_path)
    rng = random.Random(7)
    artists = ["wlop", "Greg Rutkowski", "sakimichan", "ciloranko", "Mika Pikazo", "fuzichoco", "rella", "kantoku"]
    base_tags = ["1girl", "solo", "masterpiece", "best quality", "blue hair", "long hair", "smile", "looking at viewer",
                 "school uniform", "outdoors", "sunlight", "very aesthetic", "highres", "{{detailed eyes}}", "[blurry]"]
    names = ["a", "ab", "abc", "abcd", "long_member_name_1", "猫头鹰", "猫", "x y z"]
    now = time.time()
    rows = []
    for i in range(400):
        k = rng.randint(1, 8)
        parts = rng.sample(base_tags, rng.randint(3, 8))
        for _ in range(rng.randint(0, 3)):
            a = rng.choice(artists)
            parts.append(rng.choice([f"artist:{a}", f"artist: {a.upper()}", f"{{artist:{a}}}", f"artist:{a} , extra"]))
        prompt = rng.choice([", ", " ,  ", "，", "\n"]).join(parts)
        if i % 37 == 0:
            prompt += "，航母编队, tags " + "x" * 70
        if i % 41 == 0:
            prompt = "  " + prompt.upper() + "\t\u00a0end "
        neg = rng.choice(["lowres, bad anatomy", "lowres,  bad hands\n\nnsfw", ""])
        rows.append((now - rng.randint(0, 40 * 86400), k, names[k - 1], rng.choice(["image", "text"]), "nai-diffusion-4-5-full",
                     rng.choice(["ok", "ok", "rejected"]), prompt, neg))
    rows.append((now - 100, 1, "a", "image", "m", "ok", "Duplicate Prompt, solo", ""))
    rows.append((now - 200, 2, "ab", "image", "m", "ok", "duplicate prompt, SOLO", ""))
    for r in rows:
        c.execute("INSERT INTO generation_audit(ts,key_id,key_name,kind,model,status,prompt,negative) VALUES (?,?,?,?,?,?,?,?)", r)
    c.commit()
    c.close()

    py_html, rs_html = work / "py.html", work / "rs.html"
    args = ["--db", db_path, "--days", "30", "--top", "400", "--prompts", "3000"]
    p1 = run([sys.executable, ROOT / "deploy/ops/prompt_stats.py", *args, "--out", py_html])
    p2 = run([BIN, "prompt-stats", *args, "--out", rs_html])
    check("prompt-stats 退出码一致", p1[0] == p2[0] == 0, p1[2] + p2[2])
    check("prompt-stats stdout 一致", p1[1] == p2[1], f"--py--\n{p1[1]}\n--rs--\n{p2[1]}")
    old_js = """  document.querySelectorAll('#' + id + ' tbody tr').forEach(tr => {
    tr.hidden = q && !tr.textContent.toLowerCase().includes(q);
  });"""
    new_js = """  // 旧版用 querySelectorAll('#'+id)：第一张表的 id 里有「:」，选择器会抛错、搜索框失效；改按 id 取元素
  const tb = document.getElementById(id);
  if (!tb) return;
  tb.querySelectorAll('tbody tr').forEach(tr => {
    tr.hidden = q && !tr.textContent.toLowerCase().includes(q);
  });"""
    check("prompt-stats HTML 一致（除搜索框 JS 的修复）", py_html.read_text().replace(old_js, new_js) == rs_html.read_text())


if __name__ == "__main__":
    try:
        compare_patrol()
        compare_snapshot()
        compare_prompt_stats()
    finally:
        shutil.rmtree(WORK, ignore_errors=True)
    print("全部一致" if not FAILED else f"有差异：{FAILED}")
    sys.exit(1 if FAILED else 0)
