//! 生成记录：提示词与原图（旧版 `app/audit.py`）。均可单独关闭，自动过期。
//! 画廊小图从原图现场缩小，不另存缩略图。
//!
//! 本模块只放「记什么 / 怎么记」：
//! - [`capture_prompts`] / [`prompt_texts`]：从请求体取提示词与生成参数，逐字节复刻旧版（含旧版抛的异常）；
//! - [`full_image`] / [`make_thumbnail`]：从上游返回的 zip（或裸图片）里取第一张图；
//! - [`audit_notice`] 与 `audit_*` 设置读取：给成员看的披露文案，以及记录开关 / 保留时长。
//!
//! 调用点（出图成功后的后台任务、后台画廊接口）不在这里。
//!
//! 与旧版一致的怪癖（都是故意的，见 golden 夹具 `tests/fixtures/audit_*.jsonl*`）：
//! - 请求体里字段类型不对时，旧版直接对它 `.get(...)` / 迭代，抛 `AttributeError` / `TypeError`，
//!   调用方（`audit_generation`）把整条记录丢掉并记一条 bug。所以 [`capture_prompts`] 返回 `PyResult`，
//!   调用方要照样处理 `Err`（丢掉这次记录、记 bug），不能当成「没有提示词」；
//! - [`full_image`] 不校验内容：任何不超过 15 MiB 的字节串都原样存下，类型串靠魔数猜，猜不出就当 PNG；
//! - 缩略图要真的解码：像素超过 2500 万（Pillow 的 `MAX_IMAGE_PIXELS`）、损坏、截断都返回 `None`；
//! - zip 里取「后缀是 .png / .jpg / .jpeg / .webp（不分大小写）的第一个条目」，不看目录层级；
//!   同名条目以最后一个为准（Python `getinfo` 的行为）；
//! - `audit_flags` 里存着的天数是 `inf` / `1e999` 时旧版抛 `OverflowError`（没人捕获），这里返回 `DbError::Invalid`；
//!   空串 / `nan` / 乱码回到 7 天（`ValueError` 被捕获），不是环境默认值。
//!
//! 与旧版的差异（图片编解码换成 `image` crate，应并入 docs/v3-deviations.md；对拍见 tests/audit_golden.rs）：
//! - 字节不逐位相同：缩放核、JPEG 量化 / 子采样 / 霍夫曼优化与 Pillow 不同；尺寸、收不收、类型串一致；
//! - 只认 PNG / JPEG / WebP：Pillow 还会打开 GIF / BMP / TIFF 等，上游不会返回这些；
//! - zip 只支持 stored / deflate：旧版 `zipfile` 还能读 bzip2 / lzma，上游不会这样压；
//! - 截掉了 PNG 尾部（IEND 与最后一个 IDAT 的校验）但像素数据完整的图，Pillow 照样打开，这里拒绝；
//! - 16 位灰度 PNG 的颜色：Pillow 把大于 255 的值截成 255，这里取高 8 位（尺寸、收不收一致）；
//! - 动画图（APNG / 动画 WebP）取第一帧，与 Pillow 一致；
//! - `repr` 里的未分配码点（`str(非字符串提示词)` 时才会走到）见 `gate_core::pyext::repr_str` 的说明。

use crate::env::EnvConfig;
use gate_core::pyext::{PyErr, PyResult, py_str, type_name};
use gate_core::{jsondump, pyc, pyval, site_flags};
use gate_db::{Db, DbError};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::io::{Cursor, Read};

/// 画廊卡片按原图比例整张显示，高清屏上 512 会糊。
pub const THUMB_SIDE: u32 = 768;
pub const THUMB_QUALITY: u8 = 82;
pub const MAX_IMAGE_BYTES: usize = 15 * 1024 * 1024;
/// 已停用：原图不再按天数删（2026-10-11 站长）。
pub const IMAGE_RETENTION_KEY: &str = "audit_image_retention_days";
/// 原图占用上限（MB）；见 [`site_flags::IMAGE_CAP_MB`]。
pub const IMAGE_CAP_KEY: &str = "audit_image_cap_mb";
/// 成员只能打包下载最近 2 天的原图：只用来找回客户端里误删的图。
pub const MEMBER_DOWNLOAD_DAYS: i64 = 2;

