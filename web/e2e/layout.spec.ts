/**
 * 排版硬规则（站长 10-15）：任何宽度下都不能出现「最后一行只剩一两个字」，同一行的控件必须等高、上下对齐。
 * 首页（游客）和后台各页，在常见的桌面 / 平板 / 手机宽度下逐一检查。
 *
 * 运行：E2E_BASE=http://localhost:5174 pnpm exec playwright test e2e/layout.spec.ts
 */
import { type Cookie, expect, type Page, test } from "@playwright/test";
import { readCreds } from "./devEnv";
import { orphans } from "./orphans";

const WIDTHS = [1440, 1280, 1024, 900, 768, 430, 390, 375];

/** 视觉上同一行、相邻（间距 < 48px）的控件：高度和顶边都要一致（误差 1.5px）。 */
async function misaligned(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const sel =
      "button:not(.switch):not(.tabs__tab), select, input:not([type=checkbox]):not([type=radio]):not([type=hidden]), .xa-seg, .copy, .field-inline";
    const els = [...document.querySelectorAll<HTMLElement>(sel)].filter((el) => {
      const r = el.getBoundingClientRect();
      if (!r.width || !r.height || getComputedStyle(el).visibility === "hidden") return false;
      // 复合控件只看外框
      if (el.parentElement?.closest(".xa-seg, .copy, .field-inline")) return false;
      return !el.closest("table, nav, aside, [role=menu], dialog:not([open])");
    });
    const name = (el: HTMLElement) =>
      (el.getAttribute("aria-label") || el.textContent || el.className)
        .trim()
        .replace(/\s+/g, " ")
        .slice(0, 16);
    const out: string[] = [];
    const rs = els.map((el) => ({ el, r: el.getBoundingClientRect() }));
    for (let i = 0; i < rs.length; i++)
      for (let j = i + 1; j < rs.length; j++) {
        const a = rs[i] as (typeof rs)[number];
        const b = rs[j] as (typeof rs)[number];
        const overlap = Math.min(a.r.bottom, b.r.bottom) - Math.max(a.r.top, b.r.top);
        if (overlap < Math.min(a.r.height, b.r.height) * 0.4) continue;
        const gap = Math.max(a.r.left, b.r.left) - Math.min(a.r.right, b.r.right);
        if (gap > 48 || gap < -2) continue;
        if (Math.abs(a.r.height - b.r.height) > 1.5 || Math.abs(a.r.top - b.r.top) > 1.5)
          out.push(
            `「${name(a.el)}」${Math.round(a.r.height)}px vs 「${name(b.el)}」${Math.round(b.r.height)}px`,
          );
      }
    return out;
  });
}

async function check(page: Page, where: string) {
  const o = await orphans(page);
  const m = await misaligned(page);
  expect.soft(o, `${where}：最后一行只剩一两个字`).toEqual([]);
  expect.soft(m, `${where}：同一行控件不齐`).toEqual([]);
}

test("首页（游客）：各宽度无孤字、控件对齐", async ({ page }) => {
  for (const w of WIDTHS) {
    await page.setViewportSize({ width: w, height: 900 });
    await page.goto("/");
    await expect(page.locator(".lp-status__text")).not.toBeEmpty();
    await check(page, `首页 ${w}px`);
  }
});

const ADMIN = [
  "/admin",
  "/admin/members",
  "/admin/logs/audit",
  "/admin/logs/usage",
  "/admin/logs/actions",
  "/admin/logs/bugs",
  "/admin/logs/feedback",
  "/admin/upstream",
  "/admin/onboarding",
  "/admin/limits",
  "/admin/modules",
  "/admin/discord",
  "/admin/system",
];

test.describe("后台", () => {
  let session: Cookie[] = [];
  test.beforeAll(async ({ browser, baseURL }) => {
    const password = process.env.E2E_ADMIN_PASSWORD ?? readCreds().admin_password;
    test.skip(!password, "没有后台密码（先跑 cargo xtask dev）");
    const ctx = await browser.newContext({ ...(baseURL ? { baseURL } : {}) });
    const r = await ctx.request.post("/admin/api/login", { data: { password } });
    expect(r.ok()).toBeTruthy();
    session = await ctx.cookies();
    await ctx.close();
  });

  for (const w of [1440, 1024, 390]) {
    test(`各页 ${w}px：无孤字、控件对齐`, async ({ page, context }) => {
      await context.addCookies(session);
      await page.setViewportSize({ width: w, height: 900 });
      for (const path of ADMIN) {
        await page.goto(path);
        await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
        await page.waitForLoadState("networkidle");
        await check(page, `${path} ${w}px`);
      }
    });
  }
});
