/** 问卷题目与提交格式（与旧版 survey.html / POST /member/survey 一致：选项的 value 就是后台看到的文字）。 */

export type Choice = { value: string; label: string };

export const UNDERSTAND: Choice[] = [
  { value: "完全理解", label: "完全理解，没问题" },
  { value: "基本理解", label: "基本理解，偶尔会有疑问" },
  { value: "不太理解", label: "不太理解，希望有更清楚的说明" },
  { value: "没注意过", label: "说实话没注意过这件事" },
];

export const IMPROVE: Choice[] = [
  { value: "报错提示更清楚", label: "报错提示更清楚（一眼知道为什么被拒、该等多久）" },
  { value: "排队和等待体验", label: "排队和等待体验（能看到排第几、预计时间更准）" },
  { value: "教程和文档", label: "教程和文档（接入步骤、常见问题）" },
  { value: "稳定性", label: "稳定性（少报错、少失败）" },
  { value: "网页功能", label: "网页功能（额度查询、状态页、反馈入口）" },
  { value: "客户端接入便利性", label: "客户端接入便利性（填地址就能用，少踩坑）" },
];

export const DONATE: Choice[] = [
  { value: "愿意捐V4.5", label: "愿意捐 V4.5 额度，站长可以联系我" },
  { value: "可以考虑", label: "可以考虑，先了解更多" },
  { value: "暂不考虑", label: "暂不考虑" },
];

export type Answers = {
  client: string;
  understand: string;
  improve: string[];
  improveOther: string;
  donate: string;
};

export const EMPTY: Answers = { client: "", understand: "", improve: [], improveOther: "", donate: "" };

/** 必答题没选的提示（题号与页面上一致）。 */
export function validate(a: Answers): { understand?: string; donate?: string } {
  const errs: { understand?: string; donate?: string } = {};
  if (!a.understand) errs.understand = "第 2 题还没选";
  if (!a.donate) errs.donate = "第 4 题还没选";
  return errs;
}

/** 提交给 /member/survey 的 JSON（字段名沿用旧版）。 */
export function payload(a: Answers) {
  return {
    client: a.client.trim(),
    understand: a.understand,
    improve: a.improve,
    improve_other: a.improveOther.trim(),
    donate: a.donate,
  };
}
