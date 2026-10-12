"""文本通道的 8192 token 上下文预算（2026-10-13 站长踩到的坑）。

现场：站长用小白盒插件的 Agent 指向我们，`llama-3-erato-v1` 与 `kayra-v1` 都返回 502，
日志是「上游文本请求失败（HTTP 400）」。直接打上游才看到真话：

    {"statusCode":400,"message":"Invalid request: Requested length exceeds
     model's maximum context size, 8192 tokens at 1215..."}

即 NovelAI 文本模型是「输入 + 输出 ≤ 8192 token」。我们原来只按**字符**限流
（max_input_chars=24000），中文差不多 1 字 1 token，插件的 Agent 又带一大坨系统提示词/世界书，
必然超。修法是按 token 预算做「中间省略」截断：保留开头（人设/规则）与结尾（最近对话）。
"""
from app.policy import (TEXT_CONTEXT_TOKENS, TEXT_MAX_GENERATION_TOKENS, estimate_text_tokens,
                        clamp_text_params, _truncate_middle)


def _body(text, max_length=300):
    return {"input": text, "model": "kayra-v1",
            "parameters": {"max_length": max_length, "min_length": 1,
                           "temperature": 0.9, "top_p": 0.9, "top_k": 40, "use_string": True}}


def test_estimate_counts_cjk_dearer_than_ascii():
    assert estimate_text_tokens("你好世界") > estimate_text_tokens("abcd")
    # 中日韩按 1 token/字（估高不估低），ASCII 按 0.34
    assert estimate_text_tokens("中" * 100) >= 100
    assert estimate_text_tokens("a" * 100) < 60


def test_short_input_is_left_alone():
    body, notes, problem = clamp_text_params(_body("你好"), max_output_tokens=300, max_input_chars=24000)
    assert problem is None and body["input"] == "你好"
    assert not any("省略" in n for n in notes)


def test_long_input_is_truncated_to_fit_the_context():
    long_text = "这是一段很长的设定与对话。" * 1500          # 19500 字符 ≈ 19500 token
    assert estimate_text_tokens(long_text) > TEXT_CONTEXT_TOKENS
    body, notes, problem = clamp_text_params(_body(long_text), max_output_tokens=300,
                                             max_input_chars=24000)
    assert problem is None                                    # 不再丢给上游换 400
    assert len(body["input"]) < len(long_text)
    assert estimate_text_tokens(body["input"]) + body["parameters"]["max_length"] <= TEXT_CONTEXT_TOKENS
    assert any("8192" in n and "省略" in n for n in notes)


def test_truncation_keeps_head_and_tail():
    text = "开头很重要" + "中间" * 5000 + "结尾也很重要"
    out, _cut = _truncate_middle(text, 500)
    assert out.startswith("开头很重要") and out.endswith("结尾也很重要")
    assert "中间省略" in out


def test_char_cap_still_rejects_absurd_bodies():
    body, _notes, problem = clamp_text_params(_body("字" * 30000), max_output_tokens=300,
                                              max_input_chars=24000)
    assert problem is not None and "输入过长" in problem


def test_generation_length_is_capped_at_upstream_limit():
    """上游单次生成上限 250 token：以前 max_text_output_tokens=300 让**每一次**文本请求都 400。"""
    body, notes, problem = clamp_text_params(_body("你好", max_length=300), max_output_tokens=300,
                                            max_input_chars=24000)
    assert problem is None
    assert body["parameters"]["max_length"] == TEXT_MAX_GENERATION_TOKENS == 250
    assert any("250" in n for n in notes)

    # 插件把 max_tokens 设成 32000 也一样被钳住
    body, _, _ = clamp_text_params(_body("你好", max_length=32000), max_output_tokens=300,
                                   max_input_chars=24000)
    assert body["parameters"]["max_length"] == 250

    # 站长把上限写到环境变量里也兜得住
    body, _, _ = clamp_text_params(_body("你好", max_length=2000), max_output_tokens=2000,
                                   max_input_chars=24000)
    assert body["parameters"]["max_length"] == 250


def test_budget_leaves_room_for_upstream_generation_limit():
    """输入预算 + 生成上限必须留在 8192 上下文里。"""
    text = "设定" * 4000
    body, _, _ = clamp_text_params(_body(text, max_length=250), max_output_tokens=300,
                                   max_input_chars=24000)
    assert estimate_text_tokens(body["input"]) + body["parameters"]["max_length"] <= TEXT_CONTEXT_TOKENS
