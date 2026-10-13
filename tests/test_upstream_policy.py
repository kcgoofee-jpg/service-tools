"""upstream_policy：Retry-After 解析怪癖、403 双义判别、分类器，以及 nai.py 再导出不被断。"""
import time

from app import nai
from app.upstream_policy import (MODEL_403_MARKERS, Verdict, classify_upstream, clamp_retry_after,
                                 parse_retry_after)


def test_clamp_bounds_and_junk_default():
    assert clamp_retry_after("junk") == 20.0 and clamp_retry_after(None) == 20.0
    assert clamp_retry_after(float("inf")) == 3600.0 and clamp_retry_after(float("nan")) == 20.0
    assert clamp_retry_after(0) == 5.0 and clamp_retry_after(-3) == 5.0
    assert clamp_retry_after(10 ** 9) == 3600.0 and clamp_retry_after(30) == 30.0


def test_parse_retry_after_quirks_are_pinned():
    # 非纯数字一律 20 秒（"-5"、"1e9"、"inf" 都不走 float() 路径——行为与平移前逐字节一致）
    for junk in ("-5", "1e9", "tomorrow", "inf", "", None):
        assert parse_retry_after(junk) == 20.0
    assert parse_retry_after("7") == 7.0
    past = time.strftime("%a, %d %b %Y %H:%M:%S GMT", time.gmtime(time.time() - 3600))
    assert parse_retry_after(past) == 5.0                        # 过去的时间钳到下限，不是 None
    future = time.strftime("%a, %d %b %Y %H:%M:%S GMT", time.gmtime(time.time() + 120))
    assert 110 <= parse_retry_after(future) <= 121


def test_classify_upstream_kinds():
    v = classify_upstream(429, retry_after_header="30")
    assert v == Verdict("rate_limited", 30.0)
    marker_body = '{"statusCode":403,"message":"model \'nai-x\' doesn\'t exist"}'
    assert classify_upstream(403, peek=marker_body).kind == "forbidden_model"
    assert classify_upstream(403, peek='{"statusCode":403,"message":"blocked"}').kind == "forbidden_account"
    assert classify_upstream(403).kind == "forbidden_account"    # 没窥探到 body 按账号类处理
    assert classify_upstream(401).kind == "unauthorized"
    assert classify_upstream(500).kind == "other"
    assert classify_upstream(500).retry_after == 0.0


def test_nai_reexports_stay_intact():
    # state.py 与老测试从 app.nai 导入这些名字；改名/搬家必须保留别名
    assert nai.parse_retry_after is parse_retry_after
    assert nai.clamp_retry_after is clamp_retry_after
    assert nai.MODEL_403_MARKERS == MODEL_403_MARKERS
    assert nai._is_model_level_403("model 'x' does not exist") is True
    assert nai._is_model_level_403("hello") is False
