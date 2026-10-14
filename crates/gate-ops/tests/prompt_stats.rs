//! 提示词统计：解析 / 汇总 / HTML 渲染。数据全是合成的；敏感词只借用词表里一个中性的军事类词（「航母」）。

mod common;

use common::*;
use gate_ops::prompt_stats::*;
use jiff::tz::TimeZone;
use std::path::Path;

/// 2026-09-21 22:13:20 +08:00
const NOW: f64 = 1_790_000_000.0;

fn row(ts: f64, who: &str, prompt: &str, negative: &str) -> AuditRow {
    AuditRow {
        ts,
        key_name: who.into(),
        kind: "image".into(),
        model: "nai-diffusion-4-5-full".into(),
        status: "ok".into(),
        prompt: prompt.into(),
        negative: negative.into(),
    }
}

fn opts() -> RenderOpts {
    RenderOpts {
        days: 30,
        top: 400,
        prompts: 3000,
        now: NOW,
        tz: TimeZone::get("Asia/Shanghai").unwrap(),
    }
}

#[test]
fn norm_collapses_python_whitespace() {
    assert_eq!(norm("  a \t b\n\n c\u{a0}d\u{3000}e  "), "a b c d e");
    assert_eq!(norm(""), "");
    assert_eq!(norm(" \u{1c} "), "", "\\x1c 在 Python 里算空白");
}

#[test]
fn mask_keeps_only_head_and_tail_by_code_points() {
    // Python：len <= 2 → name[:1] + "*"；否则 name[:2] + "***" + name[-2:]
    assert_eq!(mask(""), "*");
    assert_eq!(mask("a"), "a*");
    assert_eq!(mask("ab"), "a*");
    assert_eq!(mask("abc"), "ab***bc");
    assert_eq!(mask("猫"), "猫*");
    assert_eq!(mask("猫头"), "猫*");
    assert_eq!(mask("猫头鹰公益站"), "猫头***益站");
    // 先做空白折叠：「x y」只有 3 个字符
    assert_eq!(mask("  x   y  "), "x *** y");
}

#[test]
fn summarize_counts_artists_tags_prompts() {
    let rows = vec![
        // ts 倒序，与数据库查询顺序一致
        row(
            NOW - 10.0,
            "alice",
            "1girl, Artist: WLOP, {artist:sakimichan}, solo",
            "lowres",
        ),
        row(NOW - 20.0, "bob", "artist:wlop , 1girl\n[blurry]", ""),
        row(
            NOW - 30.0,
            "alice",
            "1girl, Artist:WLOP",
            "artist:Bad One, lowres",
        ),
    ];
    let s = summarize(&rows);
    // 画师串：按小写合并，次数 / 成员数 / 最近出现
    assert_eq!(
        s.artists.most_common(),
        vec![("wlop", 3), ("sakimichan", 1), ("bad one", 1)]
    );
    assert_eq!(s.artist_members["wlop"].len(), 2);
    assert_eq!(s.artist_last["wlop"], NOW - 10.0);
    assert_eq!(s.artist_last["bad one"], NOW - 30.0);
    // 标签：按 , ， 换行 [ ] { } 切开，artist: 开头的不算；并列保持首次出现的顺序
    let tags = s.tags.most_common();
    assert_eq!(tags[0], ("1girl", 3));
    assert!(tags.iter().all(|(t, _)| !t.starts_with("artist:")));
    assert!(tags.contains(&("blurry", 1)));
    assert!(tags.contains(&("lowres", 2)));
    // 提示词去重：大小写不同算同一条，保留首次出现的原文
    assert_eq!(s.prompts.len(), 3);
    assert_eq!(
        s.prompts[0].text,
        "1girl, Artist: WLOP, {artist:sakimichan}, solo"
    );
}

#[test]
fn prompts_dedup_ignores_case_and_keeps_first_text() {
    let rows = vec![
        row(NOW - 1.0, "alice", "Hello World", ""),
        row(NOW - 2.0, "bob", "hello   world", ""),
        row(NOW - 3.0, "alice", "HELLO WORLD", ""),
    ];
    let s = summarize(&rows);
    assert_eq!(s.prompts.len(), 1);
    let e = &s.prompts[0];
    assert_eq!(
        (e.text.as_str(), e.count, e.members.len(), e.first, e.last),
        ("Hello World", 3, 2, NOW - 3.0, NOW - 1.0)
    );
}

