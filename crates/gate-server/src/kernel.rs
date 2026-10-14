//! 自适应内核（旧版 `app/kernel.py` / `modules.py` / `quota_algo.run` / `autopilot.run`）的运行层。
//! 纯逻辑在 gate-core；这里接库、时钟和后台任务。完整实现由内核移植任务补上。

use crate::error::{ApiError, InternalError};
use crate::state::AppState;
use serde_json::Value;

/// 动态额度重算（旧版 `quota_algo.run(state)`），返回写进 `quota_algo_last` 的结果。
pub async fn run_quota_algo(state: &AppState) -> Result<Value, ApiError> {
    let _ = state;
    Err(InternalError("动态额度内核尚未接入".into()).into())
}
