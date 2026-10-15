import { describe, expect, it } from "vitest";
import { type RegistrationInfo, regBody, regServer, roleProblem } from "./core";

const labels = { image: "文生图", vibe: "Vibe 编码" };
const reg: RegistrationInfo = {
  configured: true,
  open: false,
  max_users: 10,
  features: ["image"],
  daily_images: 150,
  daily_v5: 6,
  image_scope: "all",
  expires_days: 30,
  role_guild: "",
  role_id: "",
  role_note: "",
  active: 8,
};

describe("注册设置表单", () => {
  it("features 为 null 表示全部已开放功能", () => {
    expect(regServer({ ...reg, features: null }, labels)).toMatchObject({
      features_mode: "all",
      feat_image: true,
      feat_vibe: true,
    });
    expect(regServer(reg, labels)).toMatchObject({
      features_mode: "custom",
      feat_image: true,
      feat_vibe: false,
    });
  });
  it("请求体只含改过的字段", () => {
    const raw = () => "";
    expect(regBody({ max_users: 12 }, raw, ["image", "vibe"], true)).toEqual({ max_users: 12 });
  });
  it("功能清单成组提交：全部 → null，自定义 → 选中的名单", () => {
    const all = (n: string) => (n === "features_mode" ? "all" : true);
    expect(regBody({ features_mode: "all" }, all, ["image", "vibe"], true)).toEqual({ features: null });
    const custom = (n: string) => (n === "features_mode" ? "custom" : n === "feat_vibe");
    expect(regBody({ feat_vibe: true }, custom, ["image", "vibe"], true)).toEqual({ features: ["vibe"] });
  });
  it("身份组两个 ID 一起提交；不公告时带 notify:false", () => {
    const raw = (n: string) => (n === "role_guild" ? " 123456789012345678 " : "987654321098765432");
    expect(regBody({ role_guild: "123456789012345678" }, raw, [], false)).toEqual({
      role_guild: "123456789012345678",
      role_id: "987654321098765432",
      notify: false,
    });
  });
  it("限定身份组必须同时填或同时留空", () => {
    expect(roleProblem("123", "")).not.toBeNull();
    expect(roleProblem("", "")).toBeNull();
    expect(roleProblem("1", "2")).toBeNull();
  });
});