/// Pillow 的 `Image.MAX_IMAGE_PIXELS`：超过即报错（默认只是警告），防止解压炸弹。
/// 旧版同时把 `DecompressionBombWarning` 升级成错误，所以「超过 1 倍」就已经拒绝。
pub const MAX_IMAGE_PIXELS: u64 = 25_000_000;
/// 缩略图只肯读声明大小不超过 40 MiB 的 zip 条目。
const MAX_THUMB_ZIP_ENTRY: u64 = 40 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// `extra.params` 里保留的生成参数（顺序即输出顺序）。
const PARAM_KEYS: [&str; 13] = [
    "seed",
    "sampler",
    "steps",
    "scale",
    "width",
    "height",
    "noise_schedule",
    "cfg_rescale",
    "sm",
    "sm_dyn",
    "ucPreset",
    "qualityToggle",
    "n_samples",
];

// ----------------------------------------------------------------------------- 提示词

/// `x.get(key)`：不是 dict 时 Python 抛 `AttributeError`。
fn dict_get<'a>(v: &'a Value, key: &str) -> PyResult<Option<&'a Value>> {
    match v {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(PyErr::Attribute(format!(
            "'{}' object has no attribute 'get'",
            type_name(other)
        ))),
    }
}

/// `str(v or "")`。
fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if pyval::truthy(x) => py_str(x),
        _ => String::new(),
    }
}

/// `body.get("parameters") if isinstance(body.get("parameters"), dict) else {}`。
/// `body` 本身不是 dict 时 `.get` 抛 `AttributeError`。
fn params_of(body: &Value) -> PyResult<Option<&Map<String, Value>>> {
    Ok(dict_get(body, "parameters")?.and_then(Value::as_object))
}

fn param<'a>(params: Option<&'a Map<String, Value>>, key: &str) -> Option<&'a Value> {
    params.and_then(|m| m.get(key))
}

/// 正向 / 负向提示词（截断，防止超长内容写库）。
pub fn prompt_texts(body: &Value) -> PyResult<(String, String)> {
    let params = params_of(body)?;
    let mut positive = str_or_empty(dict_get(body, "input")?);
    let negative = str_or_empty(param(params, "negative_prompt"));
    if positive.is_empty() {
        // ((params.get("v4_prompt") or {}).get("caption") or {}) if isinstance(v4_prompt, dict) else {}
        let caption = match param(params, "v4_prompt") {
            Some(Value::Object(v4)) => v4.get("caption").filter(|c| pyval::truthy(c)),
            _ => None,
        };
        // caption 是真值但不是 dict（字符串 / 列表 / 数字）时 `.get` 抛 AttributeError
        positive = match caption {
            Some(c) => str_or_empty(dict_get(c, "base_caption")?),
            None => String::new(),
        };
    }
    Ok((
        pyc::take_chars(&positive, 2000).to_string(),
        pyc::take_chars(&negative, 1000).to_string(),
    ))
}

/// 从 `v4_prompt` / `v4_negative_prompt` 里取 base_caption 和各角色 caption（去掉空的）；旧版 `_captions`。
pub fn captions(node: Option<&Value>) -> PyResult<(String, Vec<String>)> {
    let cap = match node {
        Some(Value::Object(m)) => m.get("caption").filter(|c| pyval::truthy(c)),
        _ => None,
    };
    let Some(cap) = cap else {
        return Ok((String::new(), Vec::new()));
    };
    let base = str_or_empty(dict_get(cap, "base_caption")?);
    // cap.get("char_captions") or []：到这里 cap 一定是 dict（否则上一行已经抛了）
    let listed = dict_get(cap, "char_captions")?.filter(|c| pyval::truthy(c));
    let items: &[Value] = match listed {
        None => &[],
        Some(Value::Array(a)) => a,
        // 字符串迭代出单个字符、dict 迭代出键：都是 str，通不过 isinstance(c, dict)，等于空
        Some(Value::String(_) | Value::Object(_)) => &[],
        Some(other) => {
            return Err(PyErr::Type(format!(
                "'{}' object is not iterable",
                type_name(other)
            )));
        }
    };
    let chars = items
        .iter()
        .filter_map(Value::as_object)
        .map(|c| str_or_empty(c.get("char_caption")))
        .filter(|c| !c.is_empty())
        .collect();
    Ok((base, chars))
}

