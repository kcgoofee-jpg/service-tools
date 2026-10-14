import { LogOut, Moon, Sun } from "lucide-preact";
import { useEffect, useState } from "preact/hooks";
import { ApiError, sendJSON } from "../../lib/api";
import { effectiveTheme, themeChoice, toggleTheme } from "../../lib/theme";
import { Button, Menu, toast } from "../../ui";

/** 明暗切换：图标表示「点了会变成什么」，读屏也是同样的说法。 */
export function ThemeButton() {
  // 跟随系统时，系统主题变了也要重绘
  const [, bump] = useState(0);
  useEffect(() => {
    const mq = matchMedia("(prefers-color-scheme: dark)");
    const on = () => bump((n) => n + 1);
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, []);
  void themeChoice.value;
  const dark = effectiveTheme() === "dark";
  const label = dark ? "切换到浅色主题" : "切换到深色主题";
  return (
    <Button variant="ghost" icon aria-label={label} title={label} onClick={toggleTheme}>
      {dark ? <Sun size={18} aria-hidden="true" /> : <Moon size={18} aria-hidden="true" />}
    </Button>
  );
}

async function logout() {
  try {
    await sendJSON("POST", "/logout");
    location.reload();
  } catch (e) {
    toast(e instanceof ApiError ? e.message : "退出失败，请稍后再试", { error: true });
  }
}

/** Discord 头像地址（头像是哈希，可能带动图前缀 a_）；拿不到就用名字首字。 */
function avatarUrl(id?: string, hash?: string | null): string {
  return id && hash && /^(a_)?[0-9a-f]{32}$/.test(hash)
    ? `https://cdn.discordapp.com/avatars/${id}/${hash}.png?size=64`
    : "";
}

/** 右上角账户入口：游客是一个「登录」按钮；登录后是头像，点开是名字 + 退出。 */
function Account(props: { user: TopbarUser }) {
  const [broken, setBroken] = useState(false);
  const { name } = props.user;
  const src = broken ? "" : avatarUrl(props.user.id, props.user.avatar);
  const initial = (name.match(/[\p{Script=Han}A-Za-z0-9]/u)?.[0] ?? "?").toUpperCase();
  return (
    <Menu
      button={(p) => (
        <button type="button" class="lp-avatar" aria-label={`账户：${name}`} title={name} {...p}>
          {src ? (
            <img
              src={src}
              alt=""
              width={32}
              height={32}
              referrerpolicy="no-referrer"
              onError={() => setBroken(true)}
            />
          ) : (
            <span aria-hidden="true">{initial}</span>
          )}
        </button>
      )}
      header={
        <>
          已登录
          <strong>{name}</strong>
        </>
      }
      items={[
        {
          label: (
            <>
              <LogOut size={14} aria-hidden="true" /> 退出登录
            </>
          ),
          onSelect: logout,
        },
      ]}
    />
  );
}

export type TopbarUser = { name: string; id?: string | undefined; avatar?: string | null | undefined };

export function Topbar(props: {
  /** 已登录成员；游客不传 */
  user?: TopbarUser | undefined;
  /** 网页 Discord 登录是否开放（只有接口明确说开放才显示按钮） */
  loginOpen: boolean;
}) {
  return (
    <header class="lp-top">
      <div class="lp-top__in">
        <a class="lp-brand" href="/">
          <span aria-hidden="true">🦉</span> 猫头鹰公益站
        </a>
        <span class="spacer" />
        <nav class="lp-top__nav" aria-label="站点导航">
          <a class="lp-top__link" href="/status" target="_blank" rel="noopener">
            运行情况
          </a>
        </nav>
        <ThemeButton />
        {props.user ? (
          <Account user={props.user} />
        ) : (
          props.loginOpen && (
            <a class="btn btn--primary btn--sm" href="/login" aria-label="用 Discord 登录">
              登录
            </a>
          )
        )}
      </div>
    </header>
  );
}

export function SiteFooter(props: { note?: string }) {
  return (
    <footer class="lp-foot">
      <p>
        猫头鹰公益站 · 非商业公益服务，不保证可用性 · <a href="/terms">条款</a> · <a href="/privacy">隐私</a>
        {props.note ? ` · ${props.note}` : ""}
      </p>
    </footer>
  );
}
