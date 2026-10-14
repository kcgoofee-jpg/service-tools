/** 模块：每个模块回答一个问题，每 10 分钟运行一次并用独立来源交叉核对。 */
import { useMemo, useState } from "preact/hooks";
import { Button } from "../../../ui";
import { act, useAdmin } from "../Admin";
import { LoadError, Loading, SectionHead } from "./_kit";
import ModuleCard from "./modules/card";
import { failingChecks, type ModulesResponse, sortMods } from "./modules/core";
import { useBadge } from "./system/badge";
import { Sect, Switch } from "./system/forms";
import "./modules.css";

export default function Modules() {
  const { data, error, loading } = useAdmin<ModulesResponse>("/admin/api/modules");
  const [errorFirst, setErrorFirst] = useState(true);
  const [running, setRunning] = useState(false);
  const mods = useMemo(() => sortMods(data?.modules ?? [], errorFirst), [data, errorFirst]);
  useBadge("modules", data ? failingChecks(data.modules) : null);

  return (
    <Sect>
      <SectionHead
        title="模块"
        desc="每个模块回答一个问题，每 10 分钟自动运行一次，并用独立的数据来源交叉核对。关闭只停止自动调整，保护类的硬限制不受影响。"
        actions={
          <div class="actions">
            <span class="actions">
              <span id="mod-sort-label" class="small">
                报错优先
              </span>
              <Switch checked={errorFirst} labelledBy="mod-sort-label" onChange={setErrorFirst} />
            </span>
            <Button
              disabled={running}
              onClick={async () => {
                setRunning(true);
                await act("POST", "/admin/api/modules/run", undefined, "全部模块已重新运行并校验");
                setRunning(false);
              }}
            >
              {running ? "运行中…" : "立即运行全部校验"}
            </Button>
          </div>
        }
      />
      {loading && !data && <Loading rows={4} />}
      {!data && error && <LoadError error={error} />}
      {mods.map((m) => (
        <ModuleCard key={m.name} mod={m} />
      ))}
    </Sect>
  );
}
