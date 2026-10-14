//! Bug 追踪的纯逻辑（`app/errors.py`）：错误特征码、事件字段的截断规则、「新 bug / 复发」私信文案。
//!
//! 不移植的部分：
//! - `Tracker` 的 I/O（`_store_locked` 的 SQL、`list` / `resolve` / `purge` 等）。表是 `error_events`
//!   （`sig` 为键，`count` 累加，`resolved_at` 非空表示站长标记过「已处理」）。
//! - `_app_frame(exc)`：从 Python 回溯里找「最里层属于本项目代码的那一帧」，格式 `文件名.py:函数名`，
//!   用来区分「同一种异常、不同位置」。Rust 没有 Python 回溯，调用方自己决定「位置」字符串怎么取
//!   （例如 panic 的 `file:line` 或模块路径），传给 [`signature`] 即可；同一个位置要稳定。
//! - `traceback.format_exception` 生成的堆栈文本：由调用方传入（[`ExcInfo::stack`]）。
//!
//! 特征码 = `sha1(basis)` 的前 12 个十六进制字符。SHA-1 在这里只是去重用的指纹，不涉及安全，
//! 所以用一个自带的小实现（工作区依赖里没有 sha1），并用 golden 对拍保证与 `hashlib.sha1` 一致。

use crate::pyc;

pub const LEVELS: [&str; 2] = ["error", "warn"];
pub const DETAIL_MAX: usize = 4000;
/// 同一个 bug 的完整堆栈多久只打印一次（秒）。
pub const PRINT_STACK_INTERVAL: f64 = 600.0;
/// 同一个 bug 的私信冷却（秒）。
pub const NOTIFY_COOLDOWN: u32 = 3600;

// ---------------------------------------------------------------- SHA-1

/// SHA-1（RFC 3174），返回 40 位小写十六进制。
fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

// ---------------------------------------------------------------- 特征码

/// `\d` 在 Python 里是 Unicode 十进制数字（Nd，Unicode 15.0）。借 `pyc::parse_int` 判断，
/// 它的数字表就是按 CPython 3.12 的 unicodedata 版本整理的。
fn is_py_digit(c: char) -> bool {
    let mut buf = [0u8; 4];
    pyc::parse_int(c.encode_utf8(&mut buf)).is_some()
}

/// `_NUM.sub('#', title)[:200]`，`_NUM = r"\d+(\.\d+)?"`：把数字（含一个小数点）折叠成 `#`，
/// 让「上游返回 502 / 503」「耗时 1.5s / 2.5s」归到同一条 bug。
pub fn normalize_title(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    let mut out = String::with_capacity(title.len());
    let mut i = 0;
    while i < chars.len() {
        if is_py_digit(chars[i]) {
            while i < chars.len() && is_py_digit(chars[i]) {
                i += 1;
            }
            // 可选的小数部分：'.' 后至少一位数字
            if i + 1 < chars.len() && chars[i] == '.' && is_py_digit(chars[i + 1]) {
                i += 1;
                while i < chars.len() && is_py_digit(chars[i]) {
                    i += 1;
                }
            }
            out.push('#');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    pyc::take_chars(&out, 200).to_string()
}

/// 带异常时特征码只看「来源 + 异常类型 + 代码位置」。
#[derive(Debug, Clone, Copy)]
pub struct ExcKey<'a> {
    /// `type(exc).__name__`
    pub type_name: &'a str,
    /// `_app_frame(exc)`：`文件名.py:函数名`，没有本项目的帧时为空串。
    pub app_frame: &'a str,
}

/// `signature(source, exc, title)`：12 位十六进制特征码。
/// 有异常：`sha1("{source}|{类型}|{位置}")`；没有：`sha1("{source}|{折叠数字后的标题[:200]}")`。
pub fn signature(source: &str, exc: Option<ExcKey<'_>>, title: &str) -> String {
    let basis = match exc {
        Some(e) => format!("{source}|{}|{}", e.type_name, e.app_frame),
        None => format!("{source}|{}", normalize_title(title)),
    };
    sha1_hex(basis.as_bytes())[..12].to_string()
}

// ---------------------------------------------------------------- 事件

/// 异常信息（Rust 侧由调用方填）。
#[derive(Debug, Clone, Copy)]
pub struct ExcInfo<'a> {
    /// `type(exc).__name__`
    pub type_name: &'a str,
    /// `str(exc)`
    pub message: &'a str,
    /// 代码位置，见 [`ExcKey::app_frame`]。
    pub app_frame: &'a str,
    /// 完整堆栈文本（`"".join(traceback.format_exception(...))`）。
    pub stack: &'a str,
}

/// `capture(...)` 的入参。
#[derive(Debug, Clone, Copy)]
pub struct Capture<'a> {
    pub source: &'a str,
    pub exc: Option<ExcInfo<'a>>,
    pub title: &'a str,
    pub detail: &'a str,
    pub path: &'a str,
    pub key_id: Option<i64>,
    pub rid: &'a str,
    /// 不在 [`LEVELS`] 里的一律当 `error`。
    pub level: &'a str,
}

/// 入库 / 保留在内存里的事件（字段顺序即旧版 dict 的键顺序）。
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorEvent {
    pub sig: String,
    pub source: String,
    pub level: String,
    pub title: String,
    pub detail: String,
    pub path: String,
    pub key_id: Option<i64>,
    pub rid: String,
    pub ts: f64,
}

