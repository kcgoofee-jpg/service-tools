/**
 * 成员首页（/）。
 * 登录成员：额度卡在第一屏，Key / 重置券 / 作品备份在下面；游客：登录与加入入口 + 三步上手 + 客户端设置 + 使用须知。
 * 所有人都有：实时状态卡、站长公告（有的话）、捐赠。
 */
import { useEffect, useState } from "preact/hooks";
import { Skeleton, toast } from "../../ui";
import { Announcement } from "./Announcement";
import { StoreCtx } from "./ctx";
import { Donate } from "./Donate";
import { GuestGuides, GuestHero, Steps } from "./Guest";
import { displayName } from "./helpers";
import { Backup, Coupons, KeyCard, NoKeyCard, QuotaCard, SurveyLink } from "./Member";
import { StatusCard } from "./StatusCard";
import { createStore, type Store } from "./store";
import { SiteFooter, Topbar } from "./Topbar";
import type { MeMember } from "./types";
import "./landing.css";

/** /login/callback 失败或登录被暂停时会回到 /?login=failed|paused：提示一下并把查询参数从地址栏去掉。 */
function useLoginNotice() {
  useEffect(() => {
    const params = new URLSearchParams(location.search);
    const v = params.get("login");
    if (!v) return;
    if (v === "failed") {
      toast(
        "Discord 登录没有成功。请确认你已加入猫头鹰公益站的 Discord 服务器，然后再点一次「用 Discord 登录」。",
        {
          error: true,
          ms: 9000,
        },
      );
    } else if (v === "paused") {
      toast("网页登录暂时关闭，开放后再来登录。你仍然可以在 Discord 里用 /quota 查额度。", {
        error: true,
        ms: 9000,
      });
    }
    params.delete("login");
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
        {me.has_key && (
          <div class="lp-o5">
            <Backup me={me} />
          </div>
        )}
        {me.has_key && (
          <div class="lp-o6">
            <SurveyLink />
          </div>
        )}
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
      <SiteFooter />
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