/// 完整提示词：返回 `(正面, 负面, extra_json)`。
///
/// extra 里是「正面 / 负面」之外的东西——角色提示词（多人图）和生成参数（种子 / 采样器 / 步数 / CFG / 尺寸…），
/// 否则光存正负面会漏掉多角色场景和复现所需的参数。没有任何这类内容时 extra 是空串。
///
/// `Err` 对应旧版抛出的 `AttributeError` / `TypeError`（请求体字段类型不对）：调用方要丢掉整条记录。
pub fn capture_prompts(body: &Value) -> PyResult<(String, String, String)> {
    let (mut positive, mut negative) = prompt_texts(body)?;
    let params = params_of(body)?;
    let (pos_base, pos_chars) = captions(param(params, "v4_prompt"))?;
    let (neg_base, neg_chars) = captions(param(params, "v4_negative_prompt"))?;
    if positive.is_empty() {
        positive = pos_base;
    }
    if negative.is_empty() {
        // 只在 v4_negative_prompt 里写负面词的客户端，以前会漏记
        negative = neg_base;
    }
    let mut kept = Map::new();
    for key in PARAM_KEYS {
        if let Some(v) = param(params, key).filter(|v| !v.is_null()) {
            kept.insert(key.to_string(), v.clone());
        }
    }
    let to_values = |chars: Vec<String>| -> Vec<Value> {
        chars.into_iter().take(12).map(Value::String).collect()
    };
    let (char_prompts, char_negatives) = (to_values(pos_chars), to_values(neg_chars));
    let has = !char_prompts.is_empty() || !char_negatives.is_empty() || !kept.is_empty();
    let extra = if has {
        let mut extra = Map::new();
        extra.insert("char_prompts".into(), Value::Array(char_prompts));
        extra.insert("char_negatives".into(), Value::Array(char_negatives));
        extra.insert("params".into(), Value::Object(kept));
        pyc::take_chars(&jsondump::dumps(&Value::Object(extra)), 4000).to_string()
    } else {
        String::new()
    };
    Ok((
        pyc::take_chars(&positive, 4000).to_string(),
        pyc::take_chars(&negative, 2000).to_string(),
        extra,
    ))
}

// ----------------------------------------------------------------------------- 图片

fn is_image_name(raw: &[u8]) -> bool {
    // Python：n.lower().endswith((".png", ".jpg", ".jpeg", ".webp"))。
    // 后缀全是 ASCII，条目名无论按 UTF-8 还是 cp437 解码，ASCII 部分都一样，直接比字节。
    let lower = raw.to_ascii_lowercase();
    [&b".png"[..], b".jpg", b".jpeg", b".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// zip 里第一个图片条目的内容；没有、声明大小超过 `max_declared`、压缩包损坏、CRC 对不上都返回 `None`。
fn first_zip_image(payload: &[u8], max_declared: u64) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(payload)).ok()?;
    let mut picked = None;
    for i in 0..archive.len() {
        let entry = archive.by_index_data(i).ok()?;
        if is_image_name(entry.name_raw()) {
            picked = Some((i, entry.size()));
            break;
        }
    }
    // 重名条目：zip crate 的索引保留第一次出现的位置、内容取最后一个，与 Python `getinfo(names[0])` 一致
    let (index, declared) = picked?;
    if declared > max_declared {
        return None;
    }
    let mut entry = archive.by_index(index).ok()?;
    let mut data = Vec::with_capacity(usize::try_from(declared).unwrap_or(0).min(MAX_IMAGE_BYTES));
    // 多读 1 字节：读到条目真正结束（返回 0）zip 才会核对 CRC。声明大小说谎（压缩炸弹 / 截断）时长度对不上，
    // 旧版 `ZipFile.read` 会因 CRC 不符抛 BadZipFile，同样当作失败
    (&mut entry)
        .take(declared + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 != declared {
        return None;
    }
    Some(data)
}

/// `reducing_gap` 之外 Pillow `Image.thumbnail` 算最终尺寸的方式：保持比例、不放大。
/// 返回 `None` 表示不用缩（两边都没超过上限）。
fn thumbnail_size(width: u32, height: u32) -> Option<(u32, u32)> {
    let (x, y) = (i64::from(THUMB_SIDE), i64::from(THUMB_SIDE));
    if x >= i64::from(width) && y >= i64::from(height) {
        return None;
    }
    let aspect = f64::from(width) / f64::from(height);
    // max(min(floor(n), ceil(n), key=key), 1)：并列时取 floor
    let round_aspect = |number: f64, key: &dyn Fn(f64) -> f64| -> i64 {
        let (lo, hi) = (number.floor(), number.ceil());
        let best = if key(hi) < key(lo) { hi } else { lo };
        (best as i64).max(1)
    };
    let (mut tx, mut ty) = (x, y);
    if (x as f64) / (y as f64) >= aspect {
        tx = round_aspect(y as f64 * aspect, &|n| (aspect - n / y as f64).abs());
    } else {
        ty = round_aspect(x as f64 / aspect, &|n| {
            if n == 0.0 {
                0.0
            } else {
                (aspect - x as f64 / n).abs()
            }
        });
    }
    // 极端长宽比下取整可能超出 u32：缩略图每边最多 768，不会
    Some((tx as u32, ty as u32))
}

/// JPEG 是否完整：第一个 SOS 之后还能找到 EOI（`FF D9`；熵编码数据里的 `FF` 一定被填充成 `FF 00`，不会冒出 `FF D9`）。
///
/// image crate 用的 zune-jpeg 对截断的文件很宽容（缺的部分补灰，照样返回一张图），
/// Pillow 则报 "image file is truncated"：旧版缩略图因此是 `None`，这里补上这道判断。
fn jpeg_is_complete(data: &[u8]) -> bool {
    let mut i = 2; // 跳过 SOI
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return false;
        }
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1; // 填充字节
            continue;
        }
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if marker == 0xDA {
            return data
                .get(i + 2 + len..)
                .is_some_and(|scan| scan.windows(2).any(|w| w == [0xFF, 0xD9]));
        }
        i += 2 + len;
    }
    false
}

