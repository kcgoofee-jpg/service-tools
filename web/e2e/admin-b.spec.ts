/**
 * 后台 B 组分区（上游 / 领 Key 与功能 / 额度与保护 / 模块 / Discord / 系统）的端到端检查：
 * 登录门、逐个分区无控制台报错 / 无障碍 / 窄屏不横向滚动、改一项设置并保存、危险操作的确认框。
 *
 * 前置：后端（`cargo xtask dev`）+ 前端都在跑，E2E_BASE 指向前端（要用 localhost，后端按 ADMIN_ALLOWED_ORIGINS 校验 Origin）：
 *   cargo xtask dev
 *   cd web && pnpm exec vite --port 5174 --strictPort
 *   E2E_BASE=http://localhost:5174 pnpm exec playwright test e2e/admin-b.spec.ts
 * 会话直接用演示数据里生成的后台 Cookie（cookies.json，见 devEnv.ts；登录在开发环境里放宽了限流，
 * 生产是 5 分钟 5 次，所以这里只在「登录门」里真登录一次，被限流就跳过）。
 * 截图（桌面 1280 / 手机 375，浅色 / 深色）写到 /tmp/owl-shots-admin-b/，不进仓库。
 *
 * 不会点发送公告 / 发测试告警 / 全站重置 / 清空原图 / 改密码 这类真正对外发消息或不可逆的按钮。
 * 用例会改设置再还原，所以同一文件里按顺序跑（共用配置是 fullyParallel）。
 */
import { mkdirSync } from "node:fs";
import AxeBuilder from "@axe-core/playwright";
import { expect, type Page, test } from "@playwright/test";
import { readCreds } from "./devEnv";

test.describe.configure({ mode: "default" });

const SHOTS = process.env.E2E_SHOTS_DIR ?? "/tmp/owl-shots-admin-b";
const creds = readCreds();
const PASSWORD = process.env.E2E_ADMIN_PASSWORD ?? creds.admin_password ?? "";
const COOKIE = process.env.E2E_ADMIN_COOKIE ?? creds.admin ?? "";

/**
 * 外壳路由现状：Admin.tsx 里分区路由写成 `${path}/*`，preact-iso 的 `/*` 要求后面至少还有一段，
 * 所以 /admin/limits 本身匹配不上、会掉到总览。等外壳改成 `/:rest*` 后设 E2E_CANONICAL_URLS=1 直接用正式地址。
 */
const SUFFIX = process.env.E2E_CANONICAL_URLS ? "" : "/-";

const SECTIONS = [
  { id: "upstream", title: "上游" },
  { id: "onboarding", title: "领 Key 与功能" },
  { id: "limits", title: "额度与保护" },
  { id: "modules", title: "模块" },
  { id: "discord", title: "Discord" },
  { id: "system", title: "系统" },
] as const;

type Collected = { errors: string[] };

/** 收集控制台错误。实时推送（/admin/api/events）断线时外壳会自动退回定时刷新，这条不算。 */
function collect(page: Page): Collected {
  const c: Collected = { errors: [] };
  page.on("console", (m) => {
    if (m.type() !== "error") return;
    if (m.location().url.includes("/admin/api/events")) return;
    c.errors.push(m.text());
  });
  page.on("pageerror", (e) => c.errors.push(String(e)));
  return c;
}

async function visit(page: Page, id: string, title: string): Promise<void> {
  await page.goto(`/admin/${id}${SUFFIX}`);
  await expect(page.getByRole("heading", { level: 1, name: title, exact: true })).toBeVisible();
  await expect(page.locator('[aria-busy="true"]')).toHaveCount(0);
  await page.waitForTimeout(400);
}

async function axeViolations(page: Page): Promise<string[]> {
  const r = await new AxeBuilder({ page }).analyze();
  return r.violations
    .filter((v) => v.impact === "serious" || v.impact === "critical")
    .map((v) => `${v.impact} ${v.id}: ${v.nodes.map((n) => n.target.join(" ")).join(" | ")}`);
}

const overflow = (page: Page) =>
  page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test.beforeAll(() => {
  mkdirSync(SHOTS, { recursive: true });
});
test.beforeEach(async ({ context, baseURL }) => {
  if (COOKIE)
    await context.addCookies([
      { name: "nai_gate_admin", value: COOKIE, url: baseURL ?? "http://localhost:5176" },
    ]);
});

