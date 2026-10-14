/** 高危操作：全站重置 Key。 */
import { useState } from "preact/hooks";
import { Button, Card, Notice, toast } from "../../../../ui";
import { act } from "../../Admin";
import { ConfirmDialog } from "../system/forms";

export default function DangerCard() {
  const [open, setOpen] = useState(false);
  return (
    <Card class="danger-zone" title="高危操作" sub="执行前会要求你原样输入确认语。">
      <div class="stack-sm">
        <p>
          <b>全站重置 Key</b>：失效所有非管理员成员的 Key，并清空 Discord 绑定，成员需要在 Discord 里重新
          /register 领取。带「邀请」标签的 Key 不受影响（保留启用状态、Discord
          绑定和标签）。用于腾出名额给新成员。
        </p>
        <div class="actions">
          <Button variant="danger" onClick={() => setOpen(true)}>
            全站重置 Key
          </Button>
        </div>
      </div>
      <ConfirmDialog
        open={open}
        title="全站重置 Key？"
        confirmLabel="确认重置"
        tone="danger"
        requireText="确认重置"
        onClose={() => setOpen(false)}
        onConfirm={async () => {
          const r = await act<{ message?: string }>(
            "POST",
            "/admin/api/ops/reset-all-keys",
            { confirm: "确认重置" },
            "",
          );
          if (r) {
            toast(r.message ?? "全站已重置");
            setOpen(false);
          }
        }}
      >
        <Notice tone="danger">
          这会失效全站所有非管理员成员的 Key 并清空 Discord 绑定，不能撤销。成员需要在 Discord 里输入
          /register 重新领取。带「邀请」标签的 Key 不受影响。
        </Notice>
      </ConfirmDialog>
    </Card>
  );
}
