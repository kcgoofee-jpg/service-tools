//! Python `format(x, "g")` 的复刻。
//!
//! `str()` / `repr()` 见 [`crate::pyext`]，`json.dumps` 见 [`crate::pyjson`]。

/// `f"{x:g}"`（默认 6 位有效数字；整数先转 float，所以 `1000000` 会变成 `1e+06`）。
pub fn fmt_g(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    const P: i32 = 6;
    // 先按 %e 舍入到 P 位有效数字，取舍入后的指数 X 决定用定点还是科学计数法
    let sci = format!("{:.*e}", (P - 1) as usize, x);
    let (_, exp) = sci.split_once('e').expect("{:e} 总有 e");
    let exp: i32 = exp.parse().expect("指数是整数");
    if (-4..P).contains(&exp) {
        let fixed = format!("{:.*}", (P - 1 - exp) as usize, x);
        strip_zeros(&fixed)
    } else {
        let (mant, _) = sci.split_once('e').expect("{:e} 总有 e");
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{}e{esign}{:02}", strip_zeros(mant), exp.abs())
    }
}

/// 去掉小数部分末尾的 0 和多余的小数点（`%g` 不带 `#` 标志时的行为）。
fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_format() {
        assert_eq!(fmt_g(0.0), "0");
        assert_eq!(fmt_g(100000.0), "100000");
        assert_eq!(fmt_g(1000000.0), "1e+06");
        assert_eq!(fmt_g(2_000_000_000.0), "2e+09");
        assert_eq!(fmt_g(8760.0), "8760");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(123456.7), "123457");
        assert_eq!(fmt_g(0.0001), "0.0001");
        assert_eq!(fmt_g(0.00001), "1e-05");
    }
}