/// 解码成 RGB：先看头部像素数（超过上限不解码），再完整解码（截断 / 损坏在这里暴露）。
fn decode_rgb(data: &[u8]) -> Option<image::RgbImage> {
    let probe = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let format = probe.format()?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return None;
    }
    let (w, h) = probe.into_dimensions().ok()?;
    if format == ImageFormat::Jpeg && !jpeg_is_complete(data) {
        return None;
    }
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return None;
    }
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(w);
    limits.max_image_height = Some(h);
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    // 与 Pillow 的 convert("RGB") 一样：带透明通道的图只丢掉 alpha，不合成背景
    Some(reader.decode().ok()?.to_rgb8())
}

/// 从上游返回的 zip（或裸图片）里取第一张图，缩成小 JPEG；失败返回 `None`。
pub fn make_thumbnail(payload: &[u8]) -> Option<Vec<u8>> {
    let data: Cow<[u8]> = if payload.starts_with(b"PK") {
        Cow::Owned(first_zip_image(payload, MAX_THUMB_ZIP_ENTRY)?)
    } else {
        Cow::Borrowed(payload)
    };
    let mut rgb = decode_rgb(&data)?;
    if let Some((w, h)) = thumbnail_size(rgb.width(), rgb.height())
        && (w, h) != rgb.dimensions()
    {
        rgb = image::imageops::resize(&rgb, w, h, FilterType::CatmullRom);
    }
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(rgb)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, THUMB_QUALITY))
        .ok()?;
    Some(out)
}

/// 取上游返回的第一张图的原始字节（不缩放、不重压），超过大小上限则不存。返回 `(字节, content_type)`。
///
/// 不检查内容是不是图片（旧版如此）：类型串按魔数猜，猜不出就是 `image/png`；失败时是 `(None, "")`。
pub fn full_image(payload: &[u8]) -> (Option<Vec<u8>>, String) {
    let data: Cow<[u8]> = if payload.starts_with(b"PK") {
        match first_zip_image(payload, MAX_IMAGE_BYTES as u64) {
            Some(d) => Cow::Owned(d),
            None => return (None, String::new()),
        }
    } else {
        Cow::Borrowed(payload)
    };
    if data.len() > MAX_IMAGE_BYTES {
        return (None, String::new());
    }
    let ctype = if data.starts_with(PNG_MAGIC) {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "image/png"
    };
    (Some(data.into_owned()), ctype.to_string())
}

// ----------------------------------------------------------------------------- 披露文案

