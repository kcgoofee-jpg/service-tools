import { describe, expect, it } from "vitest";
import { buildBody, createDefaults, createPayload, formFromKey } from "./keyForm";
import type { KeyRow, OpsSnapshot } from "./types";

const key: KeyRow = {
  id: 1,
  name: "月见草",
  token: "nai-x",
  enabled: true,
  daily_images: 100,
  daily_anlas: 0,
  daily_v5: 5,
  monthly_anlas: 0,
  rpm: 10,
  allow_anlas: false,
  allow_img2img: false,
  exclude_global_v5: false,
  image_model_scope: "all",
  is_admin: false,
  is_test: false,
  quota_auto: false,
  role: "regular",
  features: null,
  expires_at: null,
  created_at: 0,
  last_used_at: null,
  used: { images: 0, anlas: 0, v5: 0, requests: 0 },
};

describe("createDefaults", () => {
  it("额度跟「新成员默认额度」对齐，缺失时用常量；每分钟请求数和有效天数留空", () => {
    const ops = {
      registration: { daily_images: 150, daily_v5: 6, expires_days: 30, features: ["image"] },
      features: { labels: {}, new_key: ["image", "tags"] },
    } as unknown as OpsSnapshot;
    const f = createDefaults(ops);
    expect([f.daily_images, f.daily_v5, f.rpm, f.expires]).toEqual(["150", "6", "", ""]);
    expect(f.feats).toEqual(["image", "tags"]);
    const d = createDefaults(null);
    expect([d.daily_images, d.daily_v5, d.rpm, d.expires]).toEqual(["100", "50", "", ""]);
  });
});

describe("buildBody", () => {
  it("手动模式：默认值能通过，0 视为不限", () => {
    const f = { ...createDefaults(null), name: " 张三 ", daily_images: "0", daily_v5: "0" };
    const r = buildBody(f);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.body.name).toBe("张三");
      expect(r.body.daily_images).toBe(0);
      expect(r.body.features).toEqual(["image"]);
      expect(createPayload(r.body)).not.toHaveProperty("quota");
    }
  });

  it("空值、负数、小数、越界都被拒绝，并指出是哪个字段", () => {
    const f = {
      ...createDefaults(null),
      daily_images: "",
      daily_v5: "-1",
      daily_anlas: "abc",
      rpm: "0",
      expires: "9999",
    };
    const r = buildBody(f);
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(Object.keys(r.errors).sort()).toEqual(
        ["daily_anlas", "daily_images", "daily_v5", "expires", "rpm"].sort(),
      );
      expect(r.errors.daily_images).toBe("每日图片张数不能为空");
    }
  });

  it("新建时不再带硬编码默认值：每分钟请求数、有效天数、文本额度都不发", () => {
    const r = buildBody(createDefaults(null));
    expect(r.ok).toBe(true);
    if (r.ok)
      for (const k of ["rpm", "expires_days", "daily_text_tokens"]) expect(r.body).not.toHaveProperty(k);
  });
  it("填了每分钟请求数 / 有效天数才发", () => {
    const r = buildBody({ ...createDefaults(null), rpm: "20", expires: "0" });
    expect(r.ok && [r.body.rpm, r.body.expires_days]).toEqual([20, 0]);
  });

  it("开通 Anlas 时日 / 月上限不能都是 0", () => {
    const f = { ...createDefaults(null), allow_anlas: true, daily_anlas: "0", monthly_anlas: "0" };
    const r = buildBody(f);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errors.allow_anlas).toContain("至少要有一个大于 0");
    expect(buildBody({ ...f, monthly_anlas: "10" }).ok).toBe(true);
  });

  it("功能：仅选中模式必须至少选一个；「全部」发 null", () => {
    expect(buildBody({ ...createDefaults(null), feats: [] }).ok).toBe(false);
    const r = buildBody({ ...createDefaults(null), featMode: "all" });
    expect(r.ok && r.body.features).toBe(null);
  });

  it("算法模式：不校验、不发额度字段", () => {
    const f = {
      ...createDefaults(null),
      quota: "auto" as const,
      daily_images: "",
      daily_anlas: "x",
      feats: [],
    };
    const r = buildBody(f);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.body.quota).toBe("auto");
      for (const k of [
        "daily_images",
        "daily_v5",
        "daily_anlas",
        "monthly_anlas",
        "allow_anlas",
        "features",
      ]) {
        expect(r.body).not.toHaveProperty(k);
      }
    }
  });

  it("有效天数留空 = 不发 expires_days（新建不设到期，编辑保持原有效期）", () => {
    const f = formFromKey(key, "regular");
    const r = buildBody(f);
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.body).not.toHaveProperty("expires_days");
    const r2 = buildBody({ ...f, expires: "0" });
    expect(r2.ok && r2.body.expires_days).toBe(0);
  });

  it("测试 Key 只能手动", () => {
    expect(formFromKey({ ...key, quota_auto: true }, "test").quota).toBe("manual");
    expect(formFromKey({ ...key, quota_auto: true }, "regular").quota).toBe("auto");
  });
});
