/**
 * 可输入搜索的选择框：成员 / Key 很多时，原生 <select> 一展开就是几百行，没法用。
 * 点开或直接打字就按「名字 + 备注（如 @用户名）」过滤，最多列 50 项；↑↓ 选择、回车确认、Esc 关闭。
 * 空值 = 「全部」（allLabel），始终排在第一项。
 */
import { X } from "lucide-preact";
import { useEffect, useId, useMemo, useRef, useState } from "preact/hooks";

export type SearchOption = { value: string; label: string; hint?: string };

/** 一次最多列出的条数：再多也翻不动，让人继续输入缩小范围 */
export const SEARCH_LIMIT = 50;

/** 空格分开的每个词都要在「名字 + 备注」里出现（不分大小写） */
export function filterOptions(options: SearchOption[], query: string): SearchOption[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return options;
  return options.filter((o) => {
    const hay = `${o.label} ${o.hint ?? ""}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

export function SearchSelect(props: {
  value: string;
  onChange: (value: string) => void;
  options: SearchOption[];
  /** 空值那一项的文字，也是输入框里的占位，例如「全部成员」 */
  allLabel: string;
  /** 读屏用的名字 */
  label: string;
}) {
  const { value, onChange, options, allLabel } = props;
  const id = useId();
  const listId = `${id}-list`;
  const root = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);

  const selected = options.find((o) => o.value === value);
  const matches = useMemo(() => filterOptions(options, query), [options, query]);
  const shown = matches.slice(0, SEARCH_LIMIT);
  // 第一项永远是「全部」；有搜索词时它排在结果前面，方便一键清掉筛选
  const rows: SearchOption[] = [{ value: "", label: allLabel }, ...shown];
  const more = matches.length - shown.length;

  const close = () => {
    setOpen(false);
    setQuery("");
  };
  const choose = (v: string) => {
    onChange(v);
    close();
    input.current?.blur();
  };

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) close();
    };
    document.addEventListener("pointerdown", onDoc);
    return () => document.removeEventListener("pointerdown", onDoc);
  }, [open]);

  // 当前项滚进可视区
  useEffect(() => {
    if (!open) return;
    root.current?.querySelector(`#${CSS.escape(`${id}-o${active}`)}`)?.scrollIntoView?.({ block: "nearest" });
  }, [open, active, id]);

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!open) {
        setOpen(true);
        return;
      }
      const n = rows.length;
      setActive((a) => (a + (e.key === "ArrowDown" ? 1 : -1) + n) % n);
    } else if (e.key === "Enter") {
      if (open) {
        e.preventDefault();
        const row = rows[active];
        if (row) choose(row.value);
      }
    } else if (e.key === "Escape") {
      if (open) {
        e.preventDefault();
        close();
      }
    } else if (e.key === "Tab") {
      close();
    }
  };

  return (
    <div class="ss" ref={root}>
      <input
        ref={input}
        class="input ss__input"
        type="text"
        role="combobox"
        aria-label={props.label}
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={open ? `${id}-o${active}` : undefined}
        autocomplete="off"
        spellcheck={false}
        placeholder={allLabel}
        value={open ? query : (selected?.label ?? "")}
        onFocus={() => {
          setOpen(true);
          setActive(0);
        }}
        onInput={(e) => {
          setOpen(true);
          setQuery(e.currentTarget.value);
          setActive(0);
        }}
        onKeyDown={onKey}
      />
      {value && !open && (
        <button
          type="button"
          class="ss__clear"
          aria-label={`清除，回到${allLabel}`}
          onClick={() => onChange("")}
        >
          <X size={14} aria-hidden="true" />
        </button>
      )}
      {open && (
        <div class="ss__list" id={listId} role="listbox" aria-label={props.label}>
          {rows.map((o, i) => (
            <div
              key={o.value || "__all"}
              id={`${id}-o${i}`}
              role="option"
              tabIndex={-1}
              aria-selected={o.value === value}
              class={`ss__opt${i === active ? " is-active" : ""}${o.value === value ? " is-selected" : ""}`}
              // 用 mousedown：输入框失焦之前就选中，不会被「点外面关闭」吃掉
              onMouseDown={(e) => {
                e.preventDefault();
                choose(o.value);
              }}
              onMouseEnter={() => setActive(i)}
            >
              <span class="ss__label">{o.label}</span>
              {o.hint && <span class="ss__hint">{o.hint}</span>}
            </div>
          ))}
          {shown.length === 0 && query.trim() !== "" && (
            <div class="ss__note">没有匹配「{query.trim()}」的项</div>
          )}
          {more > 0 && <div class="ss__note">还有 {more} 项，继续输入缩小范围</div>}
        </div>
      )}
    </div>
  );
}