#[test]
fn long_tags_and_empty_prompts_are_skipped() {
    let long = "x".repeat(61);
    let ok60 = "y".repeat(60);
    let s = summarize(&[
        row(NOW, "alice", &format!("{long}, {ok60}"), ""),
        row(NOW, "bob", "", "only negative"),
    ]);
    let tags: Vec<_> = s
        .tags
        .most_common()
        .into_iter()
        .map(|(t, _)| t.to_string())
        .collect();
    assert!(tags.contains(&ok60) && !tags.contains(&long));
    assert_eq!(s.prompts.len(), 1, "空提示词不进去重表");
}

#[test]
fn sensitive_hits_report_location_not_content() {
    let s = summarize(&[
        row(NOW - 5.0, "member-one", "1girl, 航母, solo", "lowres"),
        row(NOW - 6.0, "member-two", "plain prompt", "航母, 航母"),
        row(NOW - 7.0, "member-three", "nothing here", ""),
    ]);
    assert_eq!(s.sensitive.len(), 2);
    let h = &s.sensitive[0];
    assert_eq!(
        (h.category, h.hits.as_slice(), h.who.as_str()),
        ("旗帜符号与军事", ["航母"].as_slice(), "me***ne")
    );
    assert!(h.snippet.starts_with("1girl, 航母, solo\nlowres"));
    // 摘要最多 120 个字符（按码点）
    let long = format!("航母 {}", "字".repeat(300));
    let s = summarize(&[row(NOW, "m", &long, "")]);
    assert_eq!(s.sensitive[0].snippet.chars().count(), 120);
}

