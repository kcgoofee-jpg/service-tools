/** 生成记录：成员生成过的图（缩略图画廊 / 大图 / 列表）+ 提示词与参数，点开大图查看，可导出某成员的全部原图。 */
import { ChevronLeft, ChevronRight, Download, ImageOff } from "lucide-preact";
import { useEffect, useState } from "preact/hooks";
import { fullTime, rel } from "../../../../lib/format";
import { Badge, Button, copyText, cx, Dialog, SearchSelect, Skeleton } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError } from "../_kit";
import { downloadFile, Empty, Pager, usePageData } from "../members/shared";
import type { Member } from "../members/types";
import {
  aspectOf,
  charsText,
  hasPicture,
  negativeText,
  paramsLine,
  parseExtra,
  statusText,
} from "./auditData";
import { kindLabel, shortModel } from "./format";
import { intParam, useQueryState } from "./query";
import type { AuditItem, AuditResp } from "./types";

const SIZES = [24, 48, 96];
type View = "grid" | "large" | "list";
const VIEWS: Array<{ id: View; label: string }> = [
  { id: "grid", label: "画廊" },
  { id: "large", label: "大图" },
  { id: "list", label: "列表" },
];

const statusTone = (s: string): "ok" | "warn" | "danger" =>
  s === "ok" ? "ok" : s === "rejected" ? "warn" : "danger";
const thumbUrl = (id: number) => `/admin/api/audit/${id}/thumb`;

/* ---------------- 复制按钮（框里的反馈写在按钮上，不依赖 toast） ---------------- */
function CopyBtn(props: { text: string; label: string }) {
  const [state, setState] = useState<"" | "ok" | "fail">("");
  return (
    <Button
      size="sm"
      onClick={async () => {
        const ok = await copyText(props.text);
        setState(ok ? "ok" : "fail");
        setTimeout(() => setState(""), 1800);
      }}
    >
      {state === "ok" ? "已复制" : state === "fail" ? "复制失败" : props.label}
    </Button>
  );
}

function PromptBlock(props: { title: string; text: string; copy?: boolean }) {
  if (!props.text) return null;
  return (
    <div class="lg-blk">
      <div class="lg-blk__h">
        <span class="small muted">{props.title}</span>
        {props.copy !== false && <CopyBtn text={props.text} label="复制" />}
      </div>
      <p class="lg-blk__t">{props.text}</p>
    </div>
  );
}

/* ---------------- 大图查看 ---------------- */
function Lightbox(props: {
  items: AuditItem[];
  index: number;
  onIndex: (i: number) => void;
  onClose: () => void;
}) {
  const it = props.items[props.index];
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement | null)?.closest("input, textarea, select")) return;
      if (e.key === "ArrowLeft" && props.index > 0) {
        e.preventDefault();
        props.onIndex(props.index - 1);
      } else if (e.key === "ArrowRight" && props.index < props.items.length - 1) {
        e.preventDefault();
        props.onIndex(props.index + 1);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [props.index, props.items.length, props.onIndex]);
  if (!it) return null;
  const ex = parseExtra(it.extra);
  const pic = hasPicture(it);
  const params = paramsLine(ex);
  const meta = [
    shortModel(it.model),
    kindLabel(it.kind),
    fullTime(it.ts),
    it.has_image ? "" : pic ? "原图已过期，这是缩略图" : "",
  ]
    .filter(Boolean)
    .join(" · ");
  const chars = charsText(ex.char_prompts);
  return (
    <Dialog
      open
      wide
      title={`${it.key_name || "（已删除的 Key）"} 的生成记录`}
      onClose={props.onClose}
      footer={
        <>
          <span class="small muted lg-lbpos">
            {props.index + 1} / {props.items.length}
          </span>
          <span class="spacer" />
          <Button
            icon
            aria-label="上一张"
            disabled={props.index <= 0}
            onClick={() => props.onIndex(props.index - 1)}
          >
            <ChevronLeft size={18} aria-hidden="true" />
          </Button>
          <Button
            icon
            aria-label="下一张"
            disabled={props.index >= props.items.length - 1}
            onClick={() => props.onIndex(props.index + 1)}
          >
            <ChevronRight size={18} aria-hidden="true" />
          </Button>
        </>
      }
    >
      <div class="lg-lb">
        <div class="lg-lb__pic">
          {pic ? (
            <img
              key={it.id}
              src={`/admin/api/audit/${it.id}/${it.has_image ? "image" : "thumb"}`}
              alt={`${it.key_name || ""} 生成的图`}
            />
          ) : (
            <div class="lg-lb__none">
              <ImageOff size={28} aria-hidden="true" />
              <span>{it.status && it.status !== "ok" ? "生成未成功（无图）" : "未记录原图"}</span>
            </div>
          )}
        </div>
        <div class="lg-lb__cap">
          <div class="row">
            <Badge tone={statusTone(it.status)}>{statusText(it.status)}</Badge>
            <span class="small muted">{meta}</span>
          </div>
          <PromptBlock title="正面提示词" text={it.prompt || "（未记录）"} copy={!!it.prompt} />
          <PromptBlock title={`角色提示词 ×${ex.char_prompts?.length ?? 0}`} text={chars} />
          <PromptBlock title="负面提示词" text={negativeText(it, ex)} />
          {params && <p class="small mono">{params}</p>}
        </div>
      </div>
    </Dialog>
  );
}

