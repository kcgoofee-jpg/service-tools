"""全站重置 Key（/ops/reset-all-keys）：带「邀请」标签的 Key 不受影响。

2026-10-13 站长：重置是为了腾名额给新 Discord 成员，私邀朋友的 Key 要能继续用——
启用状态、Discord 绑定、全部标签都保留（名额按 discord_registrations 计数，绑定不能删）。
"""
import time

import httpx
import pytest
import pytest_asyncio

from app import main
from app.config import Settings
from app.database import Database, INVITE_TAG
from test_generation_integration import FakeState


def _key(name, token):
    return dict(name=name, token=token, daily_images=1, monthly_anlas=0,
                daily_text_tokens=0, rpm=5)


@pytest_asyncio.fixture
async def env(monkeypatch):
    state = FakeState()
    state.settings = Settings(admin_password='fixture', secret_key='fixture-session',
                              admin_cookie_secure=False)
    state.db = Database(':memory:')
    await state.db.connect()

    async def login_allowed(_):
        return True
    state.hit_login = login_allowed
    monkeypatch.setattr(main, 'STATE', state)
    monkeypatch.setattr(main.app.state, 'gate', state, raising=False)

    db = state.db
    admin = await db.create_key(_key('站长', 'nai-admin'))
    await db.set_key_role(admin['id'], 'admin')
    member = await db.create_key(_key('普通成员', 'nai-member'))
    friend = await db.create_key(_key('邀请-朋友', 'nai-friend'))
    friend2 = await db.create_key(_key('邀请-带备注', 'nai-friend2'))
    for key_id, discord_id in ((member['id'], '222'), (friend['id'], '111'), (friend2['id'], '333')):
        await db._db.execute("INSERT INTO discord_registrations(discord_id, key_id, created_at) VALUES (?,?,?)",
                             (discord_id, key_id, time.time()))
    await db._db.execute("INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)",
                         (friend['id'], INVITE_TAG, '', '站长', time.time()))
    await db._db.execute("INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)",
                         (friend2['id'], INVITE_TAG, '', '站长', time.time()))
    await db._db.execute("INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)",
                         (friend2['id'], '合作', '', '站长', time.time()))        # 邀请 Key 的其他标签也要留
    await db._db.execute("INSERT INTO key_tags(key_id, tag, note, by, ts) VALUES (?,?,?,?,?)",
                         (member['id'], '旧标记', '', '系统', time.time()))
    await db._db.execute("INSERT INTO share_state(key_id, score, strikes) VALUES (?,?,?)",
                         (friend['id'], 3.0, 1))
    await db._db.commit()

    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app),
                                 base_url='http://fixture') as client:
        try:
            yield state, client, dict(admin=admin['id'], member=member['id'],
                                      friend=friend['id'], friend2=friend2['id'])
        finally:
            await state.db.close()


async def login(client):
    assert (await client.post('/admin/api/login', json={'password': 'fixture'})).status_code == 200


@pytest.mark.asyncio
async def test_wrong_confirm_is_rejected(env):
    state, client, ids = env
    await login(client)
    r = await client.post('/admin/api/ops/reset-all-keys', json={'confirm': '重置'})
    assert r.status_code == 400
    assert (await state.db.get_key(ids['member']))['enabled']            # 什么都没动


@pytest.mark.asyncio
async def test_reset_disables_members_and_keeps_invite_keys_intact(env):
    state, client, ids = env
    await login(client)
    body = (await client.post('/admin/api/ops/reset-all-keys', json={'confirm': '确认重置'})).json()
    assert body['disabled_count'] == 1 and body['kept_invites'] == 2
    db = state.db
    assert not (await db.get_key(ids['member']))['enabled']              # 普通成员被失效
    assert (await db.get_key(ids['admin']))['enabled']                   # 站长 Key 不动
    for kept in ('friend', 'friend2'):
        assert (await db.get_key(ids[kept]))['enabled']                  # 邀请 Key 原样保留
    bound = {r['discord_id'] for r in await db._db.execute_fetchall("SELECT discord_id FROM discord_registrations")}
    assert bound == {'111', '333'}                                       # 只有邀请 Key 的绑定还在（名额按这个数）
    tags_by_key: dict[int, set] = {}
    for r in await db._db.execute_fetchall("SELECT key_id, tag FROM key_tags"):
        tags_by_key.setdefault(r['key_id'], set()).add(r['tag'])
    assert INVITE_TAG in tags_by_key.get(ids['friend'], set())
    assert {INVITE_TAG, '合作'} <= tags_by_key.get(ids['friend2'], set())    # 邀请 Key 的全部标签保留
    assert ids['member'] not in tags_by_key                                  # 普通成员的标签被清
    assert not await db._db.execute_fetchall("SELECT 1 FROM share_state")  # 防分享状态照旧清空
