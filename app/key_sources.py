"""Key 来源网段统计：用于发现一把 Key 被多人分享。

只记录请求来源所在的网段（IPv4 /24、IPv6 /48）的加盐哈希和一个打码标签（如 120.235.*.*），
不保存完整 IP；7 天后自动删除。同一 Key 24 小时内出现的网段数达到阈值时私信提醒站长。
家庭 / 公司 / 手机流量 / 代理切换也会产生不同网段，所以这只是线索，不用于自动封禁。
"""
from __future__ import annotations

import hashlib
import ipaddress
import secrets
import time
from typing import Optional

WINDOW_SECONDS = 24 * 3600
RETENTION_SECONDS = 7 * 24 * 3600
WRITE_INTERVAL = 300          # 同一 Key + 网段 5 分钟内只写一次库
ALERT_COOLDOWN = 6 * 3600
SALT_SETTING = "key_source_salt"


def network_of(ip: str) -> Optional[tuple[str, str]]:
    """返回 (网段, 打码标签)；无法解析的地址（如测试客户端）返回 None。"""
    try:
        addr = ipaddress.ip_address((ip or "").strip())
    except ValueError:
        return None
    if isinstance(addr, ipaddress.IPv6Address) and addr.ipv4_mapped:
        addr = addr.ipv4_mapped
    if addr.version == 4:
        net = ipaddress.ip_network(f"{addr}/24", strict=False)
        a, b = str(addr).split(".")[:2]
        return str(net), f"{a}.{b}.*.*"
    net = ipaddress.ip_network(f"{addr}/48", strict=False)
    head = addr.exploded.split(":")[:2]
    return str(net), ":".join(head) + ":…"


class SourceTracker:
    def __init__(self, db, alerter=None, *, threshold: int = 3):
        self.db, self.alerter, self.threshold = db, alerter, max(0, int(threshold))
        self._salt: Optional[str] = None
        self._recent: dict[tuple[int, str], float] = {}

    async def _get_salt(self) -> str:
        if self._salt is None:
            salt = await self.db.get_setting(SALT_SETTING, None)
            if not salt:
                salt = secrets.token_hex(16)
                await self.db.set_setting(SALT_SETTING, salt)
            self._salt = str(salt)
        return self._salt

    async def observe(self, key, ip: str, now: Optional[float] = None) -> None:
        """记录一次成功鉴权的来源网段。管理员 Key 不记录。"""
        if key["is_admin"]:
            return
        found = network_of(ip)
        if found is None:
            return
        net, label = found
        now = time.time() if now is None else now
        digest = hashlib.sha256(((await self._get_salt()) + net).encode()).hexdigest()[:16]
        slot = (int(key["id"]), digest)
        if now - self._recent.get(slot, 0) < WRITE_INTERVAL:
            return
        self._recent[slot] = now
        if len(self._recent) > 4096:
            cutoff = now - WRITE_INTERVAL
            self._recent = {k: t for k, t in self._recent.items() if t >= cutoff}
        is_new = await self.db.touch_key_source(int(key["id"]), digest, label, now, now - WINDOW_SECONDS)
        if not is_new or not self.threshold or self.alerter is None:
            return
        labels = await self.db.key_source_labels(int(key["id"]), now - WINDOW_SECONDS)
        if len(labels) >= self.threshold:
            self.alerter.notify(
                f"key_sources_{key['id']}",
                f"Key「{key['name']}」24 小时内从 {len(labels)} 个不同网段使用（{'、'.join(labels[:6])}），"
                "可能被分享。也可能只是换了网络 / 代理，建议先问一下本人。",
                cooldown=ALERT_COOLDOWN)
