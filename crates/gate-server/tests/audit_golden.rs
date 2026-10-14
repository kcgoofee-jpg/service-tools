//! `audit`（旧版 `app/audit.py`）与旧 Python 的对拍。夹具由 `tools/parity/golden/audit.py` 在旧代码上生成：
//! - `audit_prompts`：提示词整理，逐字节一致，Python 抛的异常（类型 + 消息）也要一致；
//! - `audit_notice`：披露文案，逐字节一致；
//! - `audit_settings`：假库里的原始设置串 → 记录开关 / 保留时长 / 披露文案（真实 SQLite 库）；
//! - `audit_images`：图片只对「决定」——收不收、类型串、缩略图尺寸与采样点颜色（编码器不同，字节不比）。
//!
//! 测试函数名都以 `audit_` 开头，`cargo test -p gate-server audit` 能选中。

use base64::Engine;
use gate_core::pyext::PyErr;
use gate_db::{Db, DbError};
use gate_golden::{Case, assert_eq_py};
use gate_server::audit;
use gate_server::env::EnvConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::Path;

const M: &str = env!("CARGO_MANIFEST_DIR");

/// 读 `tests/fixtures/<name>.jsonl`（或 `.jsonl.gz`）。夹具放在 fixtures 目录而不是 gate-golden 默认的 golden 目录。
fn load(name: &str) -> Vec<Case> {
    let dir = Path::new(M).join("tests").join("fixtures");
    let plain = dir.join(format!("{name}.jsonl"));
    let gz = dir.join(format!("{name}.jsonl.gz"));
    let text = if gz.exists() {
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&gz).expect("读夹具"))
            .read_to_string(&mut text)
            .expect("解压夹具");
        text
    } else {
        std::fs::read_to_string(&plain)
            .unwrap_or_else(|e| panic!("读不到夹具 {}: {e}", plain.display()))
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| Case {
            line: i + 1,
            raw: serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("{name}:{} 不是合法 JSON: {e}", i + 1)),
        })
        .collect()
}

/// 把 Rust 结果和夹具里 Python 的结果（值或异常）对上。
#[track_caller]
fn check(c: &Case, got: Result<Value, (String, String)>) {
    match (c.err(), got) {
        (None, Ok(v)) => assert_eq_py(c, &v, c.out()),
        (Some(ty), Err((got_ty, got_msg))) => {
            assert_eq!(
                got_ty, ty,
                "第 {} 行：异常类型不同（{got_msg}）\n{}",
                c.line, c.raw
            );
            let want = c.raw["err"]["msg"].as_str().unwrap();
            assert_eq!(got_msg, want, "第 {} 行：异常消息不同", c.line);
        }
        (None, Err((t, m))) => panic!("第 {} 行：Python 成功，Rust 报 {t}: {m}\n{}", c.line, c.raw),
        (Some(ty), Ok(v)) => panic!(
            "第 {} 行：Python 抛 {ty}，Rust 却返回 {v}\n{}",
            c.line, c.raw
        ),
    }
}

fn py_err(e: PyErr) -> (String, String) {
    (e.type_name().to_string(), e.message().to_string())
}

// ------------------------------------------------------------------ 提示词

#[test]
fn audit_prompts_match_python() {
    let cases = load("audit_prompts");
    assert!(cases.len() > 1500, "夹具被截断了？{}", cases.len());
    let (mut ok, mut failed) = (0, 0);
    for c in &cases {
        let fn_name = c.get("fn").and_then(Value::as_str).unwrap();
        let arg = c.arg(0);
        let got = match fn_name {
            "capture_prompts" => audit::capture_prompts(arg)
                .map(|(p, n, e)| json!([p, n, e]))
                .map_err(py_err),
            "prompt_texts" => audit::prompt_texts(arg)
                .map(|(p, n)| json!([p, n]))
                .map_err(py_err),
            "_captions" => audit::captions(Some(arg))
                .map(|(base, chars)| json!([base, chars]))
                .map_err(py_err),
            other => panic!("未知函数 {other}"),
        };
        check(c, got);
        if c.err().is_none() {
            ok += 1;
        } else {
            failed += 1;
        }
    }
    // 夹具里确实有 Python 抛异常的用例（不是全绿的假象）
    assert!(failed > 100 && ok > 1500, "ok={ok} err={failed}");
}

// ------------------------------------------------------------------ 披露文案

#[test]
fn audit_notice_matches_python() {
    let cases = load("audit_notice");
    assert!(cases.len() > 500);
    for c in &cases {
        let a = c.args();
        let got = audit::audit_notice(
            a[0].as_bool().unwrap(),
            a[1].as_bool().unwrap(),
            a[2].as_i64().unwrap(),
            a[3].as_i64().unwrap(),
            a[4].as_i64().unwrap(),
        );
        check(c, Ok(json!(got)));
    }
}

// ------------------------------------------------------------------ 设置读取（真实 SQLite）

