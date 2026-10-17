/**
 * 后台「总览 / 成员 / 日志」端到端：登录 → 逐页打开 → 打开 Key 编辑框 → 无控制台报错 →
 * 手机 375px 不横向滚动 → axe 无严重 / 致命问题 → 截图（浅色 / 深色 × 桌面 / 手机）。
 *
 * 运行（先起后端和 vite，见 playwright.config.ts 顶部：`cargo xtask dev` + `pnpm exec vite --port 5174`）：
 *   E2E_BASE=http://localhost:5174 pnpm exec playwright test e2e/admin-a.spec.ts
 * 后台密码取自环境变量 E2E_ADMIN_PASSWORD，或 `cargo xtask dev` 写出的 cookies.json（见 devEnv.ts，E2E_COOKIES 可改位置）。
 * 截图放 E2E_SHOTS（默认 /tmp/owl-shots-admin-a），不进仓库。
 */
import { mkdirSync } from "node:fs";
import AxeBuilder from "@axe-core/playwright";
import { type Cookie, expect, type Page, test } from "@playwright/test";
import { COOKIES_FILE, readCreds } from "./devEnv";
import { orphans } from "./orphans";

const SHOTS = process.env.E2E_SHOTS ?? "/tmp/owl-shots-admin-a";
const DESKTOP = { width: 1280, height: 900 };
const MOBILE = { width: 375, height: 812 };

function adminPassword(): string {
  const password = process.env.E2E_ADMIN_PASSWORD ?? readCreds().admin_password;
  if (password) return password;
  throw new Error(
    `找不到后台密码：设置 E2E_ADMIN_PASSWORD，或先跑 cargo xtask dev（会写出 ${COOKIES_FILE}）`,
  );
}

