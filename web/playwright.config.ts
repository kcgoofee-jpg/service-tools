import { defineConfig, devices } from "@playwright/test";

/**
 * 端到端测试：连的是已经在跑的前端（默认 Vite 开发服务 http://localhost:5174），不在这里启动服务。
 *
 *   tools/dev/run-legacy.sh /tmp/owl-dev-landing        # 旧 Python 网关 + 合成数据，写出 cookies.json（成员会话）
 *   cd web && VITE_GATE=http://127.0.0.1:18100 pnpm exec vite --port 5174 --strictPort
 *   E2E_BASE=http://localhost:5174 pnpm exec playwright test
 *
 * E2E_COOKIES 可改成员会话文件的位置（默认 /tmp/owl-dev-landing/cookies.json）；没有这个文件时成员相关用例自动跳过。
 */
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: process.env.E2E_BASE ?? "http://localhost:5174",
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
