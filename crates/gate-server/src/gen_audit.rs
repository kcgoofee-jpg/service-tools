//! 生成记录（旧版 `main.audit_generation`）：按后台开关记录提示词与原图。任何失败都不得影响生图结果。

use crate::key::Key;
use crate::reqctx::ReqCtx;
use crate::state::AppState;
use gate_db::repo::audit::NewAudit;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    state: &AppState,
    ctx: &ReqCtx,
    key: &Key,
    kind: &str,
    model: &str,
    status: &str,
    body: &serde_json::Value,
    content: Option<bytes::Bytes>,
) {
    let Ok((want_prompts, want_images, _days)) =
        crate::audit::audit_flags(&state.db, &state.env).await
    else {
        return;
    };
    if !(want_prompts || want_images) {
        return;
    }
    let prompts = if want_prompts {
        match crate::audit::capture_prompts(body) {
            Ok(p) => p,
            Err(e) => {
                state.bug("audit", Some((e.type_name(), e.message())), ctx);
                return;
            }
        }
    } else {
        Default::default()
    };
    // 「记录生成结果」开关就是存原图的开关；只存成功的图
    let (image, image_type) =
        match content.filter(|c| want_images && !c.is_empty() && status == "ok") {
            Some(c) => tokio::task::spawn_blocking(move || crate::audit::full_image(&c))
                .await
                .unwrap_or((None, String::new())),
            None => (None, String::new()),
        };
    let row = NewAudit {
        key_id: Some(key.id()),
        key_name: key.name(),
        kind,
        model,
        status,
        prompt: &prompts.0,
        negative: &prompts.1,
        thumb: None,
        extra: &prompts.2,
        image: image.as_deref(),
        image_type: &image_type,
    };
    if let Err(e) = state.db.add_audit(&row, state.now(), &state.clock).await {
        state.bug("audit", Some(("DbError", &e.to_string())), ctx);
    }
}
