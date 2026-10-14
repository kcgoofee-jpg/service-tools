/** 操作日志：旧名并到新名、按「谁做的 / 做什么」分组，生成筛选下拉的选项（纯函数）。 */

/** 老版本按接口路径或旧名字记的条目，显示时并到对应的名字里（历史记录不改，只改显示） */
const ALIAS: Record<string, string> = {
  "POST /gift": "奖励成员",
  "PUT /ops/economy": "节约模式（已下线）",
  节约模式: "节约模式（已下线）",
  "POST /keys/invite": "新建邀请 Key",
  创建邀请: "新建邀请 Key",
  创建邀请Key: "新建邀请 Key",
  "创建邀请 Key": "新建邀请 Key",
  网页登录: "Key 登录网页",
  "领取 Key 失败（授权回调）": "领取 Key 失败",
  "领取 Key 失败（/register）": "领取 Key 失败",
  "领取 Key（/register 直发）": "领取 Key",
  移除成员标签: "成员标签",
  取消候补: "候补",
  邀请候补: "候补",
  候补邀请过期: "候补",
  关闭Bug: "处理 Bug",
  "关闭 Bug": "处理 Bug",
  "标记 Bug 已处理": "处理 Bug",
  "全部标记 Bug 已处理": "处理 Bug",
  调整名额上限: "领 Key 名额",
  动态额度调整: "动态额度",
  动态额度应用: "动态额度",
  全站重置Key: "全站杀头重置",
  "全站重置 Key": "全站杀头重置",
};

const GROUPS: Array<[string, string[]]> = [
  [
    "成员",
    [
      "领取 Key",
      "领取 Key 失败",
      "Key 登录网页",
      "成员重置 Key（/resetkey）",
      "成员确认网络情况",
      "提交反馈（/反馈）",
      "成员使用重置券",
      "候补",
    ],
  ],
  [
    "后台 · 成员与 Key",
    [
      "新建 Key",
      "新建邀请 Key",
      "修改 Key",
      "删除 Key",
      "重置今日额度",
      "奖励成员",
      "成员标签",
      "待查：疑似小号",
      "私信成员",
      "手动绑定 Discord",
      "修改账号身份组",
      "添加黑名单",
      "移出黑名单",
      "全站杀头重置",
    ],
  ],
  [
    "后台 · 设置",
    [
      "修改开放注册设置",
      "领 Key 名额",
      "修改账号保护与排队设置",
      "修改生成记录设置",
      "自动驾驶模式",
      "处理 Bug",
      "清理日志",
      "导出客户端日志",
      "导出成员反馈",
      "清空原图",
      "登录后台",
      "节约模式（已下线）",
    ],
  ],
  [
    "系统自动",
    [
      "动态额度",
      "Anlas 自动分配",
      "Anlas 核对",
      "自动驾驶（观察）",
      "自动调整：每小时上限",
      "退群回收 Key",
      "Key 到期释放名额",
      "闲置回收 Key",
      "闲置回收前提醒",
      "每日清空自助注册",
      "闲置计时重置",
    ],
  ],
];

export const actionLabel = (a: string): string => ALIAS[a] ?? a;

export type KindGroup = { title: string; options: Array<{ value: string; label: string }> };

const collator = new Intl.Collator(["zh-Hans-CN", "en"], { numeric: true, sensitivity: "base" });

/** kinds：接口给的全部原始操作名。同一个显示名可能对应多个旧名，筛选值用「|」连起来（后端按 | 拆开）。 */
export function actionKindGroups(kinds: string[]): KindGroup[] {
  const byLabel = new Map<string, string[]>();
  for (const k of kinds) {
    const l = actionLabel(k);
    byLabel.set(l, [...(byLabel.get(l) ?? []), k]);
  }
  const used = new Set<string>();
  const pick = (title: string, labels: string[]): KindGroup | null => {
    const options = labels
      .filter((l) => byLabel.has(l))
      .map((l) => {
        used.add(l);
        return { value: (byLabel.get(l) ?? []).join("|"), label: l };
      });
    return options.length ? { title, options } : null;
  };
  const head = GROUPS.map(([t, ls]) => pick(t, ls)).filter((g): g is KindGroup => g !== null);
  // 没归类的；裸接口路径（"POST /xxx"）不列
  const rest = [...byLabel.keys()]
    .filter((l) => !used.has(l) && !/^[A-Z]+ \//.test(l))
    .sort(collator.compare);
  const other = pick("其他", rest);
  return other ? [...head, other] : head;
}
