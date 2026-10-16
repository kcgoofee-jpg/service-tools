import { render } from "preact";
import { afterEach, describe, expect, it, vi } from "vitest";
import { type Chip, FilterBar } from "./FilterBar";

const CHIPS: Chip[] = [
  { id: "all", label: "全部成员", count: 238 },
  { id: "today", label: "今日活跃", count: 87 },
  { id: "rejected", label: "24 小时被拒过", count: 48 },
  { id: "attention", label: "需要处理", count: 2, hot: true },
];
const REASONS = [
  { tag: "传了底图", count: 4 },
  { tag: "请求过快", count: 8 },
];

let host: HTMLElement | null = null;
function mount(p: Partial<Parameters<typeof FilterBar>[0]> = {}) {
  host = document.createElement("div");
  document.body.append(host);
  const props = {
    chips: CHIPS,
    active: "all",
    onToggle: vi.fn(),
    reasons: REASONS,
    reason: null,
    onReason: vi.fn(),
    reasonsOf: "rejected",
    ...p,
  };
  render(<FilterBar {...props} />, host);
  return { props, host };
}
afterEach(() => {
  if (host) render(null, host);
  host?.remove();
  host = null;
});

const buttons = (h: HTMLElement) => [...h.querySelectorAll("button")];
const btn = (h: HTMLElement, text: string) => {
  const b = buttons(h).find((x) => x.textContent?.startsWith(text));
  if (!b) throw new Error(`没有「${text}」`);
  return b;
};

describe("FilterBar", () => {
  it("每个标签 = 名称 + 人数，都是按钮，选中的 aria-pressed", () => {
    const { host: h } = mount({ active: "today" });
    expect(buttons(h).map((b) => b.textContent)).toEqual([
      "全部成员238",
      "今日活跃87",
      "24 小时被拒过48",
      "需要处理2",
    ]);
    expect(buttons(h).map((b) => b.getAttribute("aria-pressed"))).toEqual([
      "false",
      "true",
      "false",
      "false",
    ]);
    expect(h.querySelector("select")).toBeNull();
    expect(h.querySelector("[role=group]")?.getAttribute("aria-label")).toBe("筛选成员");
  });
  it("点标签 = 把它的 id 交给上层（再点取消由上层处理）", () => {
    const { props, host: h } = mount({ active: "today" });
    btn(h, "今日活跃").click();
    expect(props.onToggle).toHaveBeenCalledWith("today");
    btn(h, "24 小时被拒过").click();
    expect(props.onToggle).toHaveBeenLastCalledWith("rejected");
  });
  it("需要处理有人时人数标成醒目色，0 时不标", () => {
    const { host: h } = mount();
    expect(btn(h, "需要处理").querySelector(".mb-n")?.className).toContain("is-hot");
    render(null, h);
    render(
      <FilterBar
        chips={CHIPS.map((c) => (c.hot ? { ...c, count: 0 } : c))}
        active="all"
        onToggle={() => {}}
        reasons={[]}
        reason={null}
        onReason={() => {}}
        reasonsOf="rejected"
      />,
      h,
    );
    expect(btn(h, "需要处理").querySelector(".mb-n")?.className).not.toContain("is-hot");
  });
  it("被拒原因只在「24 小时被拒过」选中时、紧跟在它后面出现", () => {
    const off = mount({ active: "all" });
    expect(buttons(off.host).map((b) => b.textContent)).not.toContain("传了底图4");
    render(null, off.host);
    off.host.remove();

    const { host: h } = mount({ active: "rejected" });
    expect(buttons(h).map((b) => b.textContent)).toEqual([
      "全部成员238",
      "今日活跃87",
      "24 小时被拒过48",
      "传了底图4",
      "请求过快8",
      "需要处理2",
    ]);
    // 同一行同一个横栏：原因不在另一个容器里
    expect(h.querySelectorAll(".mb-bar")).toHaveLength(1);
  });
  it("原因是第二层：点选 / 再点取消", () => {
    const { props, host: h } = mount({ active: "rejected" });
    btn(h, "传了底图").click();
    expect(props.onReason).toHaveBeenCalledWith("传了底图");
    render(null, h);
    render(<FilterBar {...props} reason="传了底图" />, h);
    expect(btn(h, "传了底图").getAttribute("aria-pressed")).toBe("true");
    expect(btn(h, "请求过快").getAttribute("aria-pressed")).toBe("false");
    btn(h, "传了底图").click();
    expect(props.onReason).toHaveBeenLastCalledWith(null);
  });
  it("没有原因可选时不画「原因」引导", () => {
    const { host: h } = mount({ active: "rejected", reasons: [] });
    expect(h.querySelector(".mb-bar__lead")).toBeNull();
  });
  it("没有「快被回收」标签（回收关闭）时横栏里就没有它", () => {
    const { host: h } = mount();
    expect(buttons(h).some((b) => b.textContent?.includes("快被回收"))).toBe(false);
  });
});
