"""Key 暂停闸门：自动驾驶等模块对单把 Key 的临时暂停（如死循环重试限流）。

原防分享的证据/评分/自动处罚已整体移除（2026-10-13 站长拍板）：站规明确允许挂梯子换节点
与多设备，网段/设备指纹恰好都是被允许的行为，observe 模式下只有噪音没有执法价值
（最后一批 10 个标记里 4 个被站长判定误判）。本模块只保留「暂停闸门」本身：
paused_until + 给成员看的原因，状态在库里，重启不丢。
"""
from __future__ import annotations

import time
from typing import Optional


class ShareGuard:
    def __init__(self, db):
        self.db = db
        self.paused: dict[int, float] = {}
        self.pause_reasons: dict[int, str] = {}    # key_id → 给成员看的暂停原因

    async def load(self) -> None:
        rows = await self.db._db.execute_fetchall(
            "SELECT key_id, paused_until, pause_reason FROM share_state WHERE paused_until > ?", (time.time(),))
        self.paused = {int(r[0]): float(r[1]) for r in rows}
        self.pause_reasons = {int(r[0]): r[2] for r in rows if r[2]}

    def paused_until(self, key_id: int, now: Optional[float] = None) -> float:
        until = self.paused.get(int(key_id), 0.0)
        return until if until > (time.time() if now is None else now) else 0.0

    async def pause_key(self, key_id: int, seconds: float, reason: str, now: Optional[float] = None) -> bool:
        """非防分享的 Key 暂停（如自动驾驶对死循环重试限流）。已在暂停中则不重复，返回是否新暂停。"""
        now = time.time() if now is None else now
        kid = int(key_id)
        if self.paused.get(kid, 0.0) > now:
            return False
        until = now + max(0.0, seconds)
        await self.db._db.execute(
            "INSERT INTO share_state(key_id, paused_until, pause_reason) VALUES (?,?,?) "
            "ON CONFLICT(key_id) DO UPDATE SET paused_until=excluded.paused_until, pause_reason=excluded.pause_reason",
            (kid, until, reason))
        await self.db._db.commit()
        self.paused[kid] = until
        self.pause_reasons[kid] = reason
        return True

    async def clear(self, key_id: int) -> None:
        """站长解除暂停（比如判定自动驾驶限流不当）。"""
        kid = int(key_id)
        self.paused.pop(kid, None)
        self.pause_reasons.pop(kid, None)
        await self.db._db.execute("DELETE FROM share_state WHERE key_id=?", (kid,))
        await self.db._db.commit()
