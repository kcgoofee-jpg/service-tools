"""局部重绘 / 图生图的两条上游地雷（2026-10-13 站长实测：局部重绘失败、原因不明）。

逐组合打真实上游得到的事实：

    action=infill 不带 parameters.mask        → 500 Internal Server Error
    action=infill 或 img2img 带 sm/sm_dyn     → 500 Internal Server Error
    其余 10 种组合（带 mask、V4 / V4.5 / V5 / V5 Medium、strength 0.7/1.0、
    add_original_image、controlnet_strength、noise、extra_noise_seed……）→ 200

上游对这两种情况都不给可读的错误，成员只会看到「生成失败」。所以在网关层挡：
缺蒙版 → 400 说清楚；SMEA → 按官方前端置 false（官方前端在 img2img / 局部重绘一律发 false）。
"""
from app.policy import guard_image_action


def _body(action="infill", mask="b64mask", **params):
    p = {"width": 128, "height": 128, "steps": 28, "n_samples": 1, "image": "b64img"}
    p.update(params)
    if mask is not None:
        p["mask"] = mask
    return {"input": "1girl", "model": "nai-diffusion-5-full-inpainting",
            "action": action, "parameters": p}


def test_infill_without_mask_is_rejected_with_a_readable_reason():
    out, notes, problem = guard_image_action(_body(mask=None))
    assert problem and "mask" in problem and "500" in problem


def test_infill_with_empty_mask_is_rejected_too():
    _, _, problem = guard_image_action(_body(mask=""))
    assert problem


def test_infill_with_mask_passes():
    out, notes, problem = guard_image_action(_body())
    assert problem is None and not notes
    assert out["parameters"]["mask"] == "b64mask"


def test_smea_is_reset_for_infill_and_img2img():
    for action in ("infill", "img2img"):
        out, notes, problem = guard_image_action(_body(action=action, sm=True))
        assert problem is None
        assert out["parameters"]["sm"] is False and out["parameters"]["sm_dyn"] is False
        assert any("SMEA" in n for n in notes)


def test_smea_dyn_alone_is_also_reset():
    out, notes, _ = guard_image_action(_body(sm_dyn=True))
    assert out["parameters"]["sm_dyn"] is False and notes


def test_plain_generation_keeps_smea():
    """action=generate 时 SMEA 是合法参数：不能动它（动了对不上原图）。"""
    body = {"input": "1girl", "model": "nai-diffusion-4-5-full", "action": "generate",
            "parameters": {"sm": True, "sm_dyn": True, "width": 832, "height": 1216}}
    out, notes, problem = guard_image_action(body)
    assert problem is None and not notes
    assert out["parameters"]["sm"] is True and out["parameters"]["sm_dyn"] is True


def test_missing_action_is_untouched():
    body = {"input": "1girl", "model": "nai-diffusion-4-5-full", "parameters": {"sm": True}}
    out, notes, problem = guard_image_action(body)
    assert problem is None and not notes and out["parameters"]["sm"] is True


def test_original_body_is_not_mutated_in_place():
    body = _body(sm=True)
    guard_image_action(body)
    assert body["parameters"]["sm"] is True          # 调用方拿到的入参不该被改
