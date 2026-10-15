//! 请求耗时的纯逻辑（`app/request_timing.py`）。
//!
//! 不移植：用 `ContextVar` 存的每请求状态（`begin` / `end` / `mark_*` / `rid` 等）和 `secrets.token_hex(4)`
//! 生成请求编号——Rust 侧由请求上下文自己持有。这里只放判定与格式化：
//! - [`snapshot_ms`]：`wait_ms` / `dur_ms` 的换算；[`model_label`]：被拒请求也记下的模型名。
//!
//! 旧版还有按 User-Agent / 请求头 / 请求体指纹识别客户端的那几个函数（`client_name`、`classify_client`、
//! `refine_client_from_payload`），3.0 整块删除，见 docs/v3-deviations.md #50。

use crate::pyc;

/// `set_model` 保留的模型名长度。
pub const MODEL_MAX: usize = 80;

/// `(model or "")[:80]`
pub fn model_label(model: &str) -> String {
    pyc::take_chars(model, MODEL_MAX).to_string()
}

/// `snapshot()` 里的耗时换算（单调时钟，秒）：返回 `(wait_ms, dur_ms)`。
/// `sent` 为 `None`（没发到上游）：等待 = `now - t0`，上游耗时 0。
pub fn snapshot_ms(t0: f64, sent: Option<f64>, now: f64) -> (i64, i64) {
    let (wait, dur) = match sent {
        None => (now - t0, 0.0),
        Some(s) => (s - t0, now - s),
    };
    ((wait * 1000.0) as i64, (dur * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing() {
        assert_eq!(snapshot_ms(1.0, None, 3.5), (2500, 0));
        assert_eq!(snapshot_ms(1.0, Some(2.0), 5.0), (1000, 3000));
    }

    #[test]
    fn model_label_truncates() {
        assert_eq!(
            model_label("nai-diffusion-4-5-full"),
            "nai-diffusion-4-5-full"
        );
        assert_eq!(model_label(&"m".repeat(100)).chars().count(), MODEL_MAX);
    }
}
