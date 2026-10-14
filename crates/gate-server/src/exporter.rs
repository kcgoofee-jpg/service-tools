//! 把成员的原图打包成 ZIP 或 EPUB（电子书）（旧版 `app/exporter.py`）。后台导出和成员自助打包共用。
//!
//! 行是 `db.audit_images_for` 的结果。可能较大较慢，调用方放到阻塞线程里跑。

use gate_core::clock::Clock;
use gate_core::pyext::py_str;
use gate_core::pyval::truthy;
use gate_db::repo::audit::AuditImage;
use serde_json::Value;
use std::io::{Cursor, Write};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

/// 原图扩展名（其余类型落 `.png`，与旧版 `_EXT.get(type, "png")` 一致）。
fn ext_of(image_type: &str) -> &'static str {
    match image_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// `html.escape(s)`（引号也转义）。
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// 一行参数摘要：`种子 42 · k_euler · 28步 · CFG 5 · 832×1216`。额外信息读不出来就是空串。
fn params_line(extra: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(extra) else {
        return String::new();
    };
    let p = v.get("params").filter(|p| truthy(p));
    let Some(p) = p.and_then(Value::as_object) else {
        return String::new();
    };
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    let mut bits = Vec::new();
    if !get("seed").is_null() {
        bits.push(format!("种子 {}", py_str(get("seed"))));
    }
    if truthy(get("sampler")) {
        bits.push(py_str(get("sampler")));
    }
    if truthy(get("steps")) {
        bits.push(format!("{}步", py_str(get("steps"))));
    }
    if !get("scale").is_null() {
        bits.push(format!("CFG {}", py_str(get("scale"))));
    }
    if truthy(get("width")) && truthy(get("height")) {
        bits.push(format!(
            "{}×{}",
            py_str(get("width")),
            py_str(get("height"))
        ));
    }
    bits.join(" · ")
}

/// zip 条目的修改时间（本地时间；超出 zip 能表示的范围就用默认值）。
fn zip_time(clock: &Clock, ts: f64) -> Option<zip::DateTime> {
    let z = jiff::Timestamp::from_second(ts as i64)
        .ok()?
        .to_zoned(clock.tz().clone());
    zip::DateTime::from_date_and_time(
        u16::try_from(z.year()).ok()?,
        u8::try_from(z.month()).ok()?,
        u8::try_from(z.day()).ok()?,
        u8::try_from(z.hour()).ok()?,
        u8::try_from(z.minute()).ok()?,
        u8::try_from(z.second()).ok()?,
    )
    .ok()
}

fn opts(method: CompressionMethod, clock: &Clock, now: f64) -> SimpleFileOptions {
    let o = SimpleFileOptions::default().compression_method(method);
    match zip_time(clock, now) {
        Some(t) => o.last_modified_time(t),
        None => o,
    }
}

/// 原图 zip + prompts.txt（每张的正面 / 负面 / 参数）。图片本身已压缩，所以只存不压（`ZIP_STORED`）。
pub fn build_zip(rows: &[AuditImage], who: &str, clock: &Clock, now: f64) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    let mut lines = vec![
        format!(
            "{who} 的作品 · 共 {} 张 · 导出于 {}",
            rows.len(),
            clock.format(now, "%Y-%m-%d %H:%M")
        ),
        String::new(),
    ];
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let stored = opts(CompressionMethod::Stored, clock, now);
        for (i, r) in rows.iter().enumerate() {
            let stamp = clock.format(r.ts, "%Y%m%d-%H%M%S");
            let fname = format!("{:04}_{stamp}.{}", i + 1, ext_of(&r.image_type));
            if z.start_file(&fname, stored).is_ok() {
                let _ = z.write_all(&r.image);
            }
            let params = params_line(&r.extra);
            let mut entry = format!("[{fname}]\n正面：{}\n负面：{}", r.prompt, r.negative);
            if !params.is_empty() {
                entry.push_str(&format!("\n参数：{params}"));
            }
            entry.push('\n');
            lines.push(entry);
        }
        if z.start_file("prompts.txt", stored).is_ok() {
            let _ = z.write_all(lines.join("\n").as_bytes());
        }
        let _ = z.finish();
    }
    buf.into_inner()
}