/** 收集页面错误。实时推送（/admin/api/events）断线时外壳会自动退回定时刷新，这条不算 */
function watch(page: Page): string[] {
  const errs: string[] = [];
  page.on("pageerror", (e) => errs.push(`pageerror: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() !== "error") return;
    const url = m.location().url;
    if (url.includes("/admin/api/events")) return;
    errs.push(`console: ${m.text()} @ ${url}`);
  });
  return errs;
}

let SESSION: Cookie[] = [];

const noHScroll = (page: Page) =>
  page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1);

async function axe(page: Page, label: string): Promise<void> {
  const res = await new AxeBuilder({ page }).analyze();
  const bad = res.violations.filter((v) => v.impact === "serious" || v.impact === "critical");
  expect(
    bad.map(
      (v) =>
        `${label}: ${v.id} (${v.impact}) ${v.nodes
          .slice(0, 3)
          .map((n) => n.target.join(" "))
          .join(" | ")}`,
    ),
  ).toEqual([]);
}

/** 每个页面：路径、就绪标志（等到这个元素出现才算加载完） */
const PAGES: Array<{ name: string; path: string; ready: (p: Page) => ReturnType<Page["locator"]> }> = [
  { name: "overview", path: "/admin", ready: (p) => p.getByRole("heading", { name: "各功能用量" }) },
  {
    name: "members",
    path: "/admin/members",
    ready: (p) => p.getByRole("button", { name: /展开.*的详情/ }).first(),
  },
  {
    name: "logs-audit",
    path: "/admin/logs/audit",
    ready: (p) => p.getByRole("button", { name: "导出该成员全部" }),
  },
  {
    name: "logs-usage",
    path: "/admin/logs/usage",
    ready: (p) => p.getByText("暂无记录").or(p.locator(".lg-listcard")),
  },
  { name: "logs-actions", path: "/admin/logs/actions", ready: (p) => p.getByLabel("操作者") },
  {
    name: "logs-bugs",
    path: "/admin/logs/bugs",
    ready: (p) => p.getByRole("button", { name: "全部标记已处理" }),
  },
  {
    name: "logs-feedback",
    path: "/admin/logs/feedback",
    ready: (p) => p.getByRole("button", { name: /导出 CSV/ }),
  },
  {
    name: "logs-survey",
    path: "/admin/logs/survey",
    ready: (p) => p.getByText("已填", { exact: true }).or(p.getByText("还没有成员填过问卷")),
  },
];

async function open(
  page: Page,
  path: string,
  ready: (p: Page) => ReturnType<Page["locator"]>,
): Promise<void> {
  await page.goto(path);
  await expect(ready(page).first()).toBeVisible();
}

// 后台限制登录尝试次数（5 次 / 5 分钟），所以只登录一次，把会话存下来各测试复用
// 一份会话、数据会改动（黑名单 / 标签 / 邀请 Key）：串行跑
test.describe.configure({ mode: "serial" });

test.beforeAll(async ({ browser, baseURL }) => {
  mkdirSync(SHOTS, { recursive: true });
  const ctx = await browser.newContext({ ...(baseURL ? { baseURL } : {}) });
  const r = await ctx.request.post("/admin/api/login", { data: { password: adminPassword() } });
  expect(r.ok(), "后台登录失败（可能触发了登录次数限制，等 5 分钟或调大 LOGIN_MAX_ATTEMPTS）").toBeTruthy();
  SESSION = await ctx.cookies();
  await ctx.close();
});

test("登录表单：错误密码有提示，正确密码进入后台", async ({ page }) => {
  // 未登录时 /me 与错误密码的 401 是预期的，只检查脚本异常
  const errs: string[] = [];
  page.on("pageerror", (e) => errs.push(e.message));
  await page.goto("/admin");
  await page.getByLabel("后台密码").fill("definitely-wrong-password");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("alert")).toBeVisible();
  await page.getByLabel("后台密码").fill(adminPassword());
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("navigation", { name: "后台导航" })).toBeVisible();
  expect(errs).toEqual([]);
});

test.describe("已登录", () => {
  test.beforeEach(async ({ context }) => {
    await context.addCookies(SESSION);
  });

  test("逐页打开：无控制台报错，导航角标显示未处理 Bug 数", async ({ page }) => {
    const errs = watch(page);
    for (const p of PAGES) {
      await open(page, p.path, p.ready);
      await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    }
    // 总览取到了未处理 Bug 数 → 导航「日志」有角标
    await open(page, "/admin", PAGES[0]?.ready as never);
    await expect(page.getByRole("link", { name: /^日志/ }).locator(".admin-nav__badge")).toBeVisible();
    expect(errs).toEqual([]);
  });

  test("总览：返回状态码与拒绝原因卡——三个视角、四个时间范围都能切", async ({ page }) => {
    await open(page, "/admin", PAGES[0]?.ready as never);
    await expect(page.getByRole("heading", { name: "返回状态码与拒绝原因" })).toBeVisible();
    const card = page.locator(".ov-card", {
      has: page.getByRole("heading", { name: "返回状态码与拒绝原因" }),
    });
    // 默认「网关→成员」：有请求就有状态码行，没有就是「还没有请求」
    await expect(card.locator(".ov-sc__row").or(card.getByText("这段时间还没有请求"))).not.toHaveCount(0);
    await card.getByRole("button", { name: "NovelAI→网关" }).click();
    await expect(card.getByText(/NovelAI 实际返回给网关/)).toBeVisible();
    await card.getByRole("button", { name: "拒绝原因" }).click();
    await expect(card.getByText(/按原因归类/)).toBeVisible();
    await expect(
      card.locator(".ov-sc__row").or(card.getByText("这段时间没有被拒绝或失败的请求")),
    ).not.toHaveCount(0);
    for (const range of ["1 小时", "24 小时", "7 天", "今日"]) {
      await card.getByRole("button", { name: range }).click();
      await expect(card.locator(".ov-sc__row").or(card.getByText(/没有被拒绝|还没有请求/))).not.toHaveCount(
        0,
      );
    }
  });

  test("日志：成员选择是可输入的搜索框——打字过滤、回车选中、「全部」清掉筛选", async ({ page }) => {
    await open(page, "/admin/logs/audit", PAGES.find((x) => x.name === "logs-audit")?.ready as never);
    const box = page.getByRole("combobox", { name: "成员" });
    await box.click();
    const list = page.getByRole("listbox", { name: "成员" });
    await expect(list).toBeVisible();
    await expect(list.getByRole("option").first()).toHaveText("全部成员");
    const total = await list.getByRole("option").count();
    // 不存在的名字：只剩「全部成员」，并说明没有匹配
    await box.fill("这个名字肯定不存在zzzz");
    await expect(list.getByRole("option")).toHaveCount(1);
    await expect(list).toContainText("没有匹配");
    // 清空再点开：全部回来；选第一个真实成员
    await box.fill("");
    if (total > 1) {
      await expect(list.getByRole("option")).toHaveCount(total);
      await box.press("ArrowDown");
      await box.press("Enter");
      await expect(list).toHaveCount(0);
      await expect(box).not.toHaveValue("");
      await expect(page.getByRole("button", { name: "导出该成员全部" })).toBeEnabled();
      // 「×」清掉，回到全部成员
      await page.getByRole("button", { name: /清除，回到全部成员/ }).click();
      await expect(box).toHaveValue("");
    }
    // 手机宽度不撑出横向滚动
    await page.setViewportSize(MOBILE);
    await box.click();
    expect(await noHScroll(page)).toBe(true);
  });

  test("总览：只列要处理的事（没有隐藏机制），统计格只有两个，已删的卡片不在", async ({ page }) => {
    await open(page, "/admin", PAGES[0]?.ready as never);
    await expect(page.getByRole("heading", { name: /件事要处理|一切正常/ })).toBeVisible();
    await expect(page.getByRole("button", { name: /隐藏/ })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /恢复/ })).toHaveCount(0);
    await expect(page.locator(".ov-stats .stat__label")).toHaveText(["今日出图", "今天在用的成员"]);
    for (const gone of ["系统状态", "近 7 天"])
      await expect(page.getByRole("heading", { name: gone })).toHaveCount(0);
    // 磁盘 / 预算 / 限流中账号这类非重大事项不在提醒里
    await expect(page.locator(".ov-issues")).not.toContainText(/磁盘|Anlas|限流中/);
  });

  test("成员：展开详情、Key 编辑框校验（空值 / 负数 / Anlas 规则）", async ({ page }) => {
    const errs = watch(page);
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    await page
      .getByRole("button", { name: /展开.*的详情/ })
      .first()
      .click();
    await page.getByRole("button", { name: "全部设置…" }).click();
    const dlg = page.getByRole("dialog", { name: /编辑：/ });
    await expect(dlg).toBeVisible();
    await dlg.getByRole("radio", { name: /^手动/ }).check();
    const images = dlg.getByLabel("每日 V4.5 及以下免费图张数");
    await images.fill("");
    await dlg.getByRole("button", { name: "保存" }).click();
    await expect(dlg.getByText("每日图片张数不能为空")).toBeVisible();
    await images.fill("-3");
    await dlg.getByRole("button", { name: "保存" }).click();
    await expect(dlg.getByText(/必须在 0～1000000 之间/)).toBeVisible();
    await images.fill("100");
    await dlg.getByRole("spinbutton", { name: /^每日 Anlas 上限/ }).fill("0");
    await dlg.getByRole("spinbutton", { name: /^每月 Anlas 上限/ }).fill("0");
    await dlg.getByRole("checkbox", { name: /^允许消耗 Anlas/ }).check();
    await dlg.getByRole("button", { name: "保存" }).click();
    await expect(dlg.getByRole("alert").filter({ hasText: /至少要有一个大于 0/ })).toBeVisible();
    await axe(page, "key-dialog");
    // 有未保存的修改：取消时先确认
    await dlg.getByRole("button", { name: "取消" }).click();
    await expect(page.getByText("有未保存的修改，确定放弃吗？")).toBeVisible();
    await page.getByRole("button", { name: "放弃修改" }).click();
    await expect(dlg).toBeHidden();
    expect(errs).toEqual([]);
  });

  /** 拦下成员接口，在真实数据上改一改（让被拒原因、回收天数可预期） */
  async function mockMembers(
    page: Page,
    edit: (body: { members: Array<Record<string, unknown>>; inactivity_days: number }) => void,
  ) {
    await page.route("**/admin/api/members", async (route) => {
      const res = await route.fetch();
      const body = await res.json();
      edit(body);
      await route.fulfill({ response: res, json: body });
    });
  }
  const bar = (page: Page) => page.getByRole("group", { name: "筛选成员" });

  test("成员：筛选是一条横栏——标签带人数、点选高亮、再点取消，没有下拉", async ({ page }) => {
    const errs = watch(page);
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    const b = bar(page);
    await expect(b.locator("select")).toHaveCount(0);
    await expect(page.getByRole("combobox", { name: "更多筛选" })).toHaveCount(0);
    // 已下线的筛选不在
    for (const gone of [/身份组/, /后台手动建/, /站长 \/ 测试/])
      await expect(b.getByRole("button", { name: gone })).toHaveCount(0);
    for (const name of [
      /^全部成员 \d+$/,
      /^今日活跃 \d+$/,
      /^还没出过图 \d+$/,
      /^24 小时新加入 \d+$/,
      /^24 小时被拒过 \d+$/,
      /^邀请 Key \d+$/,
      /^需要处理 \d+$/,
    ])
      await expect(b.getByRole("button", { name })).toBeVisible();

    // 默认：有要处理的人就看「需要处理」，没有就是「全部成员」；点「今日活跃」→ 高亮，再点 → 回到全部
    const today = b.getByRole("button", { name: /^今日活跃/ });
    const all = b.getByRole("button", { name: /^全部成员/ });
    await today.click();
    await expect(today).toHaveAttribute("aria-pressed", "true");
    await expect(all).toHaveAttribute("aria-pressed", "false");
    await today.click();
    await expect(today).toHaveAttribute("aria-pressed", "false");
    await expect(all).toHaveAttribute("aria-pressed", "true");

    // 键盘：Tab 到标签、空格 / 回车切换
    await b.getByRole("button", { name: /^还没出过图/ }).focus();
    await page.keyboard.press("Enter");
    await expect(b.getByRole("button", { name: /^还没出过图/ })).toHaveAttribute("aria-pressed", "true");
    expect(errs).toEqual([]);
  });

  test("成员：「24 小时被拒过」选中后，被拒原因在同一条横栏里紧跟着出现，可再窄一层", async ({ page }) => {
    await mockMembers(page, (body) => {
      const people = body.members.filter((m) => !m.is_admin && !m.is_test);
      for (const m of people) {
        m.rejected_24h = 0;
        m.auto_tags = [];
      }
      const rows = people.slice(0, 3);
      rows.forEach((m, i) => {
        m.rejected_24h = 2;
        m.auto_tags = i === 0 ? [{ tag: "传了底图", count: 2 }] : [{ tag: "请求过快", count: 2 }];
      });
    });
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    const b = bar(page);
    await expect(b.getByRole("button", { name: /^传了底图/ })).toHaveCount(0);
    const rejected = b.getByRole("button", { name: /^24 小时被拒过/ });
    await rejected.click();
    const slow = b.getByRole("button", { name: /^请求过快 2$/ });
    const img = b.getByRole("button", { name: /^传了底图 1$/ });
    await expect(slow).toBeVisible();
    await expect(img).toBeVisible();
    // 同一行：和「24 小时被拒过」同高同顶
    const [r, s] = await Promise.all([rejected.boundingBox(), slow.boundingBox()]);
    expect(Math.abs((r?.y ?? 0) - (s?.y ?? 99))).toBeLessThan(1.5);
    await slow.click();
    await expect(slow).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByRole("button", { name: /展开.*的详情/ })).toHaveCount(2);
    await slow.click();
    await expect(page.getByRole("button", { name: /展开.*的详情/ })).toHaveCount(3);
    // 换掉被拒筛选：原因收起
    await b.getByRole("button", { name: /^今日活跃/ }).click();
    await expect(slow).toHaveCount(0);
  });

  test("成员：回收关闭时没有「快被回收」，开启时标签用设置的天数", async ({ page }) => {
    await mockMembers(page, (body) => {
      body.inactivity_days = 0;
    });
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    await expect(bar(page).getByRole("button", { name: /^全部成员/ })).toBeVisible();
    await expect(bar(page).getByRole("button", { name: /快被回收/ })).toHaveCount(0);
    await page.unroute("**/admin/api/members");
    await mockMembers(page, (body) => {
      body.inactivity_days = 5;
    });
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    await expect(bar(page).getByRole("button", { name: /^快被回收（满 5 天） \d+$/ })).toBeVisible();
  });

  for (const size of [DESKTOP, MOBILE]) {
    test(`成员：筛选横栏在 ${size.width}px 是单行，不换行、不出孤字、不撑出横向滚动`, async ({ page }) => {
      await page.setViewportSize(size);
      await mockMembers(page, (body) => {
        // 最长的情形：回收开着、被拒原因展开
        body.inactivity_days = 3;
        const m = body.members.find((x) => !x.is_admin && !x.is_test);
        if (m) {
          m.rejected_24h = 1;
          m.auto_tags = [
            { tag: "传了底图", count: 1 },
            { tag: "请求过快", count: 1 },
          ];
        }
      });
      await open(page, "/admin/members", PAGES[1]?.ready as never);
      const b = bar(page);
      await b.getByRole("button", { name: /^24 小时被拒过/ }).click();
      await expect(b.getByRole("button", { name: /^请求过快/ })).toBeVisible();
      const geo = await b.evaluate((el) => {
        const chips = [...el.querySelectorAll<HTMLElement>("button")].map((x) => x.getBoundingClientRect());
        return {
          tops: [...new Set(chips.map((r) => Math.round(r.top)))],
          heights: [...new Set(chips.map((r) => Math.round(r.height)))],
          n: chips.length,
          fits: el.scrollWidth <= el.clientWidth + 1,
          noWrap: [...el.querySelectorAll<HTMLElement>("button")].every(
            (x) => getComputedStyle(x).whiteSpace === "nowrap",
          ),
          pageScroll: document.documentElement.scrollWidth <= window.innerWidth + 1,
        };
      });
      expect(geo.n).toBeGreaterThanOrEqual(8);
      expect(geo.tops, "所有标签同一行").toHaveLength(1);
      expect(geo.heights, "标签等高").toHaveLength(1);
      expect(geo.noWrap).toBe(true);
      expect(geo.pageScroll, "页面不横向滚动").toBe(true);
      // 放不下（手机，或桌面上原因也展开时）横栏自己横向滚，而不是换行
      if (!geo.fits) expect(await b.evaluate((el) => getComputedStyle(el).overflowX)).toBe("auto");
      expect(await orphans(page)).toEqual([]);
      await axe(page, `members-bar@${size.width}`);
    });
  }

  test("成员：?focus=<Key 编号> 展开并高亮该成员", async ({ page }) => {
    await open(page, "/admin/members?focus=3", PAGES[1]?.ready as never);
    const row = page.locator("#mb-m-3");
    await expect(row).toBeVisible();
    await expect(row).toHaveClass(/is-focus/);
    await expect(row.getByRole("button", { name: /收起.*的详情/ })).toBeVisible();
  });

  test("成员：新建邀请 Key 的消息说明用 Discord 登录", async ({ page }) => {
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    await page.getByRole("button", { name: "更多", exact: true }).first().click();
    await page.getByRole("menuitem", { name: "新建邀请 Key" }).click();
    const dlg = page.getByRole("dialog", { name: /已创建/ });
    await expect(dlg).toBeVisible();
    const msg = dlg.getByRole("textbox");
    await expect(msg).toHaveValue(/用 Discord 登录/);
    await expect(msg).not.toHaveValue(/网页登录开放后/);
    await dlg.getByRole("button", { name: "完成" }).click();
    await expect(dlg).toBeHidden();
  });

  test("成员：黑名单（添加 → 搜索 → 移出）与加标签", async ({ page }) => {
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    await page.getByRole("button", { name: "更多", exact: true }).first().click();
    await page.getByRole("menuitem", { name: /封禁黑名单/ }).click();
    const bans = page.getByRole("dialog", { name: "封禁黑名单" });
    await bans.getByLabel("Discord ID（纯数字）").fill("123");
    await bans.getByRole("button", { name: "加入黑名单" }).click();
    await expect(bans.getByText("Discord ID 必须是 17～20 位纯数字")).toBeVisible();
    await bans.getByLabel("Discord ID（纯数字）").fill("900000000000009999");
    await bans.getByLabel("封禁理由").fill("端到端测试");
    await bans.getByRole("button", { name: "加入黑名单" }).click();
    await expect(bans.getByText("900000000000009999").first()).toBeVisible();
    await axe(page, "bans-dialog");
    await bans.getByRole("button", { name: "移出黑名单" }).first().click();
    await page
      .getByRole("dialog", { name: "移出黑名单" })
      .getByRole("button", { name: "移出黑名单" })
      .click();
    await expect(bans.getByText("900000000000009999")).toHaveCount(0);
    await bans.getByRole("button", { name: "关闭", exact: true }).last().click();

    await page
      .getByRole("button", { name: /展开.*的详情/ })
      .first()
      .click();
    await page
      .getByRole("button", { name: /^标签$/ })
      .first()
      .click();
    const tag = page.getByRole("dialog", { name: /添加标签/ });
    await tag.getByRole("button", { name: "添加" }).click();
    await expect(tag.getByText("标签名不能为空")).toBeVisible();
    await tag.getByLabel("标签名").fill("端到端");
    await tag.getByRole("button", { name: "添加" }).click();
    await expect(tag).toBeHidden();
    const remove = page.getByRole("button", { name: "移除标签 端到端" });
    await expect(remove).toBeVisible();
    await remove.click();
    await expect(remove).toBeHidden();
  });

  test("成员：写操作流程（身份组 / 编辑 / 发券 / 重置额度 / 停用启用 / 重新生成 / 新建 Key / 高危确认）", async ({
    page,
  }) => {
    const errs = watch(page);
    await open(page, "/admin/members", PAGES[1]?.ready as never);
    // 「全部成员」筛选：停用后这位成员不会从列表里消失
    await page
      .getByRole("group", { name: "筛选成员" })
      .getByRole("button", { name: /^全部成员/ })
      .click();
    const btn = page.getByRole("button", { name: /展开.*的详情/ }).first();
    const detailId = await btn.getAttribute("aria-controls");
    expect(detailId).toBeTruthy();
    await btn.click();
    const row = page.locator(`#${detailId}`);
    await expect(row).toBeVisible();

    // 身份组：切到另一个再切回（切换是直接生效的，带 toast）
    const invite = row.getByRole("button", { name: "邀请", exact: true });
    const regular = row.getByRole("button", { name: "普通", exact: true });
    const [from, to] =
      (await regular.getAttribute("aria-pressed")) === "true" ? [regular, invite] : [invite, regular];
    await (to === invite ? invite : regular).click();
    await expect(page.getByText(/身份组切换为/)).toBeVisible();
    await from.click();

    // 显示 Key（默认遮住中间）
    await expect(row.locator(".mb-key__v")).toContainText("····");
    await row.getByRole("button", { name: "显示" }).click();
    await expect(row.locator(".mb-key__v")).not.toContainText("····");

    // 编辑后保存（不改内容）
    await row.getByRole("button", { name: "全部设置…" }).click();
    const edit = page.getByRole("dialog", { name: /编辑：/ });
    await edit.getByRole("button", { name: "保存" }).click();
    await expect(edit).toBeHidden();

    // 发重置券：天数 1～30
    await row.getByRole("button", { name: "发重置券" }).click();
    const coupon = page.getByRole("dialog", { name: /发重置券/ });
    await coupon.getByLabel("有效天数").fill("0");
    await coupon.getByRole("button", { name: "发出" }).click();
    await expect(coupon.getByText("有效期必须在 1～30 之间")).toBeVisible();
    await coupon.getByLabel("有效天数").fill("5");
    await coupon.getByRole("button", { name: "发出" }).click();
    await expect(coupon).toBeHidden();

    // 重置今日额度
    await row.getByRole("button", { name: /重置今日额度/ }).click();
    const reset = page.getByRole("dialog", { name: /重置.*今日额度/ });
    await reset.getByRole("button", { name: "重置", exact: true }).click();
    await expect(reset).toBeHidden();

    // 重新生成 Key：确认后显示新 Key
    await row.getByRole("button", { name: "更多" }).click();
    await page.getByRole("menuitem", { name: /重新生成 Key/ }).click();
    await page
      .getByRole("dialog", { name: /重新生成/ })
      .getByRole("button", { name: "重新生成", exact: true })
      .click();
    const reveal = page.getByRole("dialog", { name: "Key 已重新生成" });
    await expect(reveal.getByText(/^nai-/)).toBeVisible();
    await reveal.getByRole("button", { name: "完成" }).click();

    // 绑定 Discord：ID 格式校验
    await row.getByRole("button", { name: /^(换绑|手动绑定)$/ }).click();
    const bind = page.getByRole("dialog", { name: /(换绑|绑定) Discord/ });
    await bind.getByLabel("Discord 用户 ID").fill("12");
    await bind.getByRole("button", { name: /^(换绑|绑定)$/ }).click();
    await expect(bind.getByText(/17～20 位纯数字/).first()).toBeVisible();
    await bind.getByRole("button", { name: "取消" }).click();

    // 新建 Key（手动）：创建后展示 Key
    await page.getByRole("button", { name: "新建 Key" }).click();
    const create = page.getByRole("dialog", { name: "新建 Key" });
    await create.getByLabel("名称 / 备注").fill("e2e-临时");
    await create.getByRole("radio", { name: /^手动/ }).check();
    await create.getByRole("button", { name: "创建" }).click();
    const made = page.getByRole("dialog", { name: "新 Key 已创建" });
    await expect(made.getByText(/^nai-/)).toBeVisible();
    await made.getByRole("button", { name: "完成" }).click();

    // 奖励成员、全站重置：校验与确认词
    await page.getByRole("button", { name: "更多", exact: true }).first().click();
    await page.getByRole("menuitem", { name: /奖励成员/ }).click();
    const gift = page.getByRole("dialog", { name: "奖励成员" });
    await gift.getByRole("button", { name: "发出奖励" }).click();
    await expect(gift.getByText(/17～20 位纯数字/).first()).toBeVisible();
    await gift.getByRole("button", { name: "取消" }).click();
    await page.getByRole("button", { name: "更多", exact: true }).first().click();
    await page.getByRole("menuitem", { name: /全站重置 Key/ }).click();
    const all = page.getByRole("dialog", { name: "全站重置 Key" });
    await expect(all.getByRole("button", { name: "确认重置" })).toBeDisabled();
    await all.getByRole("button", { name: "取消" }).click();

    // 停用：停用的 Key 不再出现在成员列表里（谁用过仍记在日志里）
    await page.getByRole("searchbox", { name: "搜索成员" }).fill("e2e-临时");
    const tmp = page.getByRole("button", { name: /展开.*e2e-临时.*的详情/ });
    const tmpId = await tmp.getAttribute("aria-controls");
    await tmp.click();
    const tmpRow = page.locator(`#${tmpId}`);
    await tmpRow.getByRole("button", { name: "更多" }).click();
    await page.getByRole("menuitem", { name: "停用" }).click();
    const off = page.getByRole("dialog", { name: /停用/ });
    await off.getByRole("button", { name: "停用", exact: true }).click();
    await expect(off).toBeHidden();
    await expect(tmp).toHaveCount(0);
    expect(errs).toEqual([]);
  });

  test("日志：成员反馈一条一行（时间 · 成员 · 内容），旧版选择题的记录显示成一行摘要，可搜索", async ({
    page,
  }) => {
    const errs = watch(page);
    await open(page, "/admin/logs/feedback", PAGES[6]?.ready as never);
    await expect(page.getByRole("columnheader")).toHaveText(["时间", "成员", "内容"]);
    // 评价筛选和选择题统计已经去掉
    await expect(page.getByLabel("评价")).toHaveCount(0);
    await expect(page.getByText("好评满意率")).toHaveCount(0);
    // 新记录是成员写的话；旧记录（种子里有一条）是一行摘要
    await expect(page.getByText("晚上八点左右排队有点久，希望能快一点。")).toBeVisible();
    await expect(page.getByText(/^（旧版选择题）整体：满意；困扰：排队太久/)).toBeVisible();
    await axe(page, "feedback");
    // 按内容搜；换成找不到的关键字显示空状态
    const box = page.getByRole("searchbox", { name: "搜索" });
    await box.fill("ComfyUI");
    await expect(page.getByText("教程写得很清楚")).toBeVisible();
    await expect(page.getByText("晚上八点左右排队有点久")).toHaveCount(0);
    await box.fill("旧版选择题");
    await expect(page.getByText(/^（旧版选择题）/)).toBeVisible();
    await expect(page.getByText("教程写得很清楚")).toHaveCount(0);
    await box.fill("没有人会写这句话");
    await expect(page.getByText("未找到符合条件的反馈")).toBeVisible();
    expect(errs).toEqual([]);
  });

  test("日志：子页是路由，筛选条件在地址栏里，可直接打开", async ({ page }) => {
    await page.goto("/admin/logs");
    await expect(page).toHaveURL(/\/admin\/logs\/audit$/);
    await open(page, "/admin/logs/usage?feature=image&per=50", PAGES[3]?.ready as never);
    await expect(page.getByRole("combobox", { name: /^功能/ })).toHaveValue("image");
    await expect(page.getByRole("combobox", { name: /^每页/ })).toHaveValue("50");
    await page.getByRole("link", { name: "Bug 追踪" }).click();
    await expect(page).toHaveURL(/\/admin\/logs\/bugs$/);
    await expect(page.getByRole("link", { name: "Bug 追踪" })).toHaveAttribute("aria-current", "page");
  });

  test("日志：Bug 全部标记已处理有确认框；生成记录能打开大图", async ({ page }) => {
    await open(page, "/admin/logs/bugs", PAGES[5]?.ready as never);
    const all = page.getByRole("button", { name: "全部标记已处理" });
    if (await all.isEnabled()) {
      await all.click();
      const dlg = page.getByRole("dialog", { name: "全部标记已处理" });
      await expect(dlg).toBeVisible();
      await dlg.getByRole("button", { name: "取消" }).click();
      await expect(dlg).toBeHidden();
    }
    await open(page, "/admin/logs/audit", PAGES[2]?.ready as never);
    const first = page.getByRole("button", { name: /^查看 .* 的.*记录$/ }).first();
    await first.waitFor({ state: "visible", timeout: 5000 }).catch(() => undefined);
    if (await first.isVisible()) {
      await first.click();
      const lb = page.getByRole("dialog", { name: /的生成记录/ });
      await expect(lb).toBeVisible();
      await axe(page, "lightbox");
      await page.keyboard.press("ArrowRight");
      await expect(lb.getByText(/^2 \/ /)).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(lb).toBeHidden();
    }
    // 生成记录的设置只在「领 Key 与功能」里改，日志页不再有第二个入口
    await expect(page.getByRole("button", { name: "记录设置" })).toHaveCount(0);
  });

  for (const [vp, size] of [
    ["桌面 1280", DESKTOP],
    ["手机 375", MOBILE],
  ] as const) {
    test(`${vp}：各页无横向滚动、无严重无障碍问题`, async ({ page }) => {
      await page.setViewportSize(size);
      const errs = watch(page);
      for (const p of PAGES) {
        await open(page, p.path, p.ready);
        await page.waitForTimeout(300);
        expect(await noHScroll(page), `${p.name} 在 ${vp} 下出现横向滚动`).toBe(true);
        await axe(page, `${p.name}@${vp}`);
      }
      if (size.width === MOBILE.width) {
        // 成员表在手机上变成卡片
        await open(page, "/admin/members", PAGES[1]?.ready as never);
        await expect(page.getByRole("list", { name: "成员列表" })).toBeVisible();
        await expect(page.locator("table.mb-table")).toHaveCount(0);
        // 展开详情、打开编辑框，仍不横向滚动
        await page
          .getByRole("button", { name: /展开.*的详情/ })
          .first()
          .click();
        expect(await noHScroll(page)).toBe(true);
        await page.getByRole("button", { name: "全部设置…" }).click();
        await expect(page.getByRole("dialog", { name: /编辑：/ })).toBeVisible();
        expect(await noHScroll(page)).toBe(true);
      } else {
        await open(page, "/admin/members", PAGES[1]?.ready as never);
        await expect(page.locator("table.mb-table")).toBeVisible();
      }
      expect(errs).toEqual([]);
    });
  }

  test("字号手机与桌面一致（只有布局变化）", async ({ page }) => {
    const sizes: Record<string, string[]> = {};
    for (const [name, size] of [
      ["desktop", DESKTOP],
      ["mobile", MOBILE],
    ] as const) {
      await page.setViewportSize(size);
      await open(page, "/admin/members", PAGES[1]?.ready as never);
      await page
        .getByRole("button", { name: /展开.*的详情/ })
        .first()
        .click();
      sizes[name] = await page.evaluate(() =>
        ["h1", ".mb-name", ".mb-verdict", ".mb-kv", ".mb-chip", ".btn"].map((sel) => {
          const el = document.querySelector(sel);
          return el ? getComputedStyle(el).fontSize : "none";
        }),
      );
    }
    expect(sizes.mobile).toEqual(sizes.desktop);
  });

  test("截图：浅色 / 深色 × 桌面 / 手机", async ({ page }) => {
    test.setTimeout(240_000);
    for (const scheme of ["light", "dark"] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const [vpName, size] of [
        ["desktop", DESKTOP],
        ["mobile", MOBILE],
      ] as const) {
        await page.setViewportSize(size);
        const tag = `${vpName}-${scheme}`;
        for (const p of PAGES) {
          await open(page, p.path, p.ready);
          await page.waitForTimeout(500);
          await page.screenshot({ path: `${SHOTS}/${p.name}-${tag}.png`, fullPage: true });
        }
        // 展开的成员 + Key 编辑框
        await open(page, "/admin/members", PAGES[1]?.ready as never);
        await page
          .getByRole("button", { name: /展开.*的详情/ })
          .first()
          .click();
        await page.waitForTimeout(300);
        await page.screenshot({ path: `${SHOTS}/members-expanded-${tag}.png`, fullPage: true });
        await page.getByRole("button", { name: "全部设置…" }).click();
        await expect(page.getByRole("dialog", { name: /编辑：/ })).toBeVisible();
        await page.waitForTimeout(300);
        await page.screenshot({ path: `${SHOTS}/members-keydialog-${tag}.png` });
        await page.keyboard.press("Escape");
        // 大图
        await open(page, "/admin/logs/audit", PAGES[2]?.ready as never);
        const tile = page.getByRole("button", { name: /^查看 .* 的.*记录$/ }).first();
        await tile.waitFor({ state: "visible", timeout: 5000 }).catch(() => undefined);
        if (await tile.isVisible()) {
          await tile.click();
          await page.waitForTimeout(500);
          await page.screenshot({ path: `${SHOTS}/logs-lightbox-${tag}.png` });
        }
      }
    }
  });
});
