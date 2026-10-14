/** 生成记录：成员生成过的图（缩略图画廊 / 大图 / 列表）+ 提示词与参数，点开大图查看，可导出某成员的全部原图。 */
import { ChevronLeft, ChevronRight, Download, ImageOff, Settings2 } from "lucide-preact";
import { useEffect, useId, useRef, useState } from "preact/hooks";
import { getJSON } from "../../../../lib/api";
import { fullTime, rel } from "../../../../lib/format";
import { Badge, Button, copyText, cx, Dialog, Field, Notice, Skeleton, toast } from "../../../../ui";
import { useAdmin } from "../../Admin";
import { LoadError } from "../_kit";
import { call } from "../members/actions";
import {
  ConfirmDialog,
  downloadFile,
  Empty,
  Pager,
  useBusy,
  useCloseGuard,
  usePageData,
} from "../members/shared";
import type { Member, OpsSnapshot } from "../members/types";
import { parseNum } from "../members/validate";
import {
  aspectOf,
  charsText,
  gb,
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

/* ---------------- 记录设置 ---------------- */
function AuditSettings(props: { onClose: () => void; onSaved: () => void }) {
  useBusy();
  const [cfg, setCfg] = useState<OpsSnapshot["audit"] | null>(null);
  const [loadErr, setLoadErr] = useState("");
  const [f, setF] = useState<{
    prompts: boolean;
    thumbs: boolean;
    download: boolean;
    keep: string;
    days: string;
    cap: string;
  } | null>(null);
  const [errs, setErrs] = useState<Record<string, string>>({});
  const [formErr, setFormErr] = useState("");
  const [pending, setPending] = useState(false);
  const [purge, setPurge] = useState(false);
  const formId = useId();
  const initial = useRef("");

  useEffect(() => {
    getJSON<OpsSnapshot>("/admin/api/ops")
      .then((o) => {
        const a = o.audit;
        setCfg(a);
        const next = {
          prompts: a.prompts,
          thumbs: a.thumbs,
          download: a.image_retention_days > 0,
          keep: String(a.image_keep_hours ?? 24),
          days: String(a.retention_days),
          cap: String(a.image_cap_mb ?? 8192),
        };
        initial.current = JSON.stringify(next);
        setF(next);
      })
      .catch((e) => setLoadErr(e instanceof Error ? e.message : String(e)));
  }, [initial]);

  const dirty = f !== null && JSON.stringify(f) !== initial.current;
  const guard = useCloseGuard(dirty, props.onClose);
  const set = <K extends keyof NonNullable<typeof f>>(k: K, v: NonNullable<typeof f>[K]) =>
    setF((x) => (x ? { ...x, [k]: v } : x));

  const save = async () => {
    if (!f || pending) return;
    const days = parseNum(f.days, "提示词保留天数", { max: 3650 });
    const cap = parseNum(f.cap, "原图容量上限", { max: 500000 });
    const keep = parseNum(f.keep, "原图保存小时数", { max: 8760 });
    const e: Record<string, string> = {};
    if (!days.ok) e.days = days.error;
    if (!cap.ok) e.cap = cap.error;
    if (!keep.ok) e.keep = keep.error;
    setErrs(e);
    if (!days.ok || !cap.ok || !keep.ok) {
      setFormErr("有填写不正确的地方，请按提示修改");
      return;
    }
    setFormErr("");
    setPending(true);
    const r = await call("PUT", "/admin/api/ops/audit", {
      prompts: f.prompts,
      thumbs: f.thumbs,
      retention_days: days.value,
      image_cap_mb: cap.value,
      image_keep_hours: keep.value,
      member_download: f.download,
    });
    setPending(false);
    if (!r.ok) return setFormErr(r.error);
    props.onSaved();
  };

  const err = (k: string) =>
    errs[k] ? (
      <span class="field__err" role="alert">
        {errs[k]}
      </span>
    ) : undefined;
  return (
    <>
      <Dialog
        key={guard.gen}
        open
        title="生成记录设置"
        onClose={guard.tryClose}
        footer={
          <>
            <Button onClick={guard.tryClose}>取消</Button>
            <Button variant="primary" type="submit" form={formId} disabled={!f || pending}>
              {pending ? "保存中…" : "保存"}
            </Button>
          </>
        }
      >
        {guard.asking && (
          <div class="mb-ask">
            <Notice tone="warn">
              <span>有未保存的修改，确定放弃吗？</span>
              <span class="spacer" />
              <Button size="sm" onClick={guard.keep}>
                继续编辑
              </Button>
              <Button size="sm" variant="danger" onClick={guard.discard}>
                放弃修改
              </Button>
            </Notice>
          </div>
        )}
        {loadErr ? (
          <Notice tone="danger">读取设置失败：{loadErr}</Notice>
        ) : !f || !cfg ? (
          <Skeleton h="160px" />
        ) : (
          <form
            id={formId}
            class="xa-form"
            noValidate
            onSubmit={(e) => {
              e.preventDefault();
              void save();
            }}
          >
            <div class="xa-form__opts">
              <label class="xa-check">
                <input
                  type="checkbox"
                  checked={f.prompts}
                  onChange={(e) => set("prompts", e.currentTarget.checked)}
                />
                <span>记录提示词</span>
              </label>
              <label class="xa-check">
                <input
                  type="checkbox"
                  checked={f.thumbs}
                  onChange={(e) => set("thumbs", e.currentTarget.checked)}
                />
                <span>
                  保存原图<small>到保存时长就删，另外超过容量上限时删最旧的</small>
                </span>
              </label>
              <label class="xa-check">
                <input
                  type="checkbox"
                  checked={f.download}
                  onChange={(e) => set("download", e.currentTarget.checked)}
                />
                <span>
                  成员打包下载原图
                  <small>
                    开了以后，成员在首页能打包自己最近 2 天的原图（受保存时长限制），用来找回误删的图
                  </small>
                </span>
              </label>
            </div>
            <div class="xa-form__grid">
              <Field label="原图保存小时数" hint={err("keep") ?? "到点自动删；0 = 只按容量上限删"}>
                <input
                  class="input"
                  type="number"
                  min={0}
                  max={8760}
                  value={f.keep}
                  aria-invalid={errs.keep ? true : undefined}
                  onInput={(e) => set("keep", e.currentTarget.value)}
                />
              </Field>
              <Field label="提示词保留天数" hint={err("days") ?? "0 = 长期保留"}>
                <input
                  class="input"
                  type="number"
                  min={0}
                  max={3650}
                  value={f.days}
                  aria-invalid={errs.days ? true : undefined}
                  onInput={(e) => set("days", e.currentTarget.value)}
                />
              </Field>
              <Field
                label="原图容量上限（MB）"
                hint={
                  err("cap") ??
                  `现在已用 ${gb(cfg.image_bytes || 0)}（约 1.3MB/张，存成文件）。超过后删最旧的；0 = 不限`
                }
              >
                <input
                  class="input"
                  type="number"
                  min={0}
                  max={500000}
                  value={f.cap}
                  aria-invalid={errs.cap ? true : undefined}
                  onInput={(e) => set("cap", e.currentTarget.value)}
                />
              </Field>
            </div>
            {formErr && (
              <p class="xa-formerr" role="alert">
                {formErr}
              </p>
            )}
            <div class="lg-danger">
              <Button variant="danger" onClick={() => setPurge(true)}>
                清空服务器上的全部原图
              </Button>
              <span class="small muted">只删原图，提示词和参数保留；成员就打包不到了</span>
            </div>
          </form>
        )}
      </Dialog>
      {purge && (
        <ConfirmDialog
          title="清空全部原图"
          confirmText="清空"
          danger
          onClose={() => setPurge(false)}
          onConfirm={async () => {
            const r = await call<{ purged: number }>("POST", "/admin/api/audit/purge-images", {
              confirm: "清空",
            });
            if (!r.ok) return r.error;
            toast(`已清空 ${r.data.purged} 张原图`);
            return undefined;
          }}
        >
          <p>确定清空服务器上的全部原图？只删原图，提示词和参数保留。删掉后成员就打包不到了，不能恢复。</p>
        </ConfirmDialog>
      )}
    </>
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
  const [settings, setSettings] = useState(false);
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
        <label class="lg-ctl">
          <span class="small muted">成员</span>
          <select
            class="select"
            value={key}
            onChange={(e) => set({ key: e.currentTarget.value, page: null })}
          >
            <option value="">全部成员</option>
            {(members.data?.members ?? []).map((m) => {
              const v = (m.all_key_ids ?? [m.id]).join(",");
              return (
                <option key={v} value={v}>
                  {m.discord?.display_name || m.name}
                  {m.key_count > 1 ? `（${m.key_count} 把 Key）` : ""}
                </option>
              );
            })}
          </select>
        </label>
        <label class="lg-ctl">
          <span class="small muted">视图</span>
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
        <span class="spacer" />
        {au &&
          (recording ? (
            <Badge tone="ok" dot>
              记录中
            </Badge>
          ) : (
            <Badge>记录已关闭</Badge>
          ))}
        <Button onClick={() => setSettings(true)}>
          <Settings2 size={15} aria-hidden="true" />
          记录设置
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
      {settings && (
        <AuditSettings
          onClose={() => setSettings(false)}
          onSaved={() => {
            setSettings(false);
            toast("生成记录设置已保存");
          }}
        />
      )}
    </div>
  );
}
