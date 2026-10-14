import { Megaphone } from "lucide-preact";
import { Card } from "../../ui";
import { useStore } from "./ctx";

/** 站长公告是后台存的 HTML：沙箱 iframe 里显示（脚本不会执行，也读不到本站数据）。 */
export function Announcement() {
  const { meta } = useStore();
  if (!meta.value.has_announcement) return null;
  return (
    <Card
      title={
        <span class="lp-title-ic">
          <Megaphone size={18} aria-hidden="true" /> 站长公告
        </span>
      }
      class="lp-announce"
    >
      <iframe class="lp-announce__frame" sandbox="" src="/announcement" title="站长公告" loading="lazy" />
    </Card>
  );
}
