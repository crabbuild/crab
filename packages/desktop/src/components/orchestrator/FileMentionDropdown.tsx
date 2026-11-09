/**
 * File mention dropdown — the `#file.ts` equivalent of the existing `/`
 * slash command typeahead. Triggers when the user types `#` at a word
 * boundary and surfaces matching files from the repo.
 *
 * The picking flow mirrors `SlashCommandDropdown`:
 *   - arrow keys navigate
 *   - Enter picks
 *   - Escape (or `""`) dismisses
 *
 * The actual repo listing lookup lives in the parent so this component
 * stays dumb and easy to unit-test.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { FileText } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

export interface FileMatch {
  /** Repo-relative path. */
  path: string;
  /** Display label — usually the basename. */
  label: string;
}

interface Props {
  /** Matches to render (already filtered/sorted by the parent). */
  matches: FileMatch[];
  /** `true` while the parent is fetching a fresh list. */
  loading?: boolean;
  /**
   * Called with the picked path, or an empty string if the user pressed
   * Escape. An empty string signals dismiss without picking.
   */
  onSelect: (path: string) => void;
  visible: boolean;
}

// ── Component ───────────────────────────────────────────────────────

export function FileMentionDropdown({ matches, loading, onSelect, visible }: Props) {
  const [activeIndex, setActiveIndex] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setActiveIndex(0);
  }, [matches, visible]);

  useEffect(() => {
    if (!listRef.current) return;
    const active = listRef.current.querySelector("[data-active='true']");
    active?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!visible) return;

      // If there's nothing to pick (still loading or zero matches),
      // still swallow Escape so it doesn't bubble to the parent.
      if (matches.length === 0) {
        if (e.key === "Escape") {
          e.preventDefault();
          onSelect("");
        }
        return;
      }

      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActiveIndex((i) => (i + 1) % matches.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActiveIndex((i) => (i - 1 + matches.length) % matches.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        onSelect(matches[activeIndex].path);
      } else if (e.key === "Escape") {
        e.preventDefault();
        onSelect("");
      }
    },
    [visible, matches, activeIndex, onSelect],
  );

  useEffect(() => {
    if (!visible) return;
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [visible, handleKeyDown]);

  if (!visible) return null;

  return (
    <div
      ref={listRef}
      role="listbox"
      aria-label="File mentions"
      className="absolute bottom-full left-0 right-0 mb-1 max-h-[min(240px,var(--composer-popup-height))] overflow-auto rounded-lg border border-gx-border bg-gx-bg shadow-lg z-50"
    >
      {loading && matches.length === 0 && (
        <div className="px-3 py-2 text-[11px] text-gx-muted">Searching files…</div>
      )}
      {!loading && matches.length === 0 && (
        <div className="px-3 py-2 text-[11px] text-gx-muted">No matching files</div>
      )}
      {matches.map((m, i) => {
        const dir = m.path.includes("/")
          ? m.path.slice(0, m.path.lastIndexOf("/"))
          : "";
        return (
          <button
            key={m.path}
            type="button"
            role="option"
            aria-selected={i === activeIndex}
            data-active={i === activeIndex}
            onClick={() => onSelect(m.path)}
            onMouseEnter={() => setActiveIndex(i)}
            className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${
              i === activeIndex
                ? "bg-gx-accent/10 text-gx-text"
                : "text-gx-text hover:bg-gx-surface-hover"
            }`}
          >
            <FileText size={11} className="shrink-0 text-gx-accent" />
            <span className="truncate font-medium">{m.label}</span>
            {dir && (
              <span className="truncate text-[10px] text-gx-muted">{dir}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}
