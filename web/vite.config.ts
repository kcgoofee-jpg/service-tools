import { resolve } from "node:path";
import preact from "@preact/preset-vite";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

// 开发时模拟网关的页面路由：/ → landing.html，/admin、/admin/* → admin.html，/survey → survey.html
function pageRoutes(): Plugin {
  return {
    name: "owl-page-routes",
    configureServer(server) {
      server.middlewares.use((req, _res, next) => {
        const url = req.url ?? "/";
        const path = url.split("?")[0] ?? "/";
        const qs = url.slice(path.length);
        if (path === "/") req.url = `/_owl/landing.html${qs}`;
        else if (path === "/survey") req.url = `/_owl/survey.html${qs}`;
        else if ((path === "/admin" || path.startsWith("/admin/")) && !path.startsWith("/admin/api/"))
          req.url = `/_owl/admin.html${qs}`;
        next();
      });
    },
  };
}

// 产物放在 /_owl/ 下：Caddy 把 /assets/* /api/* /fonts/* 路由给 workbench 容器，不能撞路径。
// 多页：首页 / 后台 / 问卷各一个入口，网关按路径返回对应的 HTML（并注入首屏数据）。
export default defineConfig({
  base: "/_owl/",
  plugins: [pageRoutes(), preact()],
  build: {
    outDir: "dist",
    assetsDir: ".",
    target: "es2023",
    sourcemap: false,
    cssCodeSplit: true,
    rollupOptions: {
      input: {
        landing: resolve(import.meta.dirname, "landing.html"),
        admin: resolve(import.meta.dirname, "admin.html"),
        survey: resolve(import.meta.dirname, "survey.html"),
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      "^/(public|admin/api|self-register|login|logout|member|v1|status|healthz|announcement|terms|privacy)":
        process.env.VITE_GATE ?? "http://127.0.0.1:18000",
    },
  },
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