fn env_of(c: &Case) -> EnvConfig {
    let mut env = EnvConfig::load(&HashMap::<String, String>::new());
    // 旧版 getattr(settings, name, default)：夹具里缺的属性取这三个默认值
    env.audit_prompts = false;
    env.audit_thumbs = false;
    env.audit_retention_days = 7;
    let e = c.get("env").and_then(Value::as_object);
    if let Some(e) = e {
        if let Some(v) = e.get("audit_prompts") {
            env.audit_prompts = v.as_bool().unwrap();
        }
        if let Some(v) = e.get("audit_thumbs") {
            env.audit_thumbs = v.as_bool().unwrap();
        }
        if let Some(v) = e.get("audit_retention_days") {
            env.audit_retention_days = v.as_i64().unwrap();
        }
    }
    env
}

fn db_err(e: &DbError) -> (String, String) {
    // 旧版抛 OverflowError（没人捕获）；这里是 DbError::Invalid
    assert!(matches!(e, DbError::Invalid(_)), "非预期的错误类型：{e:?}");
    (
        "OverflowError".to_string(),
        "cannot convert float infinity to integer".to_string(),
    )
}

#[tokio::test]
async fn audit_settings_match_python() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("gate.db"), 0.0, &[]).unwrap();
    let cases = load("audit_settings");
    assert!(cases.len() > 1000);
    let mut errors = 0;
    for c in &cases {
        let fn_name = c.get("fn").and_then(Value::as_str).unwrap();
        if fn_name == "constants" {
            let out = c.out();
            assert_eq!(out["IMAGE_RETENTION_KEY"], audit::IMAGE_RETENTION_KEY);
            assert_eq!(out["IMAGE_CAP_KEY"], audit::IMAGE_CAP_KEY);
            assert_eq!(out["MEMBER_DOWNLOAD_DAYS"], audit::MEMBER_DOWNLOAD_DAYS);
            assert_eq!(out["THUMB_SIDE"], audit::THUMB_SIDE);
            assert_eq!(out["THUMB_QUALITY"], audit::THUMB_QUALITY);
            assert_eq!(out["MAX_IMAGE_BYTES"], audit::MAX_IMAGE_BYTES);
            assert_eq!(out["MAX_IMAGE_PIXELS"], audit::MAX_IMAGE_PIXELS);
            continue;
        }
        // 把这一例的原始设置串写进库（先清空：缺失的键必须真的没有这一行）
        db.write(|tx| tx.execute_batch("DELETE FROM site_settings"))
            .await
            .unwrap();
        for (k, v) in c.raw["kv"].as_object().unwrap() {
            db.set_setting(k, v.as_str().unwrap()).await.unwrap();
        }
        let env = env_of(c);
        let got: Result<Value, (String, String)> = match fn_name {
            "audit_flags" => audit::audit_flags(&db, &env)
                .await
                .map(|(p, t, d)| json!([p, t, d]))
                .map_err(|e| db_err(&e)),
            "audit_image_days" => audit::audit_image_days(&db)
                .await
                .map(|v| json!(v))
                .map_err(|e| db_err(&e)),
            "audit_image_keep_hours" => audit::audit_image_keep_hours(&db)
                .await
                .map(|v| json!(v))
                .map_err(|e| db_err(&e)),
            "audit_image_cap_mb" => audit::audit_image_cap_mb(&db)
                .await
                .map(|v| json!(v))
                .map_err(|e| db_err(&e)),
            "audit_disclosure" => audit::audit_disclosure(&db, &env)
                .await
                .map(|v| json!(v))
                .map_err(|e| db_err(&e)),
            other => panic!("未知函数 {other}"),
        };
        if got.is_err() {
            errors += 1;
        }
        check(c, got);
    }
    assert!(errors > 50, "夹具里应有 Python 抛 OverflowError 的用例");
}

// ------------------------------------------------------------------ 图片

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .expect("夹具里的 base64")
}

/// 夹具 recipe → 字节（与 audit.py 的 `build` 对应）：blob = head + 零填充；zip = 每个条目一个 blob。
fn blob(spec: &Value) -> Vec<u8> {
    let mut data = spec
        .get("head")
        .and_then(Value::as_str)
        .map(b64)
        .unwrap_or_default();
    data.resize(spec["len"].as_u64().unwrap() as usize, 0);
    data
}

fn build(recipe: &Value) -> Vec<u8> {
    match recipe["kind"].as_str().unwrap() {
        "blob" => blob(recipe),
        "zip" => {
            let method = if recipe["method"] == "deflate" {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            };
            let mut buf = Cursor::new(Vec::new());
            let mut z = zip::ZipWriter::new(&mut buf);
            for e in recipe["entries"].as_array().unwrap() {
                z.start_file(
                    e["name"].as_str().unwrap(),
                    zip::write::SimpleFileOptions::default().compression_method(method),
                )
                .unwrap();
                z.write_all(&blob(e)).unwrap();
            }
            z.finish().unwrap();
            buf.into_inner()
        }
        other => panic!("未知 recipe {other}"),
    }
}

