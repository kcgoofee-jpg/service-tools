import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, expect, type Page, test } from "@playwright/test";
import { COOKIES_FILE, readCreds } from "./devEnv";

/** 问卷页端到端：用 Discord 会话识别身份，提交时带 Authorization: Bearer <me.key>。配置见 playwright.config.ts。 */

const member = readCreds();
const needMember = !member.member;

async function asMember(context: BrowserContext) {
  await context.addCookies([
    { name: "owl_member", value: member.member ?? "", domain: "localhost", path: "/" },
  ]);
}
async function patch(
  page: Page,
  url: RegExp,
  fn: (body: Record<string, unknown>) => Record<string, unknown>,
) {
  await page.route(url, async (route) => {
    try {
      const res = await route.fetch();
      await route.fulfill({ response: res, json: fn((await res.json()) as Record<string, unknown>) });
    } catch {
      await route.abort().catch(() => {});
    }
  });
}
const STATUS = /\/public\/status(\?.*)?$/;
const ME = /\/public\/me(\?.*)?$/;

function watchConsole(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(`${m.text()} @ ${m.location().url}`);
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
  test("看到登录入口而不是问卷；点登录前记下要回到问卷", async ({ page }) => {
    const errors = watchConsole(page);
    await patch(page, STATUS, (b) => ({ ...b, web_login: true }));
    await page.route("**/login", (r) => r.fulfill({ contentType: "text/html", body: "<p>登录</p>" }));
    await page.goto("/survey");
    await expect(page.getByRole("heading", { level: 1, name: "成员问卷" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "请先登录" })).toBeVisible();
    await expect(page.getByRole("group")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "提交问卷" })).toHaveCount(0);
    // 不再有 Key 粘贴框
    await expect(page.getByPlaceholder("nai-…")).toHaveCount(0);
    await page.getByRole("main").getByRole("link", { name: "用 Discord 登录" }).click();
    await page.waitForURL("**/login");
    expect(await page.evaluate(() => sessionStorage.getItem("owl-next"))).toBe("/survey");
    expect(errors).toEqual([]);
  });

  test("网页登录暂未开放时说明情况", async ({ page }) => {
    await patch(page, STATUS, (b) => ({ ...b, web_login: false }));
    await page.goto("/survey");
    await expect(page.getByText("网页登录暂未开放，开放后再来填写。")).toBeVisible();
    await expect(page.getByRole("link", { name: "用 Discord 登录" })).toHaveCount(0);
  });

  test("手机 375px 无横向滚动", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await patch(page, STATUS, (b) => ({ ...b, web_login: true }));
    await page.goto("/survey");
    await expect(page.getByRole("heading", { name: "请先登录" })).toBeVisible();
    expect(await noHorizontalScroll(page)).toBe(true);
  });
});

