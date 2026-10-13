"""成员问卷（/feedback 页 + /member/survey 提交 + 后台 /admin/api/finance 查看）。

用虚拟 Key 识别身份（passive 鉴权：不算活跃、不进防分享统计）；一人一份，重复提交覆盖。
"""
import httpx
import pytest
import pytest_asyncio

from app import main
from app.config import Settings
from app.database import Database
from test_generation_integration import FakeState


@pytest_asyncio.fixture
async def env(monkeypatch):
    state = FakeState()
    state.settings = Settings(admin_password='fixture', secret_key='fixture-session',
                              admin_cookie_secure=False)
    state.db = Database(':memory:')
    await state.db.connect()
    monkeypatch.setattr(main, 'STATE', state)
    monkeypatch.setattr(main.app.state, 'gate', state, raising=False)
    key = await state.db.create_key(dict(name='成员', token='nai-member', daily_images=5,
                                         monthly_anlas=0, daily_text_tokens=0, rpm=5))

    async def _allow(_):
        return True
    state.hit_login = _allow
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app),
                                 base_url='http://fixture') as client:
        try:
            yield state, client, key
        finally:
            await state.db.close()


async def _post(client, token, body):
    return await client.post('/member/survey', json=body,
                             headers={'Authorization': 'Bearer ' + token})


BODY = dict(client='柏宝绘 1.2', understand='基本理解', improve=['稳定性', '报错提示更清楚'],
            improve_other='深夜偶尔 502', donate='愿意捐V4.5')


@pytest.mark.asyncio
async def test_submit_upsert_and_requires_key(env):
    state, client, key = env
    assert (await _post(client, 'nai-wrong', BODY)).status_code == 401          # Key 无效
    assert (await _post(client, 'nai-member', {**BODY, 'donate': ''})).status_code == 422   # 必选漏填
    ok = await _post(client, 'nai-member', BODY)
    assert ok.status_code == 200 and ok.json() == {'ok': True}
    rows = await state.db.list_survey()
    assert len(rows) == 1 and rows[0]['key_id'] == key['id']
    assert rows[0]['client'] == '柏宝绘 1.2' and '稳定性' in rows[0]['improve']
    updated = {**BODY, 'client': '换成 Aaalice 了', 'improve': ['排队和等待体验']}
    assert (await _post(client, 'nai-member', updated)).status_code == 200       # 覆盖而非新增
    rows = await state.db.list_survey()
    assert len(rows) == 1 and rows[0]['client'] == '换成 Aaalice 了'


@pytest.mark.asyncio
async def test_admin_view_needs_session_and_aggregates(env, monkeypatch):
    state, client, key = env
    assert (await client.get('/admin/api/survey')).status_code == 401          # 未登录
    assert (await client.post('/admin/api/login', json={'password': 'fixture'})).status_code == 200
    await _post(client, 'nai-member', BODY)
    data = (await client.get('/admin/api/survey')).json()
    assert data['total'] == 1
    assert data['summary']['donate'] == {'愿意捐V4.5': 1}
    assert data['summary']['improve']['稳定性'] == 1
    assert data['rows'][0]['name'] == '成员'
