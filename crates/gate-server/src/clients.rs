//! 客户端名：按 User-Agent 与特征头归类（旧版 request_timing.begin）。完整的请求体指纹识别在
//! gate_core（streams 子任务合并后接上）；这里先放 UA 归类，逻辑与旧版逐条一致。

const CLIENT_MAX: usize = 256;

/// 去控制字符、去首尾空白、截到 256 个码点（旧版 `client_name`）。
pub fn client_name(ua: &str) -> String {
    let mut out = String::with_capacity(ua.len());
    let mut in_ctrl = false;
    for c in ua.chars() {
        if c <= '\u{1f}' || c == '\u{7f}' {
            if !in_ctrl {
                out.push(' ');
                in_ctrl = true;
            }
        } else {
            out.push(c);
            in_ctrl = false;
        }
    }
    gate_core::pyc::take_chars(gate_core::pyc::strip(&out), CLIENT_MAX).to_string()
}

fn tagged(cname: &str, prefix: &str, label: &str) -> String {
    if cname.starts_with(prefix) {
        cname.to_string()
    } else {
        gate_core::pyc::take_chars(&format!("{label} ({cname})"), CLIENT_MAX).to_string()
    }
}

/// `aalice`：请求带 x-correlation-id / x-initiated-at 头（Aaalice NAI Launcher）。
pub fn classify(ua: &str, aalice: bool) -> String {
    let cname = client_name(ua);
    let low = ua.to_lowercase();
    if aalice || low.contains("dart") {
        tagged(&cname, "Aaalice", "Aaalice_NAI_Launcher")
    } else if low.contains("dsh") {
        tagged(&cname, "DSH-Tavern", "DSH-Tavern-Image")
    } else if low.contains("luker") {
        tagged(&cname, "Luker", "Luker")
    } else if low.contains("tauri") {
        tagged(&cname, "Tauri", "TauriTavern")
    } else if low.contains("workbench") {
        tagged(&cname, "NAI-Workbench", "NAI-Workbench")
    } else if low.contains("littlewhitebox") || low.contains("xiaobaihe") {
        tagged(&cname, "LittleWhiteBox", "LittleWhiteBox")
    } else {
        cname
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_python() {
        assert_eq!(client_name(" a\u{1}\u{2}b "), "a b");
        assert_eq!(
            classify("Dart/3.5 (dart:io)", false),
            "Aaalice_NAI_Launcher (Dart/3.5 (dart:io))"
        );
        assert_eq!(
            classify("Mozilla/5.0", true),
            "Aaalice_NAI_Launcher (Mozilla/5.0)"
        );
        assert_eq!(
            classify("TauriTavern/1.0 tauri", false),
            "TauriTavern/1.0 tauri"
        );
        assert_eq!(classify("curl/8", false), "curl/8");
    }
}
