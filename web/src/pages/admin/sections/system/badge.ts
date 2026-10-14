import { useEffect } from "preact/hooks";
import { navBadges, type SectionId } from "../../Admin";

/** 写 / 清除侧栏角标。值为 0、空串或 null 时清除。 */
export function setBadge(id: SectionId, value: number | string | null | undefined): void {
  const next = { ...navBadges.value };
  if (value === null || value === undefined || value === 0 || value === "") delete next[id];
  else next[id] = value;
  navBadges.value = next;
}

/** 数据变化时同步角标；离开分区时保留（角标要跨页面一直提醒，直到问题处理掉） */
export function useBadge(id: SectionId, value: number | string | null | undefined): void {
  useEffect(() => {
    setBadge(id, value);
  }, [id, value]);
}
