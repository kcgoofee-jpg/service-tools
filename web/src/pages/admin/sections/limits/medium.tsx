/** V5 Medium 降档：关闭 / 开启 / 自动 三选一，点选即生效；下面写清每档的意思和现在的实际状态。 */
import { useId, useState } from "preact/hooks";
import { Card } from "../../../../ui";
import { act, useAdmin } from "../../Admin";
import { LoadError, Loading } from "../_kit";
import { Kv } from "../system/forms";
import { isMediumMode, MEDIUM_MODES, type MediumMode, type MediumState, mediumStatus } from "./core";

const URL = "/admin/api/settings";
const TITLE = "V5 Medium 降档";

type Settings = { v5_medium_mode?: string; v5_medium_state?: MediumState };

export default function MediumCard() {
  const { data, error, loading } = useAdmin<Settings>(URL);
  const [saving, setSaving] = useState(false);
  const name = useId();
  if (loading && !data)
    return (
      <Card title={TITLE}>
        <Loading rows={2} />
      </Card>
    );
  if (!data)
    return (
      <Card title={TITLE}>
        <LoadError error={error ?? "没有数据"} />
      </Card>
    );

  const mode: MediumMode = isMediumMode(data.v5_medium_mode) ? data.v5_medium_mode : "off";
  const pick = async (next: MediumMode) => {
    if (next === mode || saving) return;
    setSaving(true);
    const label = MEDIUM_MODES.find((m) => m.value === next)?.label ?? next;
    await act("PUT", URL, { v5_medium_mode: next }, `V5 Medium 降档已设为「${label}」`);
    setSaving(false);
  };

  return (
    <Card
      title={TITLE}
      sub="额度紧张时让 V5 Full 请求改走 Medium，价格和免费额度按 Medium 算，仍占成员的 V5 额度。"
    >
      <div class="stack-sm">
        <div class="seg" role="radiogroup" aria-label={TITLE}>
          {MEDIUM_MODES.map((m) => (
            <label key={m.value} class="seg__opt">
              <input
                type="radio"
                name={name}
                value={m.value}
                checked={mode === m.value}
                disabled={saving}
                onChange={() => void pick(m.value)}
              />
              <span class="seg__label">{m.label}</span>
            </label>
          ))}
        </div>
        <Kv rows={MEDIUM_MODES.map((m) => [m.label, m.desc])} />
        <div class="derive">
          <div class="derive__title">现在的实际状态</div>
          <div class="derive__body">
            <Kv rows={mediumStatus(mode, data.v5_medium_state)} />
          </div>
        </div>
      </div>
    </Card>
  );
}