/* ---------------- 页面 ---------------- */
export function Audit() {
  const { query, set } = useQueryState();
  const page = intParam(query.page, 1);
  const per = intParam(query.per, 24, SIZES);
  const view: View = query.view === "large" || query.view === "list" ? query.view : "grid";
  const key = /^\d+(,\d+)*$/.test(query.key ?? "") ? (query.key as string) : "";
  const members = useAdmin<{ members: Member[] }>("/admin/api/members");
  const status = useAdmin<{ audit: { prompts: boolean; thumbs: boolean; retention_days: number } }>(
    "/admin/api/status",
  );

  const qs = new URLSearchParams({ page: String(page), per_page: String(per) });
  if (key) qs.set("key_id", key);
  const { data, error, pending } = usePageData<AuditResp>(`/admin/api/audit?${qs}`);
  const [zoom, setZoom] = useState<number | null>(null);
  const [exporting, setExporting] = useState(false);

  const items = data?.items ?? [];
  const au = status.data?.audit;
  const recording = !!(au && (au.prompts || au.thumbs));
  const ids = key ? key.split(",") : [];

  const exportAll = async () => {
    setExporting(true);
    // 后端的导出只认单个 Key：合并过的成员有多把 Key 时逐把导出
    for (const id of ids) await downloadFile(`/admin/api/audit/export?key_id=${id}`, `owl-audit-${id}.zip`);
    setExporting(false);
  };

  return (
    <div class="stack">
      <div class="xa-toolbar">
        <div class="field-inline">
          <span class="field-inline__k">成员</span>
          <SearchSelect
            label="成员"
            value={key}
            allLabel="全部成员"
            options={(members.data?.members ?? []).map((m) => ({
              value: (m.all_key_ids ?? [m.id]).join(","),
              label: `${m.discord?.display_name || m.name}${m.key_count > 1 ? `（${m.key_count} 把 Key）` : ""}`,
              hint: m.name,
            }))}
            onChange={(v) => set({ key: v, page: null })}
          />
        </div>
        <label class="field-inline">
          <span class="field-inline__k">视图</span>
          <select
            class="select"
            value={view}
            onChange={(e) => set({ view: e.currentTarget.value === "grid" ? null : e.currentTarget.value })}
          >
            {VIEWS.map((v) => (
              <option key={v.id} value={v.id}>
                {v.label}
              </option>
            ))}
          </select>
        </label>
        <Button
          disabled={!key || exporting}
          title={key ? "把这位成员的全部原图打包下载" : "先选择一个成员"}
          onClick={exportAll}
        >
          <Download size={15} aria-hidden="true" />
          导出该成员全部
        </Button>
      </div>

      {error && !data ? (
        <LoadError error={error} />
      ) : !data ? (
        <Skeleton h="320px" />
      ) : items.length === 0 ? (
        <div class="card">
          <Empty title={key ? "该成员暂无生成记录" : "暂无生成记录"}>
            {au
              ? recording
                ? `生成记录功能已开启（提示词${au.prompts ? "开" : "关"}、原图${au.thumbs ? "开（按容量清理）" : "关"}），新的生成请求会出现在这里。${au.retention_days > 0 ? `记录会在 ${au.retention_days} 天后自动清除。` : "记录长期保存。"}`
                : "生成记录功能当前已关闭，不会保存提示词和原图。开启后，记录会在保留期满后自动清除。"
              : ""}
          </Empty>
        </div>
      ) : (
        <div class={cx("lg-auditwrap", pending && "xa-busy")} aria-busy={pending}>
          {view === "list" ? (
            <ul class="lg-alist" aria-label="生成记录">
              {items.map((it, i) => {
                const ex = parseExtra(it.extra);
                const pic = hasPicture(it);
                const params = paramsLine(ex);
                return (
                  <li key={it.id} class="card lg-aitem">
                    <button
                      type="button"
                      class="lg-thumb"
                      style={{ aspectRatio: String(aspectOf(ex)) }}
                      aria-label={`放大查看 ${it.key_name} 的记录`}
                      onClick={() => setZoom(i)}
                    >
                      {pic ? (
                        <img src={thumbUrl(it.id)} alt="" loading="lazy" decoding="async" />
                      ) : (
                        <ImageOff size={22} aria-hidden="true" />
                      )}
                    </button>
                    <div class="lg-aitem__b">
                      <div class="row">
                        <b>{it.key_name}</b>
                        <Badge tone={statusTone(it.status)}>{statusText(it.status)}</Badge>
                        <span class="small muted" title={fullTime(it.ts)}>
                          {shortModel(it.model) || "—"} · {kindLabel(it.kind)} · {rel(it.ts)}
                        </span>
                      </div>
                      {it.prompt ? (
                        <details class="lg-prompt">
                          <summary>{it.prompt}</summary>
                          <div class="lg-prompt__full">
                            <PromptBlock title="正面提示词" text={it.prompt} />
                            <PromptBlock
                              title={`角色提示词 ×${ex.char_prompts?.length ?? 0}`}
                              text={charsText(ex.char_prompts)}
                            />
                            <PromptBlock title="负面提示词" text={negativeText(it, ex)} />
                          </div>
                        </details>
                      ) : (
                        <p class="small muted">未记录提示词</p>
                      )}
                      {params && <p class="small mono muted">{params}</p>}
                    </div>
                  </li>
                );
              })}
            </ul>
          ) : (
            <ul class={cx("lg-gallery", view === "large" && "is-large")} aria-label="生成记录">
              {items.map((it, i) => {
                const pic = hasPicture(it);
                return (
                  <li key={it.id}>
                    <button
                      type="button"
                      class={cx("lg-tile", !pic && "is-none")}
                      aria-label={`查看 ${it.key_name} 的${statusText(it.status)}记录`}
                      onClick={() => setZoom(i)}
                    >
                      {pic ? (
                        <img src={thumbUrl(it.id)} alt="" loading="lazy" decoding="async" />
                      ) : (
                        <ImageOff size={26} aria-hidden="true" />
                      )}
                      {it.status !== "ok" && (
                        <span class="lg-tile__st">
                          <Badge tone={statusTone(it.status)}>{statusText(it.status)}</Badge>
                        </span>
                      )}
                      <span class="lg-tile__who">{it.key_name}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
          <Pager
            page={data.page}
            pages={data.pages}
            total={data.total}
            perPage={data.per_page}
            sizes={SIZES}
            onPage={(p) => set({ page: p === 1 ? null : String(p) })}
            onSize={(n) => set({ per: n === 24 ? null : String(n), page: null })}
          />
        </div>
      )}

      {zoom !== null && items[zoom] && (
        <Lightbox items={items} index={zoom} onIndex={setZoom} onClose={() => setZoom(null)} />
      )}
    </div>
  );
}
