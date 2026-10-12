"""文本通道的 8192 token 上下文预算（2026-10-13 站长踩到的坑）。

现场：站长用小白盒插件的 Agent 指向我们，`llama-3-erato-v1` 与 `kayra-v1` 都返回 502，
日志是「上游文本请求失败（HTTP 400）」。直接打上游才看到真话：

    {"statusCode":400,"message":"Invalid request: Requested length exceeds
     model's maximum context size, 8192 tokens at 1215..."}

即 NovelAI 文本模型是「输入 + 输出 ≤ 8192 token」。我们原来只按**字符**限流
（max_input_chars=24000），中文差不多 1 字 1 token，插件的 Agent 又带一大坨系统提示词/世界书，
必然超。修法是按 token 预算做「中间省略」截断：保留开头（人设/规则）与结尾（最近对话）。
"""
from app.policy import (TEXT_CONTEXT_TOKENS, estimate_text_tokens, clamp_text_params,
                        _truncate_middle)


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


def test_budget_shrinks_when_max_length_is_large():
    """max_length 越大，留给输入的预算越小（两者相加不能超过 8192）。"""
    text = "设定" * 4000
    small, _, _ = clamp_text_params(_body(text, max_length=64), max_output_tokens=8192,
                                    max_input_chars=24000)
    big, _, _ = clamp_text_params(_body(text, max_length=4096), max_output_tokens=8192,
                                  max_input_chars=24000)
    assert len(big["input"]) < len(small["input"])
    assert estimate_text_tokens(big["input"]) + 4096 <= TEXT_CONTEXT_TOKENS
