import { mkdirSync } from "node:fs";
import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, expect, type Page, test } from "@playwright/test";
import { COOKIES_FILE, readCreds } from "./devEnv";

/**
 * 首页端到端。对着 `cargo xtask dev` 的合成数据跑（见 playwright.config.ts 顶部的说明）；
 * 个别场景（安静时段、冷却、重置券……）用 page.route 在真实响应上改字段来造。
 */

const member = readCreds();
const needMember = !member.member;

async function asMember(context: BrowserContext) {
  await context.addCookies([
    { name: "owl_member", value: member.member ?? "", domain: "localhost", path: "/" },
  ]);
}

/** 在真实响应上改字段（保持其余字段真实）。 */
async function patch(
  page: Page,
  url: RegExp,
  fn: (body: Record<string, unknown>) => Record<string, unknown>,
) {
  await page.route(url, async (route) => {
    try {
      const res = await route.fetch();
      const body = (await res.json()) as Record<string, unknown>;
      await route.fulfill({ response: res, json: fn(body) });
    } catch {
      // 测试结束、页面已关闭时还在途的轮询：忽略
      await route.abort().catch(() => {});
    }
  });
}
const LIVE = /\/public\/live(\?.*)?$/;
const STATUS = /\/public\/status(\?.*)?$/;
const ME = /\/public\/me(\?.*)?$/;
const LIVE_STREAM = "**/public/live/stream";

// 首页优先走 SSE（/public/live/stream），EventSource 收到非 200 才会放弃并退回轮询 /public/live。下面的用例都靠
// page.route 改轮询的响应来造场景，所以让 SSE 回 404（abort 的话 EventSource 会一直重连、不退回轮询）；
// 真实的 SSE 由「实时推送」那条用例单独验证。
test.beforeEach(async ({ page }) => {
  await page.route(LIVE_STREAM, (route) => route.fulfill({ status: 404, body: "" }));
});

/** 收集控制台错误。实时推送 /public/live/stream 断线时页面会退回轮询，这条不算。 */
function watchConsole(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() !== "error") return;
    if (m.location().url.includes("/public/live/stream")) return;
    errors.push(`${m.text()} @ ${m.location().url}`);
  });
  return errors;
}

