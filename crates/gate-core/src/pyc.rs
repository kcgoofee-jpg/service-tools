//! Python 语义复刻：旧网关是 Python 3.12，切换期间两边要算出逐位相同的数。
//!
//! 这里只放「Rust 默认行为与 CPython 不同」的那几处：舍入、求和、浮点转字符串、
//! 字符串解析成数字、按码点切片、空白判定、整除 / 取模。每个函数都有 golden 夹具对拍
//! （tools/parity/golden/pyc.py）。

/// `round(x)`（不带 ndigits）：银行家舍入，结果是整数。
pub fn round0(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `round(x, n)`（n ≥ 0）：CPython 先把 x 正确舍入成 n 位小数的十进制串再解析回来；
/// Rust 的 `{:.n}` 同样是对精确二进制值做 ties-even 的正确舍入，两者逐位一致。
pub fn round(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() || ndigits > 300 {
        return x;
    }
    let s = format!("{x:.ndigits$}");
    // -0.0 舍入后 CPython 保留符号：round(-0.001, 2) == -0.0
    let v: f64 = s.parse().unwrap_or(x);
    if v == 0.0 && x.is_sign_negative() {
        -0.0
    } else {
        v
    }
}

/// `sum(floats)`：CPython 3.12 起对浮点求和用 Neumaier 补偿（与朴素累加结果不同）。
pub fn sum<I: IntoIterator<Item = f64>>(items: I) -> f64 {
    let mut f = 0.0_f64;
    let mut c = 0.0_f64;
    for x in items {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// `int(x)` 对浮点：向零截断。非有限值在 Python 里会抛异常，这里返回 None。
pub fn int_of(x: f64) -> Option<i64> {
    if x.is_finite() {
        Some(x.trunc() as i64)
    } else {
        None
    }
}

/// `a // b`（整数）：向负无穷取整。
pub fn floordiv(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// `a % b`（整数）：结果与除数同号。
pub fn modulo(a: i64, b: i64) -> i64 {
    let r = a % b;
    if r != 0 && ((r < 0) != (b < 0)) {
        r + b
    } else {
        r
    }
}

/// `a // b`（浮点）。
pub fn floordiv_f(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

/// `repr(float)` / `str(float)`：最短往返表示；指数在 [-4, 16) 之外用科学计数法，
/// 指数至少两位并带符号（`1e-05`、`1e+16`），整数值补 `.0`。
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // Rust 的 {:e} 给出最短往返的十进制数字：d.ddd e±N
    let sci = format!("{:e}", x.abs());
    let (mant, exp) = sci.split_once('e').expect("{:e} 总有 e");
    let exp: i32 = exp.parse().expect("指数是整数");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1; // 小数点在第几位数字之后
    let sign = if x < 0.0 { "-" } else { "" };
    if decpt <= -4 || decpt > 16 {
        let (head, tail) = digits.split_at(1);
        let mantissa = if tail.is_empty() {
            head.to_string()
        } else {
            format!("{head}.{tail}")
        };
        let e = decpt - 1;
        let esign = if e < 0 { '-' } else { '+' };
        return format!("{sign}{mantissa}e{esign}{:02}", e.abs());
    }
    let n = digits.len() as i32;
    let body = if decpt <= 0 {
        format!("0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt >= n {
        format!("{}{}.0", digits, "0".repeat((decpt - n) as usize))
    } else {
        let (a, b) = digits.split_at(decpt as usize);
        format!("{a}.{b}")
    };
    format!("{sign}{body}")
}

/// Python `str.isspace()` 的字符集（比 Rust `char::is_whitespace` 多 \x1c–\x1f）。
pub fn is_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0b' | '\x0c' | '\r' | '\x1c'..='\x1f' | ' ' | '\u{85}' | '\u{a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// `s.strip()`
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s[:n]`：按码点切，不按字节。
pub fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `s[-n:]`：最后 n 个码点。
pub fn last_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if n >= count {
        return s;
    }
    let skip = count - n;
    match s.char_indices().nth(skip) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

/// `len(s)`：码点数。
pub fn len_chars(s: &str) -> usize {
    s.chars().count()
}

/// Unicode 15.0（CPython 3.12 的 unicodedata 版本）里所有十进制数字块的「0」码点：
/// Python 的 `int()` / `float()` 接受任何 Nd 类数字（如全角「１２」）。
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
    0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
    0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650,
    0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0,
    0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];

/// Nd 类数字 → 0..=9。
fn decimal_value(c: char) -> Option<u32> {
    let cp = c as u32;
    DECIMAL_ZEROS
        .iter()
        .find(|&&z| cp >= z && cp < z + 10)
        .map(|&z| cp - z)
}

/// 把 Nd 数字统一成 ASCII（其余字符原样保留）。
fn ascii_digits(s: &str) -> String {
    s.chars()
        .map(|c| match decimal_value(c) {
            Some(d) => char::from_digit(d, 10).unwrap(),
            None => c,
        })
        .collect()
}

/// 数字字面量里的下划线规则（PEP 515）：只能夹在两个数字之间。
fn underscores_ok(s: &str) -> bool {
    let b = s.as_bytes();
    for (i, &ch) in b.iter().enumerate() {
        if ch == b'_' {
            let prev_digit = i > 0 && b[i - 1].is_ascii_digit();
            let next_digit = i + 1 < b.len() && b[i + 1].is_ascii_digit();
            if !(prev_digit && next_digit) {
                return false;
            }
        }
    }
    true
}

/// `int(s)`（base 10）：允许首尾空白、正负号、PEP 515 下划线；不接受小数点。
/// 接受任何 Unicode 十进制数字（与 CPython 一致）。
pub fn parse_int(s: &str) -> Option<i64> {
    let owned = ascii_digits(strip(s));
    let t = owned.as_str();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty()
        || !body.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        || !underscores_ok(body)
    {
        return None;
    }
    let digits: String = body.chars().filter(|c| *c != '_').collect();
    let v: i64 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

/// `float(s)`：允许首尾空白、正负号、下划线、inf / nan / infinity（不区分大小写）。
pub fn parse_float(s: &str) -> Option<f64> {
    let owned = ascii_digits(strip(s));
    let t = owned.as_str();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let lower = body.to_ascii_lowercase();
    let v = match lower.as_str() {
        "inf" | "infinity" => f64::INFINITY,
        "nan" => f64::NAN,
        _ => {
            if body.is_empty()
                || !matches!(body.as_bytes()[0], b'0'..=b'9' | b'.')
                || !body.bytes().all(|b| {
                    b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-' | b'_')
                })
                || !underscores_ok(body)
            {
                return None;
            }
            let cleaned: String = body.chars().filter(|c| *c != '_').collect();
            // Rust 接受 "1." ".5" "1e5"，与 Python 一致；拒绝 "e5" "." 也一致
            cleaned.parse::<f64>().ok()?
        }
    };
    Some(if neg { -v } else { v })
}

/// `int(float(s))`：设置值的常见读法（如 guard / quota_algo）。
pub fn parse_int_via_float(s: &str) -> Option<i64> {
    parse_float(s).and_then(int_of)
}

/// `str(v)` 对 bool：`"True"` / `"False"`。
pub fn bool_str(v: bool) -> &'static str {
    if v { "True" } else { "False" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_matches_cpython() {
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(0.375, 2), 0.38);
        assert_eq!(round0(0.5), 0);
        assert_eq!(round0(1.5), 2);
        assert_eq!(round0(2.5), 2);
        assert_eq!(round0(-2.5), -2);
        assert!(round(-0.001, 2).is_sign_negative());
    }

    #[test]
    fn repr_matches_cpython() {
        for (x, s) in [
            (1e-5, "1e-05"),
            (123456.0, "123456.0"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (1.5e-7, "1.5e-07"),
            (0.1, "0.1"),
            (-2.5, "-2.5"),
            (1.0 / 3.0, "0.3333333333333333"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (-0.0, "-0.0"),
        ] {
            assert_eq!(float_repr(x), s, "{x}");
        }
    }

    #[test]
    fn neumaier_differs_from_naive() {
        let xs = [1e16, 1.0, -1e16];
        assert_eq!(sum(xs), 1.0);
        assert_eq!(xs.iter().sum::<f64>(), 0.0);
    }

    #[test]
    fn parse_like_python() {
        assert_eq!(parse_int(" 1_000 "), Some(1000));
        assert_eq!(parse_int("1__0"), None);
        assert_eq!(parse_int("12.0"), None);
        assert_eq!(parse_int("+5"), Some(5));
        assert_eq!(parse_float("1e3"), Some(1000.0));
        assert_eq!(parse_int_via_float("15.9"), Some(15));
        assert_eq!(parse_int_via_float("-15.9"), Some(-15));
        assert_eq!(parse_int_via_float("inf"), None);
        assert_eq!(parse_float("abc"), None);
    }

    #[test]
    fn slicing_by_code_point() {
        assert_eq!(take_chars("猫头鹰abc", 2), "猫头");
        assert_eq!(last_chars("猫头鹰abc", 2), "bc");
        assert_eq!(take_chars("ab", 5), "ab");
        assert_eq!(floordiv(-7, 2), -4);
        assert_eq!(modulo(-7, 3), 2);
    }
}