test("登录门：密码错误有提示，正确密码进入后台", async ({ page, context }) => {
  await context.clearCookies();
  await page.goto("/admin");
  await page.getByLabel("后台密码").fill("definitely-wrong-password");
  await page.getByRole("button", { name: "登录" }).click();
  const alert = page.getByRole("alert");
  await expect(alert).toBeVisible();
  if ((await alert.textContent())?.includes("频繁")) test.skip(true, "登录限流（5 分钟 5 次），稍后再跑");
  await expect(alert).toContainText("密码错误");
  await page.getByLabel("后台密码").fill(PASSWORD);
  await page.getByRole("button", { name: "登录" }).click();
  const nav = page.getByRole("navigation", { name: "后台导航" });
  await expect(nav.or(page.getByRole("alert").filter({ hasText: "频繁" }))).toBeVisible();
  if (!(await nav.isVisible())) test.skip(true, "登录限流（5 分钟 5 次），稍后再跑");
  await expect(nav).toBeVisible();
});

for (const s of SECTIONS) {
  test(`${s.title}：无控制台报错、无障碍、窄屏不横向滚动，并截图`, async ({ page }) => {
    const c = collect(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.emulateMedia({ colorScheme: "light" });
    await visit(page, s.id, s.title);
    await page.screenshot({ path: `${SHOTS}/${s.id}-desktop-light.png`, fullPage: true });
    expect(await axeViolations(page), "桌面浅色").toEqual([]);

    await page.emulateMedia({ colorScheme: "dark" });
    await page.waitForTimeout(200);
    await page.screenshot({ path: `${SHOTS}/${s.id}-desktop-dark.png`, fullPage: true });
    expect(await axeViolations(page), "桌面深色").toEqual([]);

    await page.setViewportSize({ width: 375, height: 812 });
    await page.waitForTimeout(300);
    expect(await overflow(page), "手机宽度下页面出现横向滚动").toBeLessThanOrEqual(0);
    await page.screenshot({ path: `${SHOTS}/${s.id}-mobile-dark.png`, fullPage: true });
    expect(await axeViolations(page), "手机深色").toEqual([]);

    await page.emulateMedia({ colorScheme: "light" });
    await page.waitForTimeout(200);
    expect(await overflow(page)).toBeLessThanOrEqual(0);
    await page.screenshot({ path: `${SHOTS}/${s.id}-mobile-light.png`, fullPage: true });
    expect(await axeViolations(page), "手机浅色").toEqual([]);

    expect(c.errors).toEqual([]);
  });
}

test("字号手机与桌面一致（正文 / 卡片标题 / 提示）", async ({ page }) => {
  const sizes = async () =>
    page.evaluate(() => {
      const fs = (sel: string) => {
        const el = document.querySelector(sel);
        return el ? getComputedStyle(el).fontSize : null;
      };
      return [fs("h1"), fs(".card__title"), fs(".field__label"), fs(".field__hint"), fs(".btn")];
    });
  await page.setViewportSize({ width: 1280, height: 900 });
  await visit(page, "limits", "额度与保护");
  const desktop = await sizes();
  await page.setViewportSize({ width: 375, height: 812 });
  await page.waitForTimeout(300);
  expect(await sizes()).toEqual(desktop);
});

test("额度与保护：改一项运行限制并保存（不合法时不能保存）", async ({ page }) => {
  await visit(page, "limits", "额度与保护");
  const field = page.getByLabel("单阶段排队等待上限");
  const original = await field.inputValue();
  const save = page.getByRole("button", { name: "保存运行限制" });
  await expect(save).toBeDisabled();

  await field.fill("");
  await expect(page.getByText("单阶段排队等待上限不能为空")).toBeVisible();
  await expect(save).toBeDisabled();
  await field.fill("-5");
  await expect(page.getByText("不能是负数")).toBeVisible();
  await field.fill("9999");
  await expect(page.getByText("必须在 15～300 之间")).toBeVisible();
  await expect(save).toBeDisabled();

  const next = String(Number(original) === 91 ? 92 : 91);
  await field.fill(next);
  await expect(save).toBeEnabled();
  await save.click();
  await expect(page.getByText("运行限制已保存并生效")).toBeVisible();
  await expect(save).toBeDisabled();

  await page.reload();
  await expect(page.getByLabel("单阶段排队等待上限")).toHaveValue(next);
  // 还原
  await page.getByLabel("单阶段排队等待上限").fill(original);
  await page.getByRole("button", { name: "保存运行限制" }).click();
  await expect(page.getByText("运行限制已保存并生效").first()).toBeVisible();
});

test("额度与保护：每小时上限填 0 要确认；动态额度推算过程可见", async ({ page }) => {
  await visit(page, "limits", "额度与保护");
  await expect(page.getByText("全站 V5 =", { exact: false }).first()).toBeVisible();
  await expect(page.getByText("每人 V5 = 全站", { exact: false })).toBeVisible();
  const field = page.getByLabel("每个上游账号每小时上限");
  await field.fill("0");
  await page.getByRole("button", { name: "保存账号保护" }).click();
  const dialog = page.getByRole("dialog", { name: "确定关掉这项保护？" });
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "取消" }).click();
  await expect(dialog).toBeHidden();
  await page.getByRole("button", { name: "撤销修改" }).first().click();
});