/// 向成员披露记录范围（提示词、原图各自保留多久）；未开启记录则返回空串。
/// 「记录生成结果」开关只保存原图（不再另存缩略图），保留天数单独设置；0 天 = 不存图。
///
/// 旧版 `image_days=0, keep_hours=24` 是参数默认值；这里调用方显式传。
pub fn audit_notice(
    prompts: bool,
    thumbs: bool,
    days: i64,
    image_days: i64,
    keep_hours: i64,
) -> String {
    let keep = |d: i64| {
        if d > 0 {
            format!("{d} 天后自动删除")
        } else {
            "长期保存".to_string()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if prompts {
        parts.push(format!("图片提示词（{}）", keep(days)));
    }
    if thumbs {
        let keep_txt = if keep_hours > 0 {
            format!("{keep_hours} 小时后自动删除")
        } else {
            "服务器空间满了会自动删掉最旧的".to_string()
        };
        // image_days * 24 在极端大值下可能溢出 i64（Python 是大整数）：饱和即可，比较结果不变
        let window = if 0 < keep_hours && keep_hours < image_days.saturating_mul(24) {
            format!("{keep_hours} 小时")
        } else {
            format!("{image_days} 天")
        };
        let download = if image_days > 0 {
            format!("；在这之前可在首页打包下载最近 {window} 的，用来找回误删的图")
        } else {
            String::new()
        };
        parts.push(format!("生成的原图（{keep_txt}{download}）"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "为防止滥用和优化调度，本站会保留你的{}，仅站长可见。",
        parts.join("、")
    )
}

// ----------------------------------------------------------------------------- 设置读取

/// `site_flags.get(db, spec)`（不带 settings）：读一行设置，按登记表解析，缺失 / 格式不对回到默认值。
async fn site_value(db: &Db, spec: &site_flags::Spec) -> Result<Value, DbError> {
    let raw = db.get_setting(spec.key).await?;
    Ok(site_flags::get_from_raw(spec, raw.as_deref(), None))
}

async fn site_int(db: &Db, spec: &site_flags::Spec) -> Result<i64, DbError> {
    let value = site_value(db, spec).await?;
    // 整数类设置解析后一定是整数（缺失时是登记表的默认值）；兜底只为不 panic
    let fallback = match spec.default {
        site_flags::SpecDefault::Int(i) => i,
        _ => 0,
    };
    Ok(value.as_i64().unwrap_or(fallback))
}

/// 成员能打包下载最近几天的原图：开了下载就是 2 天，关了是 0（首页显示「未开启」，下载接口拒绝）。
pub async fn audit_image_days(db: &Db) -> Result<i64, DbError> {
    let on = site_value(db, &site_flags::MEMBER_DOWNLOAD).await?;
    Ok(if pyval::truthy(&on) {
        MEMBER_DOWNLOAD_DAYS
    } else {
        0
    })
}

/// 原图在服务器上保存几小时（0 = 只按容量上限删）。
pub async fn audit_image_keep_hours(db: &Db) -> Result<i64, DbError> {
    site_int(db, &site_flags::IMAGE_KEEP_HOURS).await
}

/// 原图占用上限（MB，0 = 不限）。
pub async fn audit_image_cap_mb(db: &Db) -> Result<i64, DbError> {
    site_int(db, &site_flags::IMAGE_CAP_MB).await
}

/// 所有给成员看的「本站记录了什么」都走这一个函数，避免各处文案再各写一套。
pub async fn audit_disclosure(db: &Db, env: &EnvConfig) -> Result<String, DbError> {
    let (prompts, thumbs, days) = audit_flags(db, env).await?;
    let image_days = audit_image_days(db).await?;
    let keep_hours = audit_image_keep_hours(db).await?;
    Ok(audit_notice(prompts, thumbs, days, image_days, keep_hours))
}

/// 旧版 `truth()`：`str(value).strip().lower() in ("1", "true", "yes", "on")`。
fn truth(raw: &str) -> bool {
    matches!(
        pyc::strip(raw).to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// 记录开关（提示词、生成结果 / 原图、提示词保留天数）：后台保存的设置优先，其次是环境变量。
/// 第二项沿用旧设置名 `audit_thumbs`，含义已变为「保存原图」。
///
/// 存着的值只要有这一行（哪怕是空串）就压过环境默认值；天数 0 = 长期保留，范围 0–3650。
pub async fn audit_flags(db: &Db, env: &EnvConfig) -> Result<(bool, bool, i64), DbError> {
    let prompts = match db.get_setting("audit_prompts").await? {
        Some(raw) => truth(&raw),
        None => env.audit_prompts,
    };
    let thumbs = match db.get_setting("audit_thumbs").await? {
        Some(raw) => truth(&raw),
        None => env.audit_thumbs,
    };
    let days = match db.get_setting("audit_retention_days").await? {
        // int(float(value))：ValueError（空串 / 乱码 / nan）回到 7；inf 是 OverflowError，旧版没捕获
        Some(raw) => match pyc::parse_float(&raw) {
            Some(f) if f.is_infinite() => {
                return Err(DbError::Invalid(format!(
                    "audit_retention_days={raw:?}: OverflowError: cannot convert float infinity to integer"
                )));
            }
            Some(f) if !f.is_nan() => clamp_days(f.trunc()),
            _ => 7,
        },
        None => clamp_days(env.audit_retention_days as f64),
    };
    Ok((prompts, thumbs, days))
}

/// `max(0, min(days, 3650))`（`days` 已向零取整）。
fn clamp_days(days: f64) -> i64 {
    days.clamp(0.0, 3650.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_match_site_flags_registry() {
        assert_eq!(IMAGE_RETENTION_KEY, site_flags::IMAGE_RETENTION.key);
        assert_eq!(IMAGE_CAP_KEY, site_flags::IMAGE_CAP_MB.key);
    }

    #[test]
    fn prompts_basic_shapes() {
        let body = json!({
            "input": "1girl, solo",
            "parameters": {"negative_prompt": "lowres", "seed": 7, "scale": 5.0, "sm": false, "steps": null}
        });
        let (p, n, e) = capture_prompts(&body).unwrap();
        assert_eq!((p.as_str(), n.as_str()), ("1girl, solo", "lowres"));
        assert_eq!(
            e,
            r#"{"char_prompts": [], "char_negatives": [], "params": {"seed": 7, "scale": 5.0, "sm": false}}"#
        );
        // 什么都没有：extra 是空串
        assert_eq!(
            capture_prompts(&json!({"input": "x"})).unwrap(),
            ("x".into(), String::new(), String::new())
        );
        // 字段类型不对：旧版抛异常，整条记录被丢
        let err = capture_prompts(&json!({"parameters": {"v4_prompt": {"caption": "oops"}}}))
            .unwrap_err();
        assert_eq!(err.type_name(), "AttributeError");
        assert_eq!(err.message(), "'str' object has no attribute 'get'");
        let err = capture_prompts(
            &json!({"parameters": {"v4_prompt": {"caption": {"char_captions": 5}}}}),
        )
        .unwrap_err();
        assert_eq!(err.type_name(), "TypeError");
        assert!(capture_prompts(&json!([1])).is_err());
    }

    #[test]
    fn thumbnail_sizes_follow_pillow() {
        // 与 Pillow 12 `Image.thumbnail((768, 768))` 的实测结果一致（见夹具 audit_images）
        assert_eq!(thumbnail_size(768, 768), None);
        assert_eq!(thumbnail_size(100, 50), None);
        assert_eq!(thumbnail_size(769, 769), Some((768, 768)));
        assert_eq!(thumbnail_size(1000, 700), Some((768, 538)));
        assert_eq!(thumbnail_size(3000, 100), Some((768, 26)));
        assert_eq!(thumbnail_size(1, 30000), Some((1, 768)));
        assert_eq!(thumbnail_size(30000, 1), Some((768, 1)));
        assert_eq!(thumbnail_size(999, 1001), Some((766, 768)));
    }

    #[test]
    fn full_image_types_and_limits() {
        assert_eq!(full_image(b""), (Some(vec![]), "image/png".into()));
        assert_eq!(
            full_image(b"\xff\xd8\xffabc"),
            (Some(b"\xff\xd8\xffabc".to_vec()), "image/jpeg".into())
        );
        assert_eq!(full_image(b"RIFF\0\0\0\0WEBPx").1, "image/webp");
        assert_eq!(full_image(b"RIFF\0\0\0\0WAVEx").1, "image/png");
        assert_eq!(full_image(b"PKnot a zip"), (None, String::new()));
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert_eq!(full_image(&big), (None, String::new()));
        assert!(full_image(&big[..MAX_IMAGE_BYTES]).0.is_some());
    }

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)))
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
            .unwrap();
        out
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        for (name, data) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
        buf.into_inner()
    }

    fn thumb_size(payload: &[u8]) -> Option<(u32, u32)> {
        let t = make_thumbnail(payload)?;
        assert!(t.starts_with(b"\xff\xd8\xff"), "缩略图是 JPEG");
        let img = image::load_from_memory_with_format(&t, ImageFormat::Jpeg).unwrap();
        Some((img.width(), img.height()))
    }

    #[test]
    fn thumbnail_decisions_on_synthetic_images() {
        // 小图不放大；大图按比例缩到 768 以内
        assert_eq!(thumb_size(&png(8, 8, [200, 30, 30])), Some((8, 8)));
        assert_eq!(thumb_size(&png(1600, 800, [30, 200, 30])), Some((768, 384)));
        assert_eq!(
            thumb_size(&jpeg(900, 1800, [30, 30, 200])),
            Some((384, 768))
        );
        // zip：按后缀挑第一个图片条目（不分大小写、不看目录），按内容解码
        let (a, b) = (png(16, 16, [1, 2, 3]), png(32, 32, [4, 5, 6]));
        let z = zip_of(&[("notes.txt", b"hi"), ("sub/IMG.PNG", &a), ("b.png", &b)]);
        assert_eq!(thumb_size(&z), Some((16, 16)));
        assert_eq!(thumb_size(&zip_of(&[("a.gif", &a)])), None);
        assert_eq!(thumb_size(&zip_of(&[("a.png", b"garbage")])), None);
        assert_eq!(thumb_size(&zip_of(&[])), None);
        // JPEG：EOI 之后有多余字节照收；截断不收（zune-jpeg 本身会补灰）
        let j = jpeg(300, 200, [90, 90, 90]);
        let mut padded = j.clone();
        padded.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(thumb_size(&padded), Some((300, 200)));
        assert_eq!(thumb_size(&j[..j.len() / 2]), None);
        assert_eq!(thumb_size(&j[..j.len() - 2]), None);
        // 损坏 / 不是图片
        let p = png(64, 64, [9, 9, 9]);
        assert_eq!(thumb_size(&p[..p.len() / 2]), None);
        for junk in [
            &b""[..],
            b"x",
            b"PK",
            b"PK\x03\x04junk",
            &[0x89, b'P', b'N', b'G'],
            b"GIF89a",
        ] {
            assert_eq!(thumb_size(junk), None);
        }
    }

    #[test]
    fn full_image_extracts_first_zip_entry_without_decoding() {
        let (a, b) = (png(16, 16, [1, 2, 3]), jpeg(16, 16, [4, 5, 6]));
        let z = zip_of(&[("readme.txt", b"hi"), ("1.JPG", &b), ("2.png", &a)]);
        assert_eq!(full_image(&z), (Some(b.clone()), "image/jpeg".into()));
        assert_eq!(
            full_image(&zip_of(&[("2.png", &a)])),
            (Some(a), "image/png".into())
        );
        // 不检查内容：后缀对就原样存
        assert_eq!(
            full_image(&zip_of(&[("a.webp", b"not really")])),
            (Some(b"not really".to_vec()), "image/png".into())
        );
        assert_eq!(
            full_image(&zip_of(&[("a.txt", b"x")])),
            (None, String::new())
        );
        assert_eq!(full_image(&zip_of(&[])), (None, String::new()));
        assert_eq!(full_image(b"PK\x03\x04junk"), (None, String::new()));
    }

    #[test]
    fn notice_wording() {
        assert_eq!(audit_notice(false, false, 7, 0, 24), "");
        assert_eq!(
            audit_notice(true, false, 7, 0, 24),
            "为防止滥用和优化调度，本站会保留你的图片提示词（7 天后自动删除），仅站长可见。"
        );
        assert_eq!(
            audit_notice(true, true, 0, 2, 24),
            "为防止滥用和优化调度，本站会保留你的图片提示词（长期保存）、生成的原图（24 小时后自动删除；在这之前可在首页打包下载最近 24 小时 的，用来找回误删的图），仅站长可见。"
        );
    }
}
