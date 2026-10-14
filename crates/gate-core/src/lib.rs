//! 猫头鹰公益站的纯领域逻辑。
//!
//! 这个 crate 不做 I/O、不依赖 tokio：所有函数都是「输入 → 输出」，
//! 用 `tests/golden/` 里从旧 Python 生成的夹具逐条对拍（浮点逐位相等）。
//! 时间一律由调用方传入（Unix 秒，f64），不在这里读时钟。

pub mod clock;
pub mod pyc;
pub mod pyjson;
pub mod pyval;