async function seriousViolations(page: Page) {
  const r = await new AxeBuilder({ page }).analyze();
  return r.violations
    .filter((v) => v.impact === "serious" || v.impact === "critical")
    .map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(" ")).join(" | ")}`);
}

const noHorizontalScroll = (page: Page) =>
  page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);

test.describe("游客", () => {
  test("渲染：实时状态、三步上手、客户端设置、使用须知、页脚", async ({ page }) => {
    const errors = watchConsole(page);
    await page.goto("/");
    await expect(page.getByRole("heading", { level: 1, name: "猫头鹰公益站" })).toBeVisible();

    // 实时状态：一句话 + 两条用量
    const status = page.locator(".lp-status");
    await expect(status.getByRole("heading", { name: "当前状态" })).toBeVisible();
    await expect(status.locator(".lp-status__text")).not.toBeEmpty();
    await expect(status.getByRole("progressbar", { name: /全站今日已用/ })).toBeVisible();
    await expect(status.getByRole("progressbar", { name: /V5 已用/ })).toBeVisible();
    await expect(page.getByRole("status").filter({ hasText: /\S/ })).not.toHaveCount(0);

    // 三步上手
    await expect(page.getByRole("heading", { level: 2, name: "三步上手" })).toBeVisible();
    await expect(page.locator(".lp-step")).toHaveCount(3);
    await expect(page.getByRole("button", { name: "复制领取命令" })).toBeVisible();
    await expect(page.locator(".lp-step").nth(1)).toContainText("/register");
    await expect(page.locator(".lp-step").nth(2)).toContainText(new URL(page.url()).origin);

    // 客户端设置：三个标签，方向键可切换
    const tabs = page.getByRole("tablist", { name: "客户端" }).getByRole("tab");
    await expect(tabs).toHaveText(["柏宝绘", "文本客户端（OpenAI 兼容）", "智绘姬（st-chatu8）"]);
    await tabs.first().focus();
    await page.keyboard.press("ArrowRight");
    await expect(tabs.nth(1)).toHaveAttribute("aria-selected", "true");
    await expect(page.getByRole("tabpanel")).toContainText("/v1");
    await tabs.nth(2).click();
    await expect(page.getByRole("tabpanel")).toContainText("生图间隔时间");

    // 使用须知：数字来自接口，页面里不再有旧文案
    const rules = page.locator(".lp-rules");
    await expect(rules).toContainText("最近 60 分钟（滚动窗口）");
    await expect(rules).toContainText("每日 7 点重置");
    const text = await page.locator("main").innerText();
    for (const gone of ["3 小时", "证据", "候补", "缩略图", "闲置", "1000 点", "分享核对"]) {
      expect(text, `旧文案「${gone}」应已删除`).not.toContain(gone);
    }

    // 页脚
    const foot = page.getByRole("contentinfo");
    await expect(foot).toContainText("非商业公益服务，不保证可用性");
    await expect(foot.getByRole("link", { name: "条款" })).toHaveAttribute("href", "/terms");
    await expect(foot.getByRole("link", { name: "隐私" })).toHaveAttribute("href", "/privacy");
    expect(errors).toEqual([]);
  });

  test("首屏数据：网关注入 owl-boot 时直接用，不再请求 /public/me", async ({ page }) => {
    const boot = {
      me: {
        logged_in: true,
        has_key: true,
        key: "nai-bootbootbootbootbootbootbootboot",
        discord: { display_name: "注入用户" },
        expires_at: null,
        today: {
          images: 3,
          v5: 1,
          legacy: 2,
          daily_images: 40,
          daily_v5: 5,
          anlas: 0,
          daily_anlas: 0,
          reset_hour: 7,
        },
        coupons: [],
      },
      live: {
        quota: { images_today: 123, units_today: 123, images_cap: 500, v5_today: 9, v5_cap: 60 },
        queue: { waiting: 0, running: 0 },
        guard: { hour: 1, hour_cap: 100, burst_wait: 0, cooldown: 0, blocked: null },
        interval: 5,
      },
      status: { web_login: true },
    };
    await page.route("**/", async (route) => {
      const res = await route.fetch();
      const html = (await res.text()).replace(
        "<!--owl-boot-->",
        `<script type="application/json" id="owl-boot">${JSON.stringify(boot)}</script>`,
      );
      await route.fulfill({ response: res, body: html });
    });
    const meRequests: string[] = [];
    page.on("request", (r) => {
      if (/\/public\/me(\?|$)/.test(r.url())) meRequests.push(r.url());
    });
    // 首屏就绪前把真实的实时数据挡住，证明第一屏来自注入的数据
    await page.route(LIVE, (r) => r.abort());
    await page.goto("/");
    await expect(page.getByRole("banner").getByRole("button", { name: /注入用户/ })).toBeVisible();
    await expect(page.locator(".lp-quota")).toContainText("2 / 40");
    await expect(page.locator(".lp-status")).toContainText("123 / 500");
    await expect(page.locator(".lp-status__text")).toHaveText("马上开始 · 通道空闲");
    expect(meRequests).toEqual([]);
  });

  test("网页登录开放时：顶栏与首屏都有「用 Discord 登录」，指向 /login", async ({ page }) => {
    await patch(page, STATUS, (b) => ({ ...b, web_login: true }));
    await patch(page, LIVE, (b) => ({ ...b, web_login: true }));
    await page.goto("/");
    const login = page.getByRole("link", { name: "用 Discord 登录" });
    await expect(login).toHaveCount(2);
    await expect(login.first()).toHaveAttribute("href", "/login");
    await expect(page.getByRole("banner").getByRole("link", { name: "用 Discord 登录" })).toBeVisible();
  });

  test("网页登录关闭时不显示登录按钮，并说明原因", async ({ page }) => {
    await patch(page, STATUS, (b) => ({ ...b, web_login: false }));
    await patch(page, LIVE, (b) => ({ ...b, web_login: false }));
    await page.goto("/");
    await expect(page.getByRole("link", { name: "用 Discord 登录" })).toHaveCount(0);
    await expect(page.getByText("网页登录暂未开放")).toBeVisible();
  });

  test("加入 Discord：有邀请链接时是新窗口外链；关闭入口时置灰", async ({ page }) => {
    await patch(page, STATUS, (b) => ({ ...b, discord_invite: "https://discord.gg/owl-test" }));
    await patch(page, LIVE, (b) => ({
      ...b,
      discord_invite: "https://discord.gg/owl-test",
      home_join_open: true,
    }));
    await page.goto("/");
    const join = page.getByRole("link", { name: "加入 Discord" });
    await expect(join).toHaveCount(2);
    await expect(join.first()).toHaveAttribute("href", "https://discord.gg/owl-test");
    await expect(join.first()).toHaveAttribute("rel", /noopener/);

    await page.unroute(LIVE);
    await patch(page, LIVE, (b) => ({
      ...b,
      discord_invite: "https://discord.gg/owl-test",
      home_join_open: false,
    }));
    await page.reload();
    await expect(page.getByRole("button", { name: "关闭注册" }).first()).toBeDisabled();
  });

  test("主题切换：写入 owl-theme，刷新后保持", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await page.goto("/");
    const html = page.locator("html");
    await page.getByRole("button", { name: "切换到深色主题" }).click();
    await expect(html).toHaveAttribute("data-theme", "dark");
    expect(await page.evaluate(() => localStorage.getItem("owl-theme"))).toBe("dark");
    await expect(page.getByRole("button", { name: "切换到浅色主题" })).toBeVisible();
    await page.reload();
    await expect(html).toHaveAttribute("data-theme", "dark");
    await page.getByRole("button", { name: "切换到浅色主题" }).click();
    await expect(html).toHaveAttribute("data-theme", "light");
  });

  test("?login=failed 提示一次并清掉地址栏参数；?login=paused 同理", async ({ page }) => {
    await page.goto("/?login=failed");
    await expect(page.getByText("Discord 登录没有成功")).toBeVisible();
    await expect.poll(() => new URL(page.url()).search).toBe("");
    await page.goto("/?login=paused");
    await expect(page.getByText("网页登录暂时关闭")).toBeVisible();
    await expect.poll(() => new URL(page.url()).search).toBe("");
  });

  test("公告：有公告时在沙箱 iframe 里显示", async ({ page }) => {
    await page.route("**/announcement", (r) =>
      r.fulfill({ contentType: "text/html; charset=utf-8", body: "<p>今晚 22 点维护</p>" }),
    );
    await patch(page, STATUS, (b) => ({ ...b, has_announcement: true }));
    await patch(page, LIVE, (b) => ({ ...b, has_announcement: true }));
    await page.goto("/");
    const frame = page.locator("iframe[title='站长公告']");
    await expect(frame).toHaveAttribute("sandbox", "");
    await expect(frame).toHaveAttribute("src", "/announcement");
    await expect(page.frameLocator("iframe[title='站长公告']").getByText("今晚 22 点维护")).toBeVisible();
  });

  test("捐赠：必填校验、提交内容与成功提示", async ({ page }) => {
    let sent: Record<string, string> | null = null;
    await page.route("**/public/donate", async (route) => {
      sent = route.request().postDataJSON();
      await route.fulfill({ json: { ok: true } });
    });
    await page.goto("/");
    const card = page.locator("#donate");
    await expect(card.getByRole("heading", { name: "捐赠 NovelAI 账号" })).toBeVisible();
    await card.getByRole("button", { name: "我要捐赠" }).click();
    // 打开后焦点进 Token 框
    await expect(card.getByLabel("NovelAI Token（必填）")).toBeFocused();

    // 两项都空：两条提示同时出现，焦点回到第一个出错的框
    await card.getByRole("button", { name: "提交给站长" }).click();
    await expect(card.getByRole("alert")).toHaveText([
      "Token 好像不完整：请从 NovelAI 官网 Account 页整段复制",
      "请写一句留言 / 使用要求（站长好接进合适的池子）",
    ]);
    await expect(card.getByLabel("NovelAI Token（必填）")).toBeFocused();
    await expect(card.getByLabel("NovelAI Token（必填）")).toHaveAttribute("aria-invalid", "true");
    await card.getByLabel("NovelAI Token（必填）").fill(`pst-${"x".repeat(40)}`);
    await card.getByRole("button", { name: "提交给站长" }).click();
    await expect(card.getByRole("alert")).toHaveText("请写一句留言 / 使用要求（站长好接进合适的池子）");
    await expect(card.getByLabel("留言 / 使用要求（必填）")).toBeFocused();

    await card.getByLabel("留言 / 使用要求（必填）").fill("只用 V4.5");
    await card.getByLabel("联系方式（选填）").fill("QQ 10000");
    await card.getByRole("button", { name: "提交给站长" }).click();
    await expect(page.getByText("已收到，谢谢")).toBeVisible();
    expect(sent).toEqual({ token: `pst-${"x".repeat(40)}`, message: "只用 V4.5", contact: "QQ 10000" });
    await expect(card.getByRole("button", { name: "我要捐赠" })).toBeVisible();
  });

  test("状态一句话：安静时段 / 上游冷却 / 小时上限，与 API 原因同一说法", async ({ page }) => {
    const sentence = page.locator(".lp-status__text");
    await page.goto("/");
    const setBlocked = async (guard: Record<string, unknown>) => {
      await page.unroute(LIVE).catch(() => {});
      await patch(page, LIVE, (b) => ({ ...b, guard: { ...(b.guard as object), ...guard } }));
      await page.reload();
    };
    await setBlocked({
      blocked: { code: "quiet_cap", text: "现在是安静时段（3:00–7:00），暂停出图，7 点恢复" },
    });
    await expect(sentence).toHaveText("安静时段 · 暂停出图 · 7 点恢复");

    await setBlocked({ cooldown: 45, blocked: null });
    await expect(sentence).toHaveText("上游图片服务限流保护中 · 所有图片生成暂停约 45 秒");

    await setBlocked({
      cooldown: 0,
      blocked: {
        code: "hourly_cap",
        text: "最近 60 分钟的出图量已达上限（滚动窗口，全站每小时 150 张，不是整点重置），约 12 分钟后随最早的图滑出窗口逐张恢复",
      },
    });
    await expect(sentence).toContainText("最近 60 分钟已到上限");
    await expect(sentence).toContainText("约 12 分钟后逐张恢复");
    // 读屏播报区同步更新
    await expect(page.locator("[role=status][aria-live=polite].sr-only").first()).toContainText(
      "最近 60 分钟已到上限",
    );
  });

  test("实时推送：不拦截 SSE 时页面连上 /public/live/stream，并显示网关推来的状态", async ({ page }) => {
    await page.route(LIVE_STREAM, (route) => route.continue()); // 盖掉上面 beforeEach 的拦截
    const stream = page.waitForResponse(LIVE_STREAM);
    await page.goto("/");
    const res = await stream;
    expect(res.status()).toBe(200);
    expect(res.headers()["content-type"]).toContain("text/event-stream");
    await expect(page.locator(".lp-status__text")).not.toBeEmpty();
    await expect(page.locator(".lp-status").getByRole("progressbar", { name: /全站今日已用/ })).toBeVisible();
  });

  test("手机 375px：无横向滚动、控件不溢出", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await patch(page, STATUS, (b) => ({ ...b, web_login: true }));
    await patch(page, LIVE, (b) => ({ ...b, web_login: true }));
    await page.goto("/");
    await expect(page.getByRole("banner").getByRole("link", { name: "用 Discord 登录" })).toBeVisible();
    expect(await noHorizontalScroll(page)).toBe(true);
    // 顶栏的几个控件都在视口内
    for (const el of [
      page.getByRole("link", { name: "运行情况" }),
      page.getByRole("button", { name: /切换到/ }),
      page.getByRole("banner").getByRole("link", { name: "用 Discord 登录" }),
    ]) {
      const box = await el.boundingBox();
      expect(box && box.x >= 0 && box.x + box.width <= 375).toBe(true);
    }
  });

  for (const scheme of ["light", "dark"] as const) {
    test(`无障碍（axe）：游客 ${scheme} 没有严重 / 致命问题`, async ({ page }) => {
      await page.emulateMedia({ colorScheme: scheme });
      await patch(page, STATUS, (b) => ({ ...b, web_login: true, has_announcement: false }));
      await patch(page, LIVE, (b) => ({ ...b, web_login: true, has_announcement: false }));
      await page.goto("/");
      await page.locator(".lp-status__text").waitFor();
      expect(await seriousViolations(page)).toEqual([]);
      // 展开捐赠表单与另一个标签再测一遍
      await page.getByRole("button", { name: "我要捐赠" }).click();
      await page.getByRole("tab", { name: /智绘姬/ }).click();
      expect(await seriousViolations(page)).toEqual([]);
    });
  }
});

test.describe("登录成员", () => {
  test.skip(needMember, `没有成员会话文件 ${COOKIES_FILE}（先跑 cargo xtask dev）`);
  test.beforeEach(async ({ context }) => asMember(context));

  test("额度卡是第一块内容：V4.5 / V5 / Anlas、每日 7 点重置、最近 60 分钟（滚动窗口）", async ({ page }) => {
    const errors = watchConsole(page);
    await page.goto("/");
    const main = page.locator("main");
    // 主区域里第一个二级标题就是「我今天的额度」，且在第一屏（视口内）
    const firstHeading = main.getByRole("heading", { level: 2 }).first();
    await expect(firstHeading).toHaveText("我今天的额度");
    const quota = page.locator(".lp-quota");
    const box = await quota.boundingBox();
    expect(box && box.y < 300).toBe(true);
    await expect(quota).toContainText("每日 7 点重置");

    await expect(quota.getByRole("progressbar", { name: /V4\.5/ })).toBeVisible();
    await expect(quota.getByRole("progressbar", { name: /V5/ })).toBeVisible();
    await expect(quota.getByRole("progressbar", { name: /Anlas/ })).toBeVisible();
    await expect(quota.getByRole("progressbar", { name: /最近 60 分钟（滚动窗口）/ })).toBeVisible();
    await expect(quota).toContainText("我的平均出图耗时");

    // 游客才有的内容不出现
    await expect(page.getByRole("heading", { name: "三步上手" })).toHaveCount(0);
    await expect(page.getByRole("heading", { name: "使用须知" })).toHaveCount(0);
    expect(errors).toEqual([]);
  });

  test("Key：默认打码，可显示 / 隐藏与复制；换 Key 用 /resetkey；不出现钥匙图标", async ({
    page,
    context,
  }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    const card = page.locator(".lp-keycard");
    await expect(card.getByRole("heading", { name: "我的 Key" })).toBeVisible();
    await expect(card).toContainText("有效期至");
    await expect(card).toContainText("/resetkey");
    const shown = card.locator(".copy__value").first();
    await expect(shown).toContainText("…");
    await expect(shown).not.toContainText(member.member_key ?? "");

    await card.getByRole("button", { name: "显示完整 Key" }).click();
    await expect(shown).toHaveText(member.member_key ?? "");
    await card.getByRole("button", { name: "隐藏 Key" }).click();
    await expect(shown).toContainText("…");

    await card.getByRole("button", { name: "复制Key" }).click();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(member.member_key);
    await expect(card.getByRole("button", { name: "复制接入地址" })).toBeVisible();
    // 站长规定：任何位置都没有钥匙图标
    await expect(page.locator("svg.lucide-key, svg.lucide-key-round, svg.lucide-key-square")).toHaveCount(0);
  });

  test("用户菜单：显示名，退出登录后回到游客视图", async ({ page }) => {
    await page.goto("/");
    const menuButton = page.getByRole("banner").getByRole("button", { name: /月见草/ });
    await expect(menuButton).toBeVisible();
    await menuButton.click();
    await page.getByRole("menuitem", { name: /退出登录/ }).click();
    await expect(page.getByRole("heading", { level: 2, name: "三步上手" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "我今天的额度" })).toHaveCount(0);
  });

  test("重置券：列表、确认对话框、使用后刷新", async ({ page }) => {
    const now = Math.floor(Date.now() / 1000);
    let used = false;
    await patch(page, ME, (b) => ({
      ...b,
      coupons: used
        ? []
        : [{ id: 7, kind: "reset", note: "补偿 10-13", created_at: now, expires_at: now + 3 * 86400 }],
    }));
    await page.route("**/public/me/coupons/7/use", async (route) => {
      used = true;
      await route.fulfill({ json: { ok: true } });
    });
    await page.goto("/");
    const card = page.locator("section", { has: page.getByRole("heading", { name: "重置券" }) });
    await expect(card).toContainText("还有 3 天");
    await expect(card).toContainText("补偿 10-13");
    await card.getByRole("button", { name: "使用" }).click();
    const dialog = page.getByRole("dialog", { name: "使用重置券" });
    await expect(dialog).toContainText("现在重置今天的 V5 与 Anlas 额度？");
    await dialog.getByRole("button", { name: "取消" }).click();
    await expect(dialog).toBeHidden();
    expect(used).toBe(false);

    await card.getByRole("button", { name: "使用" }).click();
    await dialog.getByRole("button", { name: "确定使用" }).click();
    await expect(page.getByText("已重置，今天的 V5 与 Anlas 额度已恢复")).toBeVisible();
    await expect(page.getByRole("heading", { name: "重置券" })).toHaveCount(0);
  });

  test("作品备份：有可打包的图时显示保留天数与 EPUB / ZIP 链接；没有时整块不显示", async ({ page }) => {
    await patch(page, ME, (b) => ({ ...b, images_stored: 12, image_retention_days: 2 }));
    await page.goto("/");
    const card = page.locator("section", { has: page.getByRole("heading", { name: /作品备份/ }) });
    await expect(card).toContainText("最近 2 天的 12 张原图");
    await expect(card.getByRole("link", { name: /EPUB/ })).toHaveAttribute(
      "href",
      "/public/my-export?format=epub",
    );
    await expect(card.getByRole("link", { name: /ZIP/ })).toHaveAttribute(
      "href",
      "/public/my-export?format=zip",
    );

    await page.unroute(ME);
    await patch(page, ME, (b) => ({ ...b, images_stored: 0, image_retention_days: 0 }));
    await page.reload();
    await expect(page.getByRole("heading", { name: /作品备份/ })).toHaveCount(0);
  });

  test("问卷入口与捐赠都在", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("link", { name: "去填写" })).toHaveAttribute("href", "/survey");
    await expect(page.locator("#donate")).toBeVisible();
  });

  test("从问卷页点登录回来：首页把人送回 /survey（一次性）", async ({ page }) => {
    await page.addInitScript(() => {
      if (location.pathname === "/") sessionStorage.setItem("owl-next", "/survey");
    });
    await page.goto("/");
    await page.waitForURL("**/survey");
    await expect(page.getByRole("heading", { level: 1, name: "成员问卷" })).toBeVisible();
    expect(await page.evaluate(() => sessionStorage.getItem("owl-next"))).toBeNull();
  });

  test("已登录但没有 Key：注册开放时提示用 /register 领取，关闭时说明原因", async ({ page }) => {
    await patch(page, ME, (b) => ({ ...b, has_key: false, key: undefined, today: undefined }));
    const registration = (open: boolean) => (b: Record<string, unknown>) => ({
      ...b,
      registration: { configured: true, open },
    });
    await patch(page, LIVE, registration(true));
    await patch(page, STATUS, registration(true));
    await page.goto("/");
    await expect(page.getByText("你还没有 Key")).toBeVisible();
    await expect(page.getByRole("button", { name: "复制命令" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "我今天的额度" })).toHaveCount(0);

    await page.unroute(LIVE);
    await page.unroute(STATUS);
    await patch(page, LIVE, registration(false));
    await patch(page, STATUS, registration(false));
    await page.reload();
    await expect(page.getByText("注册当前已关闭，开放时间看站长公告")).toBeVisible();
    await expect(page.getByRole("button", { name: "复制命令" })).toHaveCount(0);
  });

  test("手机 375px：无横向滚动；额度卡在状态卡之前", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await patch(page, ME, (b) => ({ ...b, images_stored: 3, image_retention_days: 2 }));
    await page.goto("/");
    await expect(page.locator(".lp-quota")).toBeVisible();
    expect(await noHorizontalScroll(page)).toBe(true);
    const q = await page.locator(".lp-quota").boundingBox();
    const s = await page.locator(".lp-status").boundingBox();
    expect(q && s && q.y < s.y).toBe(true);
    // 显示完整 Key 后也不撑破页面
    await page.getByRole("button", { name: "显示完整 Key" }).click();
    expect(await noHorizontalScroll(page)).toBe(true);
  });

  for (const scheme of ["light", "dark"] as const) {
    test(`无障碍（axe）：成员 ${scheme} 没有严重 / 致命问题`, async ({ page }) => {
      await page.emulateMedia({ colorScheme: scheme });
      const now = Math.floor(Date.now() / 1000);
      await patch(page, ME, (b) => ({
        ...b,
        images_stored: 5,
        image_retention_days: 2,
        coupons: [{ id: 1, kind: "reset", note: "测试", expires_at: now + 86400 }],
      }));
      await page.goto("/");
      await page.locator(".lp-quota").waitFor();
      expect(await seriousViolations(page)).toEqual([]);
      await page.getByRole("button", { name: "显示完整 Key" }).click();
      await page.getByRole("button", { name: "使用", exact: true }).click();
      expect(await seriousViolations(page)).toEqual([]);
    });
  }
});

/* 截图：E2E_SHOTS=/tmp/owl-shots-landing pnpm exec playwright test landing -g 截图 */
test.describe("截图", () => {
  test.skip(!process.env.E2E_SHOTS, "设置 E2E_SHOTS=<目录> 才截图（截图不提交）");
  const sizes = { desktop: { width: 1280, height: 900 }, mobile: { width: 375, height: 812 } };
  for (const who of ["guest", "member"] as const) {
    for (const [name, viewport] of Object.entries(sizes)) {
      for (const scheme of ["light", "dark"] as const) {
        test(`截图 ${who} ${name} ${scheme}`, async ({ page, context }) => {
          test.skip(who === "member" && needMember, "没有成员会话");
          const dir = process.env.E2E_SHOTS ?? "";
          mkdirSync(dir, { recursive: true });
          if (who === "member") await asMember(context);
          await page.setViewportSize(viewport);
          await page.emulateMedia({ colorScheme: scheme });
          if (who === "guest") {
            await patch(page, STATUS, (b) => ({
              ...b,
              web_login: true,
              discord_invite: "https://discord.gg/owl",
            }));
            await patch(page, LIVE, (b) => ({
              ...b,
              web_login: true,
              home_join_open: true,
              discord_invite: "https://discord.gg/owl",
            }));
          } else {
            const now = Math.floor(Date.now() / 1000);
            await patch(page, ME, (b) => ({
              ...b,
              images_stored: 8,
              image_retention_days: 2,
              coupons: [{ id: 1, kind: "reset", note: "补偿 10-13", expires_at: now + 3 * 86400 }],
            }));
          }
          await page.goto("/");
          await page.locator(".lp-status__text").waitFor();
          await page.waitForTimeout(400);
          await page.screenshot({ path: `${dir}/landing-${who}-${name}-${scheme}.png`, fullPage: true });
        });
      }
    }
  }
});