fn payload_of(c: &Case) -> Vec<u8> {
    let p = &c.raw["payload"];
    match p.get("$b").and_then(Value::as_str) {
        Some(s) => b64(s),
        None => build(&p["recipe"]),
    }
}

/// 缩略图是 JPEG：尺寸必须一致，四个象限中心的颜色在容差内（重采样 / 量化不同，不比字节）。
#[track_caller]
fn check_thumb(c: &Case, want: &Value, got: Option<Vec<u8>>) {
    let name = c.raw["name"].as_str().unwrap();
    if want.is_null() {
        assert!(got.is_none(), "{name}：Python 不收，Rust 却出了缩略图");
        return;
    }
    let jpeg = got.unwrap_or_else(|| panic!("{name}：Python 出了缩略图，Rust 返回 None"));
    assert!(jpeg.starts_with(b"\xff\xd8\xff"), "{name}：缩略图不是 JPEG");
    let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .unwrap_or_else(|e| panic!("{name}：缩略图解不开：{e}"))
        .to_rgb8();
    let size = want["size"].as_array().unwrap();
    let (w, h) = (
        size[0].as_u64().unwrap() as u32,
        size[1].as_u64().unwrap() as u32,
    );
    assert_eq!(img.dimensions(), (w, h), "{name}：缩略图尺寸");
    assert_eq!(want["format"], "JPEG");
    assert_eq!(want["mode"], "RGB");
    // 太小的图（JPEG 色度下采样会把相邻象限的颜色混进来）不比颜色
    if w.min(h) >= 32 && c.get("nocolor").is_none() {
        let points = [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)];
        for (i, (fx, fy)) in points.iter().enumerate() {
            let px = img.get_pixel((f64::from(w) * fx) as u32, (f64::from(h) * fy) as u32);
            let exp = want["samples"][i].as_array().unwrap();
            for ch in 0..3 {
                let e = exp[ch].as_i64().unwrap();
                let d = (i64::from(px[ch]) - e).abs();
                assert!(
                    d <= 24,
                    "{name}：采样点 {i} 通道 {ch}：Rust {} 对 Python {e}",
                    px[ch]
                );
            }
        }
    }
}

/// 已知差异（见 audit.rs 模块说明与 docs/v3-deviations.md）：Python 能打开、这里不能的格式。
fn known_deviation(c: &Case) -> Option<&str> {
    c.get("deviation").and_then(Value::as_str)
}

#[test]
fn audit_images_match_python() {
    let cases = load("audit_images");
    assert!(cases.len() > 100);
    let (mut accepted, mut rejected) = (0, 0);
    for c in &cases {
        let name = c.raw["name"].as_str().unwrap().to_string();
        let payload = payload_of(c);
        assert!(
            c.raw["thumb_err"].is_null() && c.raw["full_err"].is_null(),
            "{name}：Python 不应抛异常"
        );

        // ---- make_thumbnail
        let thumb = audit::make_thumbnail(&payload);
        match known_deviation(c) {
            Some("gif" | "bmp" | "bzip2" | "lzma" | "png-trailer") => {
                assert!(
                    c.raw["thumb"].is_object(),
                    "{name}：这类差异是 Python 收、Rust 不收"
                );
                assert!(thumb.is_none(), "{name}：已知差异，Rust 应当不收");
            }
            Some(other) => panic!("未知差异标记 {other}"),
            None => check_thumb(c, &c.raw["thumb"], thumb.clone()),
        }
        if thumb.is_some() {
            accepted += 1;
        } else {
            rejected += 1;
        }

        // ---- full_image
        let (data, ctype) = audit::full_image(&payload);
        let want = &c.raw["full"];
        if matches!(known_deviation(c), Some("bzip2" | "lzma")) {
            assert!(want[0].is_object());
            assert_eq!(
                (data, ctype),
                (None, String::new()),
                "{name}：zip 压缩方式不支持"
            );
            continue;
        }
        assert_eq!(ctype, want[1].as_str().unwrap(), "{name}：content_type");
        match (&want[0], data) {
            (Value::Null, d) => assert!(
                d.is_none(),
                "{name}：Python 不存，Rust 却存了 {} 字节",
                d.map_or(0, |d| d.len())
            ),
            (summary, Some(d)) => {
                assert_eq!(
                    d.len() as u64,
                    summary["len"].as_u64().unwrap(),
                    "{name}：长度"
                );
                assert_eq!(
                    hex_of(&Sha256::digest(&d)),
                    summary["sha256"].as_str().unwrap(),
                    "{name}：内容"
                );
            }
            (_, None) => panic!("{name}：Python 存了，Rust 返回 None"),
        }
    }
    assert!(
        accepted > 50 && rejected > 30,
        "accepted={accepted} rejected={rejected}"
    );
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