test("Discord：机器人只有状态卡（离线角标）；公告发送要确认（不真发）", async ({ page }) => {
  await visit(page, "discord", "Discord");
  await expect(
    page.locator(".admin-nav__item", { hasText: "Discord" }).locator(".admin-nav__badge"),
  ).toContainText("!");
  await expect(page.getByText("机器人状态")).toBeVisible();
  await expect(page.getByText("离线", { exact: true })).toBeVisible();
  // 跑图分享设置、AI 评论、最近动作已删除（docs/v3-deviations.md #54）
  for (const gone of ["跑图分享设置", "AI 评论", "最近动作"])
    await expect(page.getByText(gone)).toHaveCount(0);

  // 发送公告：先看到确认框，取消
  await page.getByLabel("公告内容", { exact: true }).fill("测试公告，不会真的发出");
  await page.getByRole("button", { name: "发送到公告频道" }).click();
  const dialog = page.getByRole("dialog", { name: "发到公告频道？" });
  await expect(dialog).toContainText("测试公告，不会真的发出");
  await dialog.getByRole("button", { name: "取消" }).click();
  await expect(dialog).toBeHidden();
});

test("模块：列出真实模块，切换启用并还原，侧栏有失败校验角标", async ({ page }) => {
  await visit(page, "modules", "模块");
  await expect(page.getByText("8 大核心模块")).toHaveCount(0);
  const titles = await page.locator(".card__title").allTextContents();
  expect(titles.length).toBeGreaterThanOrEqual(5);
  const sw = page.getByRole("switch", { name: /^观测/ });
  await expect(sw).toHaveAttribute("aria-checked", "true");
  await sw.click();
  await expect(page.getByText("已关闭「观测」")).toBeVisible();
  await expect(sw).toHaveAttribute("aria-checked", "false");
  await sw.click();
  await expect(page.getByText("已启用「观测」")).toBeVisible();
  await expect(sw).toHaveAttribute("aria-checked", "true");
});

test("领 Key 与功能：改有效期并保存；危险操作要输入确认语", async ({ page }) => {
  await visit(page, "onboarding", "领 Key 与功能");
  const field = page.getByLabel("Key 有效期");
  const original = await field.inputValue();
  const next = String(Number(original) + 1);
  await field.fill(next);
  await page.getByRole("button", { name: "保存", exact: true }).first().click();
  await expect(page.getByText("领 Key 设置已保存")).toBeVisible();
  await page.reload();
  await expect(page.getByLabel("Key 有效期")).toHaveValue(next);
  await page.getByLabel("Key 有效期").fill(original);
  await page.getByRole("button", { name: "保存", exact: true }).first().click();
  await expect(page.getByText("领 Key 设置已保存").first()).toBeVisible();

  await page.getByRole("button", { name: "全站重置 Key" }).click();
  const dialog = page.getByRole("dialog", { name: "全站重置 Key？" });
  const confirm = dialog.getByRole("button", { name: "确认重置" });
  await expect(confirm).toBeDisabled();
  await dialog.getByLabel("请输入「确认重置」以确认").fill("确认重");
  await expect(confirm).toBeDisabled();
  await dialog.getByLabel("请输入「确认重置」以确认").fill("确认重置");
  await expect(confirm).toBeEnabled();
  await dialog.getByRole("button", { name: "取消" }).click(); // 不真的重置
  await expect(dialog).toBeHidden();
});

test("上游：令牌只显示掩码；改并发并还原；添加 Token 校验格式", async ({ page }) => {
  await visit(page, "upstream", "上游");
  await expect(page.locator(".token__name").first()).toContainText("…");
  const conc = page.getByLabel("图片并发").first();
  const original = await conc.inputValue();
  await conc.fill("9");
  await expect(page.getByText("图片并发必须在 1～4 之间")).toBeVisible();
  await conc.fill(original === "2" ? "3" : "2");
  await page.getByRole("button", { name: "保存", exact: true }).first().click();
  await expect(page.getByText("图片并发已保存")).toBeVisible();
  await page.getByLabel("图片并发").first().fill(original);
  await page.getByRole("button", { name: "保存", exact: true }).first().click();
  await expect(page.getByText("图片并发已保存").first()).toBeVisible();

  await page.getByRole("button", { name: "添加 Token" }).click();
  const dialog = page.getByRole("dialog", { name: "添加上游 Token" });
  await dialog.getByLabel("NovelAI 持久 API Token").fill("not-a-token");
  await dialog.getByRole("button", { name: "添加" }).click();
  await expect(dialog.getByRole("alert")).toContainText("应以 pst- 开头");
  await expect(dialog.getByLabel("NovelAI 持久 API Token")).toHaveAttribute("type", "password");
  await dialog.getByRole("button", { name: "取消" }).click();
});

