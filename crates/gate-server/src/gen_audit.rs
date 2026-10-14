//! 生成记录（旧版 `main.audit_generation`）：按后台开关记录提示词与原图。依赖 `crate::audit`。

use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::AppState;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    _state: &AppState,
    _ctx: &ReqCtx,
    _key: &Key,
    _kind: &str,
    _model: &str,
    _status: &str,
    _body: &serde_json::Value,
    _content: Option<bytes::Bytes>,
) {
}