fn page_xhtml(
    title: &str,
    img_name: &str,
    prompt: &str,
    negative: &str,
    params: &str,
    when: &str,
) -> String {
    let e = html_escape;
    let neg = if negative.is_empty() {
        String::new()
    } else {
        format!("<p class=\"neg\"><b>负面</b> {}</p>", e(negative))
    };
    let par = if params.is_empty() {
        String::new()
    } else {
        format!("<p class=\"par\">{}</p>", e(params))
    };
    format!(
        "<?xml version='1.0' encoding='utf-8'?>\n\
         <!DOCTYPE html>\n<html xmlns='http://www.w3.org/1999/xhtml'><head>\
         <title>{}</title><link rel='stylesheet' href='style.css' type='text/css'/></head>\
         <body><div class='pg'><img src='{}' alt=''/>\
         <p class='t'>{}</p><p class='p'><b>正面</b> {}</p>{neg}{par}</div></body></html>",
        e(title),
        e(img_name),
        e(when),
        e(prompt)
    )
}

/// 一张图一页的电子书：封面 + 每页图 + 提示词 / 参数。生成标准 EPUB2（zip 结构）。
pub fn build_epub(rows: &[AuditImage], who: &str, clock: &Clock, now: f64) -> Vec<u8> {
    let e = html_escape;
    let day = clock.format(now, "%Y-%m-%d");
    let title = format!("{who} 的作品集");
    let (mut manifest, mut spine, mut nav) = (String::new(), String::new(), String::new());
    struct Page {
        page: String,
        img: String,
        bytes: Vec<u8>,
        xhtml: String,
    }
    let mut pages = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let n = i + 1;
        let ext = ext_of(&r.image_type);
        let img_name = format!("img{n:04}.{ext}");
        let page_name = format!("p{n:04}.xhtml");
        let when = clock.format(r.ts, "%Y-%m-%d %H:%M");
        pages.push(Page {
            page: page_name.clone(),
            img: img_name.clone(),
            bytes: r.image.clone(),
            xhtml: page_xhtml(
                &format!("第 {n} 张"),
                &img_name,
                &r.prompt,
                &r.negative,
                &params_line(&r.extra),
                &when,
            ),
        });
        let media = if ext == "jpg" {
            "image/jpeg".to_string()
        } else {
            format!("image/{ext}")
        };
        manifest.push_str(&format!(
            "<item id='img{n}' href='{img_name}' media-type='{media}'/>"
        ));
        manifest.push_str(&format!(
            "<item id='pg{n}' href='{page_name}' media-type='application/xhtml+xml'/>"
        ));
        spine.push_str(&format!("<itemref idref='pg{n}'/>"));
        nav.push_str(&format!(
            "<navPoint id='n{n}' playOrder='{n}'><navLabel><text>第 {n} 张</text></navLabel>\
             <content src='{page_name}'/></navPoint>"
        ));
    }
    let opf = format!(
        "<?xml version='1.0' encoding='utf-8'?>\n\
         <package xmlns='http://www.idpf.org/2007/opf' version='2.0' unique-identifier='bid'>\
         <metadata xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:title>{}</dc:title>\
         <dc:creator>{}</dc:creator><dc:language>zh</dc:language>\
         <dc:identifier id='bid'>owl-{}</dc:identifier><dc:date>{day}</dc:date></metadata>\
         <manifest><item id='ncx' href='toc.ncx' media-type='application/x-dtbncx+xml'/>\
         <item id='css' href='style.css' media-type='text/css'/>\
         <item id='cover' href='cover.xhtml' media-type='application/xhtml+xml'/>\
         {manifest}</manifest>\
         <spine toc='ncx'><itemref idref='cover'/>{spine}</spine></package>",
        e(&title),
        e(who),
        now as i64
    );
    let ncx = format!(
        "<?xml version='1.0' encoding='utf-8'?>\n\
         <ncx xmlns='http://www.daisy.org/z3986/2005/ncx/' version='2005-1'>\
         <head></head><docTitle><text>{}</text></docTitle><navMap>\
         <navPoint id='cover' playOrder='0'><navLabel><text>封面</text></navLabel><content src='cover.xhtml'/></navPoint>\
         {nav}</navMap></ncx>",
        e(&title)
    );
    let cover = format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<!DOCTYPE html>\
         <html xmlns='http://www.w3.org/1999/xhtml'><head><title>封面</title>\
         <link rel='stylesheet' href='style.css' type='text/css'/></head>\
         <body><div class='cover'><h1>{}</h1><p>共 {} 张 · 导出于 {day}</p>\
         <p class='tip'>猫头鹰公益站</p></div></body></html>",
        e(&title),
        rows.len()
    );
    let css = "body{margin:0;font-family:sans-serif}.pg{text-align:center;padding:12px}\
               .pg img{max-width:100%;height:auto}.t{color:#888;font-size:.8em}\
               .p,.neg,.par{text-align:left;font-size:.85em;line-height:1.5;word-break:break-all}\
               .neg{color:#555}.par{color:#888;font-family:monospace;font-size:.78em}\
               .cover{text-align:center;padding:30% 10%}.cover h1{font-size:1.6em}.tip{color:#aaa;margin-top:40px}";
    let mut buf = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let stored = opts(CompressionMethod::Stored, clock, now);
        let deflated = opts(CompressionMethod::Deflated, clock, now);
        // EPUB 规定：第一个条目必须是未压缩的 mimetype
        let mut put = |name: &str, data: &[u8], o: SimpleFileOptions| {
            if z.start_file(name, o).is_ok() {
                let _ = z.write_all(data);
            }
        };
        put("mimetype", b"application/epub+zip", stored);
        put(
            "META-INF/container.xml",
            b"<?xml version='1.0'?>\n<container version='1.0' xmlns='urn:oasis:names:tc:opendocument:xmlns:container'>\
              <rootfiles><rootfile full-path='OEBPS/content.opf' media-type='application/oebps-package+xml'/>\
              </rootfiles></container>",
            deflated,
        );
        put("OEBPS/content.opf", opf.as_bytes(), deflated);
        put("OEBPS/toc.ncx", ncx.as_bytes(), deflated);
        put("OEBPS/style.css", css.as_bytes(), deflated);
        put("OEBPS/cover.xhtml", cover.as_bytes(), deflated);
        for p in &pages {
            put(&format!("OEBPS/{}", p.img), &p.bytes, deflated);
            put(&format!("OEBPS/{}", p.page), p.xhtml.as_bytes(), deflated);
        }
        let _ = z.finish();
    }
    buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
    ];

    fn rows() -> Vec<AuditImage> {
        vec![
            AuditImage {
                id: 1,
                ts: 1_000_000.0,
                image_type: "image/png".into(),
                prompt: "1girl, forest".into(),
                negative: "lowres".into(),
                extra: r#"{"params":{"seed":42,"sampler":"k_euler","steps":28,"scale":5,"width":832,"height":1216}}"#.into(),
                image: PNG.to_vec(),
            },
            AuditImage {
                id: 2,
                ts: 1_000_100.0,
                image_type: "image/png".into(),
                prompt: "1boy, city".into(),
                negative: String::new(),
                extra: String::new(),
                image: PNG.to_vec(),
            },
        ]
    }

    #[test]
    fn zip_has_images_and_prompts() {
        let clock = Clock::new("Asia/Shanghai", 7);
        let data = build_zip(&rows(), "小明", &clock, 1_760_000_000.0);
        let mut z = zip::ZipArchive::new(Cursor::new(data)).unwrap();
        let names: Vec<String> = z
            .file_names()
            .filter_map(Result::ok)
            .map(std::borrow::Cow::into_owned)
            .collect();
        assert_eq!(names.iter().filter(|n| n.ends_with(".png")).count(), 2);
        let mut txt = String::new();
        z.by_name("prompts.txt")
            .unwrap()
            .read_to_string(&mut txt)
            .unwrap();
        assert!(txt.starts_with("小明 的作品 · 共 2 张 · 导出于 2025-10-09 16:53\n"));
        assert!(txt.contains(
            "正面：1girl, forest\n负面：lowres\n参数：种子 42 · k_euler · 28步 · CFG 5 · 832×1216\n"
        ));
        assert!(txt.contains("[0002_19700112-214820.png]\n正面：1boy, city\n负面：\n"));
    }

    #[test]
    fn epub_is_valid_structure() {
        let clock = Clock::new("Asia/Shanghai", 7);
        let data = build_epub(&rows(), "小明", &clock, 1_760_000_000.0);
        let mut z = zip::ZipArchive::new(Cursor::new(data)).unwrap();
        let first = z.by_index(0).unwrap();
        assert_eq!(first.name().unwrap(), "mimetype");
        assert_eq!(first.compression(), CompressionMethod::Stored);
        drop(first);
        let mut mime = String::new();
        z.by_name("mimetype")
            .unwrap()
            .read_to_string(&mut mime)
            .unwrap();
        assert_eq!(mime, "application/epub+zip");
        let names: Vec<String> = z
            .file_names()
            .filter_map(Result::ok)
            .map(std::borrow::Cow::into_owned)
            .collect();
        for need in [
            "META-INF/container.xml",
            "OEBPS/content.opf",
            "OEBPS/toc.ncx",
        ] {
            assert!(names.iter().any(|n| n == need), "{need}");
        }
        assert_eq!(
            names.iter().filter(|n| n.starts_with("OEBPS/img")).count(),
            2
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("OEBPS/p") && n.ends_with(".xhtml"))
                .count(),
            2
        );
    }

    #[test]
    fn escape_like_python() {
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#x27;"
        );
    }
}
