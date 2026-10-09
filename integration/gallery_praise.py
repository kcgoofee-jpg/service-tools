"""跑图分享自动评论：奶妹用 Claude Haiku 5.5 看图，写一段热情的夸奖（Markdown + 表情）。

只有服务器 .env 里配置了 ANTHROPIC_API_KEY 才启用；没配置时只点赞不评论。
每天最多 GALLERY_AI_DAILY 条（默认 30），防止被刷帖花超。
模型拒绝或出错时不发评论（只记日志），不会发出奇怪的内容。
"""
from __future__ import annotations

import os
import time
from typing import Optional

MODEL = "claude-haiku-5-5"
DAILY_LIMIT = int(os.getenv("GALLERY_AI_DAILY", "30") or 30)
IMAGE_TYPES = ("image/png", "image/jpeg", "image/webp", "image/gif")

SYSTEM = """你是「猫头鹰公益站」Discord 社区的看板娘「奶妹」，负责在「跑图分享」频道给成员发的 AI 绘图作品写评论。

写法：
- 先认真看图：说清楚画面里有什么（人物外貌、动作、表情、服装、背景、构图、色彩、画风），夸得具体，不要空泛。
- 再结合作者写的标题和文字回应他们的心情或梗。
- 语气热情、真诚、可爱，像很喜欢这张图的同好；称呼作者为「老师」。不要猜测作者的性别。
- 用 Markdown：一个二级标题开头，可以用引用块、粗体、列表；穿插合适的表情符号。
- 长度 250～450 字，结尾鼓励继续创作，可以邀请分享提示词思路。
- 不要编造你看不到的细节，不要说图是谁画的或出自哪部作品，除非作者自己说了。
- 如果画面含有露骨的性内容（裸露的性器官、性行为等），不要写评论，只输出四个字符：[NSFW]
- 只输出评论正文，不要任何解释。"""

# 露骨图片：不夸，奶妹捂眼睛跑开（模型拒绝时也用这句）
SHY = "🙈💨 呜哇——奶妹捂住眼睛跑开啦！\n这张对奶妹来说太刺激了，不敢看不敢看～ (〃▽〃)ﾉ 老师继续加油哦！\n\n——🦉 奶妹"

_used: dict[str, int] = {}


def _today() -> str:
    return time.strftime("%Y-%m-%d")


def enabled() -> bool:
    return bool(os.getenv("ANTHROPIC_API_KEY"))


async def write_praise(title: str, text: str, image_urls: list[str]) -> Optional[str]:
    """返回评论正文；没有可用图片、超出每日上限、模型拒绝或出错时返回 None。"""
    if not enabled() or not image_urls:
        return None
    day = _today()
    if _used.get(day, 0) >= DAILY_LIMIT:
        print(f"[gallery] daily AI comment limit {DAILY_LIMIT} reached", flush=True)
        return None
    import anthropic                          # 只有启用时才需要这个依赖
    client = anthropic.AsyncAnthropic()       # 从环境变量 ANTHROPIC_API_KEY 读取
    content: list[dict] = [{"type": "image", "source": {"type": "url", "url": u}} for u in image_urls[:3]]
    content.append({"type": "text", "text": f"帖子标题：{title or '（无）'}\n作者的话：{text or '（无）'}\n\n请为这个帖子写评论。"})
    try:
        response = await client.messages.create(
            model=MODEL,
            max_tokens=2000,
            output_config={"effort": "low"},
            system=SYSTEM,
            messages=[{"role": "user", "content": content}],
        )
    except anthropic.RateLimitError:
        print("[gallery] AI comment rate limited", flush=True)
        return None
    except anthropic.APIStatusError as exc:
        print(f"[bug] gallery AI comment failed: {exc.status_code} {str(exc.message)[:200]}", flush=True)
        return None
    except anthropic.APIConnectionError:
        print("[bug] gallery AI comment: network error", flush=True)
        return None
    if response.stop_reason == "refusal":
        print("[gallery] AI comment refused by model; shy reply", flush=True)
        return SHY
    body = "".join(b.text for b in response.content if b.type == "text").strip()
    if not body:
        return None
    if body.startswith("[NSFW]"):
        print("[gallery] explicit image; shy reply", flush=True)
        return SHY
    _used[day] = _used.get(day, 0) + 1
    u = response.usage
    print(f"[gallery] AI comment ok in={u.input_tokens} out={u.output_tokens} today={_used[day]}", flush=True)
    return body[:1900] + "\n\n——🦉 奶妹"
