import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Plus, Terminal, GitBranch, Shield, Bug, Search, Code } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

export interface SlashCommand {
  name: string;
  description: string;
  template: string;
  source: "builtin" | "repo" | "user";
  path?: string;
  argumentHint?: string;
  arguments?: string[];
  category?: string;
  hidden?: boolean;
  fork?: boolean;
}

interface Props {
  commands: SlashCommand[];
  filter: string;
  onSelect: (name: string) => void;
  visible: boolean;
  /** Called when the user clicks "Create command" at the bottom. */
  onCreateCommand?: (initialName?: string) => void;
}

// ── Category icons ──────────────────────────────────────────────────

const CATEGORY_ICONS: Record<string, typeof Terminal> = {
  review: Search,
  git: GitBranch,
  testing: Bug,
  fix: Bug,
  explore: Code,
  security: Shield,
};

function CategoryIcon({ category }: { category?: string }) {
  const Icon = (category && CATEGORY_ICONS[category]) || Terminal;
  return <Icon size={12} className="shrink-0 text-gx-accent" />;
}

// ── Source badge colors ─────────────────────────────────────────────

function sourceBadgeClass(source: SlashCommand["source"]): string {
  switch (source) {
    case "builtin":
      return "bg-gx-accent/15 text-gx-accent";
    case "repo":
      return "bg-gx-success/15 text-gx-success";
    case "user":
      return "bg-gx-thinking/15 text-gx-thinking";
  }
}

// ── Fuzzy matching with scoring ─────────────────────────────────────

function scoreCommand(command: SlashCommand, filter: string): number {
  if (!filter) return 1;
  const lower = filter.toLowerCase();
  const name = command.name.toLowerCase();
  const desc = command.description.toLowerCase();

  // Exact prefix match on name — highest score.
  if (name.startsWith(lower)) return 100;
  // Name contains the filter.
  if (name.includes(lower)) return 80;
  // Category match.
  if (command.category?.toLowerCase().startsWith(lower)) return 60;
  // Description contains the filter.
  if (desc.includes(lower)) return 40;
  // Fuzzy: all characters appear in order in the name.
  if (fuzzyMatch(name, lower)) return 20;

  return 0;
}

