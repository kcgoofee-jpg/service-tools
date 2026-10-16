//! V5 Medium 降档开关的纯逻辑：三档模式、自动档的滞回判定。
//!
//! 设置 `v5_medium_mode`（登记在 `site_flags::V5_MEDIUM_MODE`）：
//! - `off`：不动（默认）；
//! - `on`：V5 Full（含局部重绘）一律改走 Medium，在费用估算和额度准入之前改；
//! - `auto`：可用上游账号里最低的 V5 剩余百分比低于「V5 余量低提示线」时才改走 Medium。
//!
//! 自动档带滞回：低于提示线才打开，回到「提示线 + [`HYSTERESIS`]」以上才关掉，避免余量在线附近抖动时
//! 同一批成员一会儿 Full 一会儿 Medium。服务层每次决策都用缓存快照重算，当前状态只存在内存里（重启后从关开始）。

use crate::site_flags;
use serde_json::Value;

/// 关掉自动降档要比提示线再高多少个百分点。
pub const HYSTERESIS: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    On,
    Auto,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::On => "on",
            Mode::Auto => "auto",
        }
    }

    /// `site_flags` 解析出来的值 → 模式；认不出的一律 `Off`（fail-closed）。
    pub fn from_flag(v: &Value) -> Self {
        match v.as_str() {
            Some("on") => Mode::On,
            Some("auto") => Mode::Auto,
            _ => Mode::Off,
        }
    }

    /// 库里的原始文本 → 模式。
    pub fn from_raw(raw: Option<&str>) -> Self {
        Self::from_flag(&site_flags::get_from_raw(
            &site_flags::V5_MEDIUM_MODE,
            raw,
            None,
        ))
    }
}

/// 可用账号里最低的 V5 剩余百分比；没有任何账号读到过百分比（还没查过）返回 None。
pub fn lowest_percent(percents: impl IntoIterator<Item = Option<i64>>) -> Option<i64> {
    percents.into_iter().flatten().min()
}

/// 自动档的下一个状态。`was_on` 是上一次决策的结果，`lowest` 是 [`lowest_percent`]。
/// 读数未知（None）时不降档：宁可照常出 Full，也不凭空猜余量。
pub fn auto_decide(was_on: bool, lowest: Option<i64>, threshold: i64) -> bool {
    match lowest {
        None => false,
        Some(p) if was_on => p < threshold + HYSTERESIS,
        Some(p) => p < threshold,
    }
}

/// 一次决策的结果（后台展示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub mode: Mode,
    /// 此刻是否改走 Medium
    pub active: bool,
    pub lowest_percent: Option<i64>,
    pub threshold: i64,
}

/// 按模式与读数算出此刻是否降档；`was_on` 只对 `auto` 有意义。
pub fn decide(mode: Mode, was_on: bool, lowest: Option<i64>, threshold: i64) -> Decision {
    let active = match mode {
        Mode::Off => false,
        Mode::On => true,
        Mode::Auto => auto_decide(was_on, lowest, threshold),
    };
    Decision {
        mode,
        active,
        lowest_percent: lowest,
        threshold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mode_parsing_fails_closed() {
        assert_eq!(Mode::from_raw(None), Mode::Off);
        assert_eq!(Mode::from_raw(Some("on")), Mode::On);
        assert_eq!(Mode::from_raw(Some("auto")), Mode::Auto);
        for bad in ["", "ON", "yes", "1", "medium", "auto2"] {
            assert_eq!(Mode::from_raw(Some(bad)), Mode::Off, "{bad}");
        }
        assert_eq!(Mode::from_flag(&json!(true)), Mode::Off);
        assert_eq!(Mode::from_flag(&Value::Null), Mode::Off);
    }

    #[test]
    fn only_full_and_its_inpainting_map_to_medium() {
        use crate::policy::v5_medium_of;
        assert_eq!(
            v5_medium_of("nai-diffusion-5-full"),
            Some("nai-diffusion-5-full-medium")
        );
        assert_eq!(
            v5_medium_of(" NAI-Diffusion-5-Full-Inpainting "),
            Some("nai-diffusion-5-full-medium-inpainting")
        );
        for other in [
            "nai-diffusion-5-full-medium",
            "nai-diffusion-5-full-medium-inpainting",
            "nai-diffusion-5-curated",
            "nai-diffusion-5",
            "nai-v5",
            "nai-diffusion-4-5-full",
            "",
        ] {
            assert_eq!(v5_medium_of(other), None, "{other}");
        }
    }

    #[test]
    fn lowest_ignores_unknown() {
        assert_eq!(
            lowest_percent([Some(40), None, Some(12), Some(70)]),
            Some(12)
        );
        assert_eq!(lowest_percent([None, None]), None);
        assert_eq!(lowest_percent([]), None);
    }

    #[test]
    fn auto_switches_on_below_threshold_only() {
        // 提示线 20：19 开，20 不开
        assert!(auto_decide(false, Some(19), 20));
        assert!(!auto_decide(false, Some(20), 20));
        assert!(!auto_decide(false, Some(21), 20));
        assert!(!auto_decide(false, Some(24), 20));
    }

    #[test]
    fn auto_has_hysteresis_of_five() {
        // 已经开着：< 25 一直开，>= 25 才关
        assert!(auto_decide(true, Some(19), 20));
        assert!(auto_decide(true, Some(20), 20));
        assert!(auto_decide(true, Some(24), 20));
        assert!(!auto_decide(true, Some(25), 20));
        assert!(!auto_decide(true, Some(80), 20));
    }

    #[test]
    fn auto_walks_through_a_recovery_without_flapping() {
        let mut on = false;
        let mut seen = Vec::new();
        for p in [30, 21, 19, 22, 24, 26, 23, 18] {
            on = auto_decide(on, Some(p), 20);
            seen.push(on);
        }
        assert_eq!(seen, [false, false, true, true, true, false, false, true]);
    }

    #[test]
    fn auto_unknown_percent_never_switches() {
        assert!(!auto_decide(false, None, 20));
        // 读数丢了（理论上缓存不会丢）：保守回到不降档
        assert!(!auto_decide(true, None, 20));
    }

    #[test]
    fn auto_with_extreme_thresholds() {
        // 提示线 100：余量 < 100 就开，开了之后要到 105 才关，实际上不会再关
        assert!(auto_decide(false, Some(99), 100));
        assert!(auto_decide(true, Some(100), 100));
        // 提示线 1：只有 0% 才开
        assert!(auto_decide(false, Some(0), 1));
        assert!(!auto_decide(false, Some(1), 1));
    }

    #[test]
    fn decide_by_mode() {
        assert!(!decide(Mode::Off, false, Some(0), 20).active);
        assert!(decide(Mode::On, false, None, 20).active);
        let d = decide(Mode::Auto, false, Some(10), 20);
        assert!(d.active && d.lowest_percent == Some(10) && d.threshold == 20);
    }
}