#[test]
fn rendered_html_has_tables_and_escapes_everything() {
    let rows = vec![
        row(
            NOW - 10.0,
            "alice",
            "<script>alert(1)</script>, artist:<b>evil</b>, a & b, 航母",
            "lowres \"q\"",
        ),
        row(NOW - 20.0, "bob", "<img src=x onerror=1>", ""),
    ];
    let html = render(&summarize(&rows), rows.len(), &opts());
    assert!(html.starts_with("<!doctype html><html lang=\"zh\">"));
    assert!(html.contains("<title>提示词统计（本地）· 2026-09-21</title>"));
    assert!(html.contains("最近 30 天，共 2 条"));
    for head in ["画师串（artist:）", "标签频率", "提示词去重"] {
        assert!(
            html.contains(&format!("<h3>{head}<span class=\"muted\">")),
            "{head}"
        );
    }
    assert!(html.contains("共 1 个</span>"), "画师串只有一个");
    assert!(html.contains("共 5 个（取前 400）"));
    // 用户内容不能变成活 HTML；也不能被转义两次
    assert!(!html.contains("<script>alert") && !html.contains("<img src=x"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("a &amp; b") && !html.contains("&amp;amp;"));
    assert!(html.contains("lowres &quot;q&quot;"));
    // 敏感自查：一条命中，只列定位信息
    assert!(html.contains("命中 1 条（时间 / 成员 / 类别 / 关键词）"));
    assert!(html.contains("<td>09-21 22:13</td><td>al***ce</td><td>image</td>"));
    assert!(html.contains("<td>旗帜符号与军事</td><td>航母</td>"));
    // 搜索按 id 取元素（第一张表的 id 里有 :，旧版的选择器会抛错）
    assert!(
        html.contains("filterTable(this,'画师串（artist:）')")
            && html.contains("document.getElementById(id)")
    );
    assert!(html.contains("data-num=\"[false, true, true, true, true]\""));
    assert!(html.ends_with("</script></body></html>"));
}

#[test]
fn rendered_html_empty_sensitive_section_and_shares() {
    let rows = vec![row(NOW - 10.0, "alice", "1girl, artist:wlop", "")];
    let html = render(&summarize(&rows), rows.len(), &opts());
    assert!(html.contains("<tr><td colspan=\"8\" class=\"muted\">没有命中</td></tr>"));
    // 占比：1 / 1 条 = 100.0%
    assert!(html.contains("<td>wlop</td><td>1</td><td>100.0%</td><td>1</td><td>09-21 22:13</td>"));
    // --prompts 限制行数，--top 只改说明文字
    let many: Vec<_> = (0..5)
        .map(|i| row(NOW - f64::from(i), "m", &format!("prompt {i}"), ""))
        .collect();
    let mut o = opts();
    o.prompts = 2;
    o.top = 7;
    let html = render(&summarize(&many), many.len(), &o);
    assert!(html.contains("共 5 条不同的提示词，按次数取前 2"));
    assert!(html.contains("（取前 7）"));
    // 只看「提示词去重」那一节（标签表不截断，里面也有 prompt 2）
    let section = html
        .split("<h3>提示词去重")
        .nth(1)
        .unwrap()
        .split("<h3>敏感内容自查")
        .next()
        .unwrap();
    assert!(
        section.contains("<td>prompt 0</td>")
            && section.contains("<td>prompt 1</td>")
            && !section.contains("<td>prompt 2</td>")
    );
    let tags = html
        .split("<h3>标签频率")
        .nth(1)
        .unwrap()
        .split("<h3>提示词去重")
        .next()
        .unwrap();
    assert!(
        tags.contains("<td>prompt 4</td>"),
        "--top 不截断标签行（与旧版一致）"
    );
}

fn seed_audit(path: &Path) {
    let c = rw(path);
    for (i, (ts, key, prompt)) in [
        (NOW - 100.0, "member-aaa", "1girl, artist:wlop, solo"),
        (NOW - 200.0, "member-bbb", "1girl, artist:wlop, smile"),
        (NOW - 40.0 * 86400.0, "member-old", "very old prompt"), // 超出 30 天
    ]
    .into_iter()
    .enumerate()
    {
        c.execute(
            "INSERT INTO generation_audit(ts, key_id, key_name, kind, model, status, prompt, negative) VALUES (?,?,?,?,?,?,?,?)",
            rusqlite::params![ts, i as i64 + 1, key, "image", "m", "ok", prompt, "lowres"],
        )
        .unwrap();
    }
}

#[test]
fn run_writes_private_report_and_summary_lines() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    seed_audit(&db);
    let out = dir.path().join("report.html");
    let args = Args {
        db,
        out: out.clone(),
        days: 30,
        top: 400,
        prompts: 3000,
    };
    let text = run(&args, NOW, TimeZone::get("Asia/Shanghai").unwrap()).unwrap();
    assert_eq!(
        text,
        "记录 2 条 · 画师串 1 个 · 标签 4 个 · 提示词 2 条 · 敏感命中 0 条\n画师串前 5： wlop ×2\n"
    );
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("共 2 条") && !html.contains("very old prompt"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
            0o600,
            "里面是提示词原文，只给自己读"
        );
    }
}

#[test]
fn run_lists_sensitive_categories_like_python_dict() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let c = rw(&db);
    for (i, p) in ["航母 1", "航母 2"].into_iter().enumerate() {
        c.execute(
            "INSERT INTO generation_audit(ts, key_id, key_name, kind, model, status, prompt, negative) VALUES (?,?,?,?,?,?,?,?)",
            rusqlite::params![NOW - i as f64, 1, "member-x", "image", "m", "ok", p, ""],
        )
        .unwrap();
    }
    let args = Args {
        db,
        out: dir.path().join("r.html"),
        days: 30,
        top: 400,
        prompts: 3000,
    };
    let text = run(&args, NOW, TimeZone::UTC).unwrap();
    assert!(
        text.ends_with("敏感命中分布： {'旗帜符号与军事': 2}\n"),
        "{text}"
    );
}

#[test]
fn run_without_records_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(dir.path());
    let args = Args {
        db,
        out: dir.path().join("r.html"),
        days: 7,
        top: 400,
        prompts: 3000,
    };
    let err = run(&args, NOW, TimeZone::UTC).unwrap_err();
    assert_eq!(
        err.to_string(),
        "最近 7 天没有审计记录（确认 audit_prompts 开着）"
    );
    assert!(!dir.path().join("r.html").exists());
}