function fuzzyMatch(text: string, query: string): boolean {
  let qi = 0;
  for (let i = 0; i < text.length && qi < query.length; i++) {
    if (text[i] === query[qi]) qi++;
  }
  return qi === query.length;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Dropdown that appears above the chat input when the user types `/`.
 * Shows matching slash commands grouped by category with name, description,
 * argument hints, and source badges.
 *
 * Supports keyboard navigation (arrow keys, Enter, Escape, Tab to accept).
 */
export function SlashCommandDropdown({ commands, filter, onSelect, visible, onCreateCommand }: Props) {
  const [activeIndex, setActiveIndex] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  // Track whether the last activeIndex change came from keyboard navigation.
  // Only scroll into view on keyboard — hover should never shift the list.
  const keyboardNavRef = useRef(false);

  // Score and filter commands.
  const filtered = useMemo(() => {
    return commands
      .map((cmd) => ({ cmd, score: scoreCommand(cmd, filter) }))
      .filter((entry) => entry.score > 0)
      .sort((a, b) => b.score - a.score)
      .map((entry) => entry.cmd);
  }, [commands, filter]);

  // Group by category for display.
  const grouped = useMemo(() => {
    const groups = new Map<string, SlashCommand[]>();
    for (const cmd of filtered) {
      const cat = cmd.category || "other";
      const list = groups.get(cat) ?? [];
      list.push(cmd);
      groups.set(cat, list);
    }
    return groups;
  }, [filtered]);

  // Flat list for keyboard navigation (preserves grouped order).
  const flatList = useMemo(() => {
    const result: SlashCommand[] = [];
    for (const cmds of grouped.values()) {
      result.push(...cmds);
    }
    return result;
  }, [grouped]);

  // Reset active index when filter or visibility changes.
  useEffect(() => {
    setActiveIndex(0);
  }, [filter, visible]);

  // Scroll active item into view — only when driven by keyboard.
  useEffect(() => {
    if (!listRef.current || !keyboardNavRef.current) return;
    keyboardNavRef.current = false;
    const active = listRef.current.querySelector("[data-active='true']");
    active?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!visible) return;
      if (flatList.length === 0) {
        if (e.key === "Escape") {
          e.preventDefault();
          onSelect("");
        }
        return;
      }

      if (e.key === "ArrowDown") {
        e.preventDefault();
        keyboardNavRef.current = true;
        setActiveIndex((i) => (i + 1) % flatList.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        keyboardNavRef.current = true;
        setActiveIndex((i) => (i - 1 + flatList.length) % flatList.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        onSelect(flatList[activeIndex].name);
      } else if (e.key === "Escape") {
        e.preventDefault();
        onSelect(""); // signal dismiss
      }
    },
    [visible, flatList, activeIndex, onSelect],
  );

  // Attach keyboard listener to the document so it captures events
  // from the textarea.
  useEffect(() => {
    if (!visible) return;
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [visible, handleKeyDown]);

  if (!visible) return null;

  // Track the flat index as we render grouped items.
  let flatIdx = 0;

  return (
    <div
      ref={listRef}
      role="listbox"
      aria-label="Slash commands"
      className="absolute bottom-full left-0 right-0 mb-1 max-h-[min(240px,40vh)] overflow-auto rounded-lg border border-gx-border bg-gx-bg shadow-lg z-50"
    >
      {/* Header */}
      <div className="sticky top-0 z-10 flex items-center justify-between border-b border-gx-border/50 bg-gx-bg/95 backdrop-blur-sm px-3 py-1.5">
        <span className="text-[10px] font-medium uppercase tracking-wider text-gx-muted">
          Commands
        </span>
        {filter && (
          <span className="text-[10px] text-gx-muted">
            {flatList.length} match{flatList.length !== 1 ? "es" : ""}
          </span>
        )}
      </div>

      {flatList.length === 0 ? (
        <div className="px-3 py-3 text-center">
          <div className="text-[11px] text-gx-muted mb-2">
            No commands match "/{filter}"
          </div>
          {onCreateCommand && (
            <button
              type="button"
              onClick={() => onCreateCommand(filter)}
              className="inline-flex items-center gap-1.5 rounded-md bg-gx-accent/10 px-2.5 py-1 text-[11px] font-medium text-gx-accent hover:bg-gx-accent/20 transition"
            >
              <Plus size={11} />
              Create "/{filter}"
            </button>
          )}
        </div>
      ) : (
        <>
          {[...grouped.entries()].map(([category, cmds]) => (
            <div key={category}>
              {/* Category header — only show when there are multiple categories */}
              {grouped.size > 1 && (
                <div className="sticky top-7 z-[5] bg-gx-bg/90 px-3 py-1 text-[9px] font-semibold uppercase tracking-wider text-gx-muted/70">
                  {category}
                </div>
              )}
              {cmds.map((cmd) => {
                const idx = flatIdx++;
                return (
                  <button
                    key={cmd.name}
                    type="button"
                    role="option"
                    aria-selected={idx === activeIndex}
                    data-active={idx === activeIndex}
                    onClick={() => onSelect(cmd.name)}
                    onMouseEnter={() => setActiveIndex(idx)}
                    className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${
                      idx === activeIndex
                        ? "bg-gx-accent/10 text-gx-text"
                        : "text-gx-text hover:bg-gx-surface-hover"
                    }`}
                  >
                    <CategoryIcon category={cmd.category} />
                    <div className="min-w-0 flex-1">
                      <div className="flex items-baseline gap-1.5">
                        <span className="font-medium">/{cmd.name}</span>
                        {cmd.argumentHint && (
                          <span className="text-[10px] text-gx-muted/70 font-mono">
                            {cmd.argumentHint}
                          </span>
                        )}
                      </div>
                      <div className="text-[10px] text-gx-muted truncate">
                        {cmd.description}
                      </div>
                    </div>
                    <span
                      className={`shrink-0 rounded-full px-1.5 py-0.5 text-[9px] ${sourceBadgeClass(cmd.source)}`}
                    >
                      {cmd.source}
                    </span>
                  </button>
                );
              })}
            </div>
          ))}

          {/* Create command footer */}
          {onCreateCommand && (
            <div className="border-t border-gx-border/50 px-3 py-1.5">
              <button
                type="button"
                onClick={() => onCreateCommand()}
                className="flex w-full items-center gap-2 rounded-md px-2 py-1 text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
              >
                <Plus size={11} />
                <span>Create custom command…</span>
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}
