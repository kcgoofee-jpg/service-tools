"""无效 Key / 后台登录的防爆破桶：IPv6 按 /64 聚合。

2026-10-13 审查：桶键曾用完整 IP，IPv6 用户在 /64 内轮换临时地址即可无限重置计数，
绕过无效 Key 拦截与后台登录限流继续爆破。
"""
import tempfile
import unittest
from pathlib import Path

from app.config import Settings
from app.state import GateState


class BruteForceBucketTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = GateState(Settings(data_dir=Path(self.tmp.name), nai_tokens=[]))
        self.state.settings.auth_fail_max = 2
        self.state.settings.login_max_attempts = 1

    async def asyncTearDown(self):
        self.tmp.cleanup()

    async def test_rotating_addresses_within_sl64_cannot_reset_the_counter(self):
        self.state.record_auth_failure("2001:db8:1:2::1")
        self.state.record_auth_failure("2001:db8:1:2:dead:beef::1")     # 同一 /64 的另一个临时地址
        self.assertGreater(self.state.auth_blocked("2001:db8:1:2::f"), 890)   # 第三个地址同样被拦
        self.assertEqual(self.state.auth_blocked("2001:db8:1:9::1"), 0)       # 别的 /64 不受影响

    async def test_ipv4_stays_exact_and_mapped_v4_folds_into_v4(self):
        self.state.record_auth_failure("10.1.2.3")
        self.assertEqual(self.state.auth_blocked("10.1.2.4"), 0)        # IPv4 不聚合
        self.state.record_auth_failure("::ffff:10.1.2.3")               # v4 映射地址归并到 v4 同桶
        self.assertGreater(self.state.auth_blocked("10.1.2.3"), 890)

    async def test_login_limiter_shares_the_sl64_bucket(self):
        self.assertTrue(await self.state.hit_login("2001:db8:1:2::1"))
        self.assertFalse(await self.state.hit_login("2001:db8:1:2::9"))  # 同 /64 直接顶到上限
        self.assertTrue(await self.state.hit_login("2001:db8:1:9::1"))

    def test_unparseable_values_fall_back_to_exact_match(self):
        self.assertEqual(self.state._bruteforce_bucket("not-an-ip"), "not-an-ip")
        self.assertEqual(self.state._bruteforce_bucket(""), "")