test("系统：发布状态页事件并发更新（走确认框，标准句式）", async ({ page }) => {
  await visit(page, "system", "系统");
  const title = `测试事件 ${Date.now()}`;
  await page.getByLabel("标题").fill(title);
  // 正文含内部细节时会提醒
  await page.getByLabel("正文（可留空）").first().fill("由于数据库内存占用 2GB");
  await expect(page.getByText("好像带了内部细节")).toBeVisible();
  await page.getByLabel("正文（可留空）").first().fill("");
  await page.getByRole("button", { name: "发布事件" }).click();
  const dialog = page.getByRole("dialog", { name: "公开发布这个事件？" });
  await expect(dialog).toContainText("我们正在调查此问题。");
  await dialog.getByRole("button", { name: "发布到状态页" }).click();
  await expect(page.getByText("事件已发布")).toBeVisible();

  const item = page.getByRole("article", { name: title });
  await expect(item).toContainText("调查中");
  await item.getByLabel("发布更新：状态").selectOption("resolved");
  await item.getByRole("button", { name: "发布更新" }).click();
  await page
    .getByRole("dialog", { name: "公开发布这条更新？" })
    .getByRole("button", { name: "发布到状态页" })
    .click();
  await expect(page.getByText("更新已发布")).toBeVisible();
  await expect(page.getByRole("article", { name: title })).toContainText("此事件已解决。");
});

test("系统：改密码先在本地校验；清空原图 / 测试告警有保护（不真改、不真发）", async ({ page }) => {
  await visit(page, "system", "系统");
  await page.getByLabel("当前密码", { exact: true }).fill("whatever-current");
  await page.getByLabel("新密码", { exact: true }).fill("a-new-long-password");
  await page.getByLabel("确认新密码", { exact: true }).fill("a-different-password");
  await page.getByRole("button", { name: "修改密码" }).click();
  await expect(page.getByRole("alert").filter({ hasText: "两次输入的新密码不一致" })).toBeVisible();
  await page.getByLabel("新密码", { exact: true }).fill("short");
  await page.getByLabel("确认新密码", { exact: true }).fill("short");
  await page.getByRole("button", { name: "修改密码" }).click();
  await expect(page.getByText("新密码至少需要 12 个字符")).toBeVisible();
  // 显示 / 隐藏密码
  await page.getByRole("button", { name: "显示新密码" }).click();
  await expect(page.getByLabel("新密码", { exact: true })).toHaveAttribute("type", "text");

  // 没有原图时「清空」不可点；没配置告警渠道时「测试告警」不可点
  await expect(page.getByRole("button", { name: "清空服务器上的全部原图" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "发送测试告警" })).toBeDisabled();
});

test("领 Key 与功能：改名额 / 开放领 Key / 关功能都要先确认（取消，不真发公告）", async ({ page }) => {
  await visit(page, "onboarding", "领 Key 与功能");
  // 名额上限变化：问要不要发名额公告
  const seats = page.getByLabel("名额上限", { exact: true });
  const original = await seats.inputValue();
  await seats.fill(String(Number(original) + 1));
  await page.getByRole("button", { name: "保存", exact: true }).first().click();
  const seatDialog = page.getByRole("dialog", { name: "名额上限有变化" });
  await expect(seatDialog.getByRole("button", { name: "仅保存，不公告" })).toBeVisible();
  await expect(seatDialog.getByRole("button", { name: "保存并公告" })).toBeVisible();
  await seatDialog.getByRole("button", { name: "取消" }).click();
  await expect(seatDialog).toBeHidden();
  await page.getByRole("button", { name: "撤销修改" }).first().click();
  await expect(page.getByLabel("名额上限", { exact: true })).toHaveValue(original);

  // 开放 / 暂停领 Key：先确认，可以选不公告
  await page.getByRole("switch", { name: /开放自助领取/ }).click();
  const openDialog = page.getByRole("dialog", { name: /自助领 Key？/ });
  await expect(openDialog.getByRole("button", { name: /仅(开放|暂停)，不公告/ })).toBeVisible();
  await openDialog.getByRole("button", { name: "取消" }).click();
  await expect(openDialog).toBeHidden();

  // 功能开关：关一项再保存，要确认；取消后撤销
  const vibe = page.getByRole("switch", { name: /Vibe 编码/ });
  const wasOn = (await vibe.getAttribute("aria-checked")) === "true";
  await vibe.click();
  await page.getByRole("button", { name: "保存", exact: true }).nth(1).click();
  const featDialog = page.getByRole("dialog", { name: "确认修改功能开关？" });
  await expect(featDialog).toBeVisible();
  await featDialog.getByRole("button", { name: "取消" }).click();
  await page.getByRole("button", { name: "撤销修改" }).first().click();
  await expect(vibe).toHaveAttribute("aria-checked", String(wasOn));
});