/// `capture` 的纯计算结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Captured {
    pub event: ErrorEvent,
    /// 一行日志：`[bug] {level} {source} sig={sig} rid={rid或-} {title[:160]}`。
    pub log_line: String,
    /// 若本次要打印完整堆栈（带异常且距上次打印超过 [`PRINT_STACK_INTERVAL`]），要打印的文本。
    pub stack_text: String,
}

/// 同一个 bug 的堆栈 10 分钟内只打印一次：`now - last.get(sig, 0) > 600`。
pub fn should_print_stack(now: f64, last_printed: Option<f64>) -> bool {
    now - last_printed.unwrap_or(0.0) > PRINT_STACK_INTERVAL
}

/// 事件字段的截断：保留末尾（堆栈的最后几行最有用）。
fn tail_chars(s: &str, n: usize) -> &str {
    pyc::last_chars(s, n)
}

/// `Tracker.capture` 里不碰 I/O 的部分：规整 level、拼标题 / 详情、算特征码、按字段上限截断。
pub fn build_event(c: &Capture<'_>, now: f64) -> Captured {
    let level = if LEVELS.contains(&c.level) {
        c.level
    } else {
        "error"
    };
    let mut title = c.title.to_string();
    let mut detail = c.detail.to_string();
    if let Some(e) = &c.exc {
        if title.is_empty() {
            title = format!("{}: {}", e.type_name, pyc::take_chars(e.message, 200));
        }
        detail = format!(
            "{}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!("{detail}\n")
            },
            e.stack
        );
    }
    let sig = signature(
        c.source,
        c.exc.map(|e| ExcKey {
            type_name: e.type_name,
            app_frame: e.app_frame,
        }),
        &title,
    );
    let rid_log = if c.rid.is_empty() { "-" } else { c.rid };
    let log_line = format!(
        "[bug] {level} {} sig={sig} rid={rid_log} {}",
        c.source,
        pyc::take_chars(&title, 160)
    );
    let stack_text = tail_chars(&detail, DETAIL_MAX).to_string();
    let event = ErrorEvent {
        sig,
        source: pyc::take_chars(c.source, 60).to_string(),
        level: level.to_string(),
        title: pyc::take_chars(&title, 300).to_string(),
        detail: stack_text.clone(),
        path: pyc::take_chars(c.path, 200).to_string(),
        key_id: c.key_id,
        rid: pyc::take_chars(c.rid, 16).to_string(),
        ts: now,
    };
    Captured {
        event,
        log_line,
        stack_text,
    }
}

// ---------------------------------------------------------------- 入库后的私信

/// 写库时按已有记录判断的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreState {
    /// 第一次出现。
    New,
    /// 站长标记「已处理」后又出现（复发）。
    Regressed,
    /// 已有未处理的同类记录：只累加计数，不打扰。
    Known,
}

/// `existing`：`SELECT count, resolved_at FROM error_events WHERE sig=?` 的结果里的 `resolved_at`
/// （外层 `None` = 没有这条记录；内层 `None` = `resolved_at` 为 NULL）。
pub fn store_state(existing: Option<Option<f64>>) -> StoreState {
    match existing {
        None => StoreState::New,
        Some(Some(_)) => StoreState::Regressed,
        Some(None) => StoreState::Known,
    }
}

/// 要私信站长的通知（`notify(key, text, cooldown_secs)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub key: String,
    pub text: String,
    pub cooldown_secs: u32,
}

/// 新 bug / 复发才通知，且只通知 `error` 级别。
pub fn notice_for(state: StoreState, e: &ErrorEvent) -> Option<Notice> {
    if state == StoreState::Known || e.level != "error" {
        return None;
    }
    let head = if state == StoreState::New {
        "🐞 新 bug"
    } else {
        "🐞 bug 复发"
    };
    let place = if e.rid.is_empty() {
        String::new()
    } else {
        format!(" · 请求 {}", e.rid)
    };
    Some(Notice {
        key: format!("bug_{}", e.sig),
        text: format!(
            "{head}（{}）：{}{place}。详情见后台「Bug 追踪」。",
            e.source,
            pyc::take_chars(&e.title, 200)
        ),
        cooldown_secs: NOTIFY_COOLDOWN,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn numbers_fold_to_hash() {
        assert_eq!(
            normalize_title("上游 502 耗时 1.5s 与 2.50.3"),
            "上游 # 耗时 #s 与 #.#"
        );
        assert_eq!(normalize_title("a1b22.c3."), "a#b#.c#.");
        assert_eq!(normalize_title("１２.５"), "#");
        assert_eq!(normalize_title("x"), "x");
    }

    #[test]
    fn notice_texts() {
        let c = Capture {
            source: "background",
            exc: None,
            title: "维护循环失败 3 次",
            detail: "",
            path: "",
            key_id: None,
            rid: "abcd1234",
            level: "weird",
        };
        let got = build_event(&c, 100.0);
        assert_eq!(got.event.level, "error");
        let n = notice_for(StoreState::New, &got.event).unwrap();
        assert!(
            n.text
                .starts_with("🐞 新 bug（background）：维护循环失败 3 次 · 请求 abcd1234。")
        );
        assert!(notice_for(StoreState::Known, &got.event).is_none());
    }
}