test.describe("登录成员", () => {
  test.skip(needMember, `没有成员会话文件 ${COOKIES_FILE}（先跑 cargo xtask dev）`);
  test.beforeEach(async ({ context }) => asMember(context));

  test("题目：分组有 legend，输入有 label，必答题有标注", async ({ page }) => {
    const errors = watchConsole(page);
    await page.goto("/survey");
    await expect(page.getByRole("heading", { level: 1, name: "成员问卷" })).toBeVisible();
    await expect(page.getByLabel(/1\. 你主要用哪个客户端接入/)).toBeVisible();
    const q2 = page.getByRole("group", { name: /2\. 你是否理解公益站限额的原理和初衷/ });
    const q3 = page.getByRole("group", { name: /3\. 你最希望改进哪些地方/ });
    const q4 = page.getByRole("group", { name: /4\. 如果你自己购买了 NovelAI/ });
    await expect(q2.getByRole("radio")).toHaveCount(4);
    await expect(q3.getByRole("checkbox")).toHaveCount(6);
    await expect(q4.getByRole("radio")).toHaveCount(3);
    await expect(q2).toContainText("必答");
    await expect(q4).toContainText("必答");
    await expect(q3.getByLabel("其他想法（选填）")).toBeVisible();
    // 一个会话一份：不再要求粘贴 Key
    await expect(page.getByPlaceholder("nai-…")).toHaveCount(0);
    expect(errors).toEqual([]);
  });

  test("没答必答题：逐题提示并把焦点放到第一个没答的题", async ({ page }) => {
    await page.goto("/survey");
    await page.getByRole("button", { name: "提交问卷" }).click();
    const q2 = page.getByRole("group", { name: /2\. 你是否理解/ });
    await expect(q2.getByRole("alert")).toHaveText("第 2 题还没选");
    await expect(page.getByRole("group", { name: /4\. 如果你自己购买了/ }).getByRole("alert")).toHaveText(
      "第 4 题还没选",
    );
    await expect(q2.getByRole("radio").first()).toBeFocused();
    // 选了就消掉
    await q2.getByLabel("基本理解，偶尔会有疑问").check();
    await expect(q2.getByRole("alert")).toHaveCount(0);
  });

  test("提交：Authorization 用会话里的 Key，请求体字段与旧版一致，成功后提示", async ({ page }) => {
    await page.goto("/survey");
    await page.getByLabel(/1\. 你主要用哪个客户端接入/).fill("柏宝绘 1.2");
    await page.getByLabel("基本理解，偶尔会有疑问").check();
    await page.getByLabel("稳定性（少报错、少失败）").check();
    await page.getByLabel("教程和文档（接入步骤、常见问题）").check();
    await page.getByLabel("其他想法（选填）").fill("e2e 测试，请忽略");
    await page.getByLabel("可以考虑，先了解更多").check();

    const reqPromise = page.waitForRequest("**/member/survey");
    const resPromise = page.waitForResponse("**/member/survey");
    await page.getByRole("button", { name: "提交问卷" }).click();
    const req = await reqPromise;
    expect(req.method()).toBe("POST");
    expect(await req.headerValue("authorization")).toBe(`Bearer ${member.member_key}`);
    expect(req.postDataJSON()).toEqual({
      client: "柏宝绘 1.2",
      understand: "基本理解",
      improve: ["稳定性", "教程和文档"],
      improve_other: "e2e 测试，请忽略",
      donate: "可以考虑",
    });
    expect((await resPromise).status()).toBe(200);
    await expect(page.getByRole("status").filter({ hasText: "已提交，谢谢你的反馈！" })).toBeVisible();
    await expect(page.getByRole("button", { name: "更新我的回答" })).toBeVisible();
  });

  test("服务端拒绝时显示原因（401：Key 已更换）", async ({ page }) => {
    await page.route("**/member/survey", (r) => r.fulfill({ status: 401, json: { detail: "Key 无效" } }));
    await page.goto("/survey");
    await page.getByLabel("完全理解，没问题").check();
    await page.getByLabel("暂不考虑").check();
    await page.getByRole("button", { name: "提交问卷" }).click();
    await expect(page.getByRole("alert").filter({ hasText: "重新登录" })).toBeVisible();
    await expect(page.getByRole("button", { name: "提交问卷" })).toBeEnabled();
  });

  test("已登录但还没有 Key：不显示题目，提示领取方式", async ({ page }) => {
    await patch(page, ME, (b) => ({ ...b, has_key: false, key: undefined }));
    await page.goto("/survey");
    await expect(page.getByText("你还没有 Key，所以暂时不能填写问卷")).toBeVisible();
    await expect(page.getByRole("group")).toHaveCount(0);
  });

  test("键盘：Tab 到单选项时整行有可见的焦点环", async ({ page }) => {
    await page.goto("/survey");
    await page.getByLabel(/1\. 你主要用哪个客户端接入/).focus();
    await page.keyboard.press("Tab");
    const outline = await page
      .getByLabel("完全理解，没问题")
      .evaluate((el) => getComputedStyle(el.closest("label") as HTMLElement).outlineStyle);
    expect(outline).not.toBe("none");
    // 方向键在同一题内切换
    await page.keyboard.press("ArrowDown");
    await expect(page.getByLabel("基本理解，偶尔会有疑问")).toBeChecked();
  });

  test("手机 375px 无横向滚动", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/survey");
    await expect(page.getByRole("button", { name: "提交问卷" })).toBeVisible();
    expect(await noHorizontalScroll(page)).toBe(true);
  });

  for (const scheme of ["light", "dark"] as const) {
    test(`无障碍（axe）：${scheme} 没有严重 / 致命问题`, async ({ page }) => {
      await page.emulateMedia({ colorScheme: scheme });
      await page.goto("/survey");
      await page.getByRole("button", { name: "提交问卷" }).waitFor();
      expect(await seriousViolations(page)).toEqual([]);
      // 带着错误提示再测一遍
      await page.getByRole("button", { name: "提交问卷" }).click();
      await page.getByRole("alert").first().waitFor();
      expect(await seriousViolations(page)).toEqual([]);
    });
  }
});
