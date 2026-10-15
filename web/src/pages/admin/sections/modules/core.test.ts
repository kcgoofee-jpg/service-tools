import { describe, expect, it } from "vitest";
import { type Mod, moduleState, sortMods } from "./core";

const mod = (name: string, over: Partial<Mod> & { checks?: Mod["last"]["checks"] }): Mod => ({
  name,
  title: name,
  question: "",
  enabled: true,
  hard_note: "",
  periodic: true,
  params: [],
  last: { ...(over.last ?? {}), ...(over.checks ? { checks: over.checks } : {}) },
  ...(over.enabled === false ? { enabled: false } : {}),
});
const bad = { name: "c", ok: false, detail: "", level: "bad" as const };
const warn = { name: "w", ok: true, detail: "", level: "warn" as const };

describe("模块状态", () => {
  it("关闭 / 有问题 / 注意 / 正常", () => {
    expect(moduleState(mod("a", { enabled: false }))).toBe("off");
    expect(moduleState(mod("a", { checks: [bad] }))).toBe("bad");
    expect(moduleState(mod("a", { last: { ok: false } }))).toBe("bad");
    expect(moduleState(mod("a", { checks: [warn] }))).toBe("warn");
    expect(moduleState(mod("a", {}))).toBe("ok");
  });
  it("报错优先：有问题的排前面，已关闭的沉底", () => {
    const list = [
      mod("ok", {}),
      mod("off", { enabled: false }),
      mod("bad", { checks: [bad] }),
      mod("warn", { checks: [warn] }),
    ];
    expect(sortMods(list, true).map((m) => m.name)).toEqual(["bad", "warn", "ok", "off"]);
    expect(sortMods(list, false).map((m) => m.name)).toEqual(["ok", "off", "bad", "warn"]);
  });
});
