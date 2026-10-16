/**
 * 成员首页（/）。
 * 登录成员：额度卡在第一屏，Key / 重置券在下面；游客：登录与加入入口 + 三步上手 + 客户端设置 + 使用须知。
 * 所有人都有：实时状态卡、站长公告（有的话）、捐赠。
 */
import { useEffect, useState } from "preact/hooks";
import { Skeleton, toast } from "../../ui";
import { Announcement } from "./Announcement";
import { StoreCtx } from "./ctx";
import { Donate } from "./Donate";
import { GuestGuides, GuestHero, Steps } from "./Guest";
import { displayName } from "./helpers";
import { Coupons, KeyCard, NoKeyCard, QuotaCard } from "./Member";
import { StatusCard } from "./StatusCard";
import { createStore, type Store } from "./store";
import { SiteFooter, Topbar } from "./Topbar";
import type { MeMember } from "./types";
import "./landing.css";

/** 验证身份组（/login?verify=1）回到首页时带的结果。 */
const VERIFY_NOTICE: Record<string, { text: string; error: boolean }> = {
  ok: { text: "身份组验证通过，现在可以回 Discord 用 /register 领取 Key 了。", error: false },
  no: {
    text: "没有检测到所需的身份组（或你不在对应的服务器），暂时不能领取 Key。",
    error: true,
  },
  error: { text: "Discord 暂时无法核验，请稍后再试。", error: true },
};

/** /login/callback 回到首页时带的结果（?login=failed 或 ?verify=ok|no|error）：提示一下并把查询参数从地址栏去掉。 */
function useLoginNotice() {
  useEffect(() => {
    const params = new URLSearchParams(location.search);
    const v = params.get("login");
    const verify = params.get("verify");
    if (!v && !verify) return;
    if (v === "failed") {
      toast(
        "Discord 登录没有成功。请确认你已加入猫头鹰公益站的 Discord 服务器，然后再点一次「用 Discord 登录」。",
        {
          error: true,
          ms: 9000,
        },
      );
    }
    const n = verify ? VERIFY_NOTICE[verify] : undefined;
    if (n) toast(n.text, { error: n.error, ms: 9000 });
    params.delete("login");
    params.delete("verify");
    const qs = params.toString();
    history.replaceState(null, "", `${location.pathname}${qs ? `?${qs}` : ""}${location.hash}`);
  }, []);
}

/** 问卷页让未登录的人先登录：登录回调只会回到首页，这里把人送回去。 */
function useReturnToSurvey(loggedIn: boolean) {
  useEffect(() => {
    if (!loggedIn) return;
    try {
      if (sessionStorage.getItem("owl-next") === "/survey") {
        sessionStorage.removeItem("owl-next");
        location.replace("/survey");
      }
    } catch {
      /* 隐私模式：不跳转 */
    }
  }, [loggedIn]);
}

function Loading() {
  return (
    <div class="lp-grid" aria-busy="true">
      <span class="sr-only">正在读取…</span>
      <Skeleton h="220px" />
      <Skeleton h="220px" />
    </div>
  );
}

function MemberView({ me }: { me: MeMember }) {
  return (
    <div class="lp-grid">
      <div class="lp-col">
        <div class="lp-o1">{me.has_key ? <QuotaCard me={me} /> : <NoKeyCard me={me} />}</div>
        {me.has_key && (
          <div class="lp-o3">
            <KeyCard me={me} />
          </div>
        )}
        {me.has_key && me.coupons && me.coupons.length > 0 && (
          <div class="lp-o4">
            <Coupons coupons={me.coupons} />
          </div>
        )}
      </div>
      <div class="lp-col">
        <div class="lp-o2">
          <StatusCard />
        </div>
      </div>
    </div>
  );
}

function Page({ store }: { store: Store }) {
  const me = store.me.value;
  const member = me?.logged_in ? me : null;
  const webLogin = store.meta.value.web_login === true;
  useLoginNotice();
  useReturnToSurvey(!!member);
  return (
    <>
      <Topbar
        user={
          member
            ? { name: displayName(member), id: member.discord?.id, avatar: member.discord?.avatar }
            : undefined
        }
        loginOpen={webLogin && !member}
      />
      <main class="page lp-main">
        {me === undefined ? (
          <Loading />
        ) : member ? (
          <>
            <h1 class="sr-only">猫头鹰公益站 · 我的额度</h1>
            <MemberView me={member} />
            <Announcement />
            <Donate />
          </>
        ) : (
          <>
            <GuestHero />
            <Announcement />
            <Donate />
            <Steps />
            <GuestGuides />
          </>
        )}
      </main>
      <SiteFooter survey={!!member?.has_key} />
    </>
  );
}

export function Landing() {
  const [store] = useState(createStore);
  useEffect(() => store.start(), [store]);
  return (
    <StoreCtx.Provider value={store}>
      <Page store={store} />
    </StoreCtx.Provider>
  );
}
