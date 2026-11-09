/**
 * AtContextDropdown — inline typeahead triggered by `@` in the chat composer.
 *
 * Cursor-style UX: typing `@` shows a compact dropdown anchored to the
 * textarea. The user can keep typing to fuzzy-filter across context kinds
 * (files, folders, instructions, commits, terminal, skills, URLs). Arrow
 * keys navigate, Enter picks, Escape dismisses.
 *
 * Two-level interaction:
 *   1. When the query is empty, show category headers the user can jump
 *      into (or just start typing to search across all).
 *   2. As the user types, results from all categories are merged and
 *      ranked by relevance.
 */

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  Bookmark,
  Brain,
  FileText,
  FolderOpen,
  GitCommit,
  Link2,
  Terminal,
  Workflow,
} from "lucide-react";
import { AgentWorkflowDefinition, CommitRow, DirEntry, rpc } from "../../ipc";
import { searchInstructions } from "../../lib/instructions-library";
import { searchSkills } from "../../lib/skills-library";
import type { LucideIcon } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

export type AtContextKind =
  | "instruction"
  | "file"
  | "folder"
  | "url"
  | "commit"
  | "terminal"
  | "skill"
  | "workflow";

export interface AtContextItem {
  kind: AtContextKind;
  /** Unique key for React rendering. */
  id: string;
  /** Primary display text. */
  label: string;
  /** Secondary hint (path, category, SHA prefix, etc.). */
  hint?: string;
  /** The value to attach (path, URL, SHA, skill name, instruction body). */
  value: string;
}

interface Props {
  /** The text after `@` that the user has typed so far. */
  filter: string;
  /** Whether the dropdown is visible. */
  visible: boolean;
  /** Repo id for file/commit lookups. `null` disables those sources. */
  repoId: string | null;
  /** Agent workflows available for inline invocation. */
  workflows?: AgentWorkflowDefinition[];
  /**
   * Called with the picked item, or `null` if the user pressed Escape
   * (dismiss without picking).
   */
  onSelect: (item: AtContextItem | null) => void;
  /**
   * Called when the user selects a category header. For file/folder,
   * the parent should narrow the inline filter. For other types, the
   * parent should open the full context picker modal on that tab.
   */
  onCategorySelect?: (kind: AtContextKind) => void;
}

// ── Category metadata ───────────────────────────────────────────────

interface CategoryMeta {
  kind: AtContextKind;
  label: string;
  Icon: LucideIcon;
  prefix: string;
}

const CATEGORIES: CategoryMeta[] = [
  { kind: "workflow", label: "Workflows", Icon: Workflow, prefix: "workflow" },
  { kind: "file", label: "Files", Icon: FileText, prefix: "file" },
  { kind: "folder", label: "Folders", Icon: FolderOpen, prefix: "folder" },
  { kind: "instruction", label: "Instructions", Icon: Bookmark, prefix: "instruction" },
  { kind: "commit", label: "Commits", Icon: GitCommit, prefix: "commit" },
  { kind: "terminal", label: "Terminal", Icon: Terminal, prefix: "terminal" },
  { kind: "skill", label: "Skills", Icon: Brain, prefix: "skill" },
  { kind: "url", label: "URL", Icon: Link2, prefix: "url" },
];

// ── Component ───────────────────────────────────────────────────────

export function AtContextDropdown({ filter, visible, repoId, workflows = [], onSelect, onCategorySelect }: Props) {
  const [activeIndex, setActiveIndex] = useState(0);
  const [kindFilter, setKindFilter] = useState<AtContextKind | null>(null);
  const listRef = useRef<HTMLDivElement>(null);

  // Fetch repo file list once (cached across filter changes).
  const [fileEntries, setFileEntries] = useState<DirEntry[]>([]);
  const [filesLoaded, setFilesLoaded] = useState(false);

  useEffect(() => {
    if (!visible || !repoId) return;
    if (filesLoaded) return;
    let cancelled = false;
    rpc.list(repoId, {}).then((r) => {
      if (!cancelled) {
        setFileEntries(r.entries);
        setFilesLoaded(true);
      }
    }).catch(() => { /* best-effort */ });
    return () => { cancelled = true; };
  }, [visible, repoId, filesLoaded]);

  // Reset file cache when repo changes.
  useEffect(() => {
    setFileEntries([]);
    setFilesLoaded(false);
  }, [repoId]);

  // Async commit search — fetches recent commits and caches them for
  // inline filtering. Loaded lazily when the commit kind filter is active
  // or when the dropdown first opens (for cross-kind search).
  const [commitEntries, setCommitEntries] = useState<CommitRow[]>([]);
  const [commitsLoaded, setCommitsLoaded] = useState(false);

  useEffect(() => {
    if (!visible || !repoId) return;
    if (commitsLoaded) return;
    // Only load commits when needed (kind filter is commit, or dropdown is
    // visible and we want cross-kind search to include commits).
    if (kindFilter !== "commit" && kindFilter !== null) return;
    let cancelled = false;
    rpc.gitLog(repoId, { max: 50, all: true })
      .then((r) => {
        if (!cancelled) {
          setCommitEntries(r.commits);
          setCommitsLoaded(true);
        }
      })
      .catch(() => { /* best-effort */ });
    return () => { cancelled = true; };
  }, [visible, repoId, commitsLoaded, kindFilter]);

  // Reset commit cache when repo changes.
  useEffect(() => {
    setCommitEntries([]);
    setCommitsLoaded(false);
  }, [repoId]);

  // Reset kind filter when dropdown closes.
  useEffect(() => {
    if (!visible) setKindFilter(null);
  }, [visible]);

  // Build the merged item list based on the current filter and kind filter.
  const items = useMemo(() => {
    const q = filter.trim().toLowerCase();

    // When the filter is empty and no kind filter is active, show category
    // headers as selectable items so the user can see what's available.
    if (!q && !kindFilter) {
      return CATEGORIES.map((cat): AtContextItem => ({
        kind: cat.kind,
        id: `cat-${cat.kind}`,
        label: cat.label,
        hint: `Type to search ${cat.label.toLowerCase()}`,
        value: "",
      }));
    }

    const results: AtContextItem[] = [];
    const MAX_PER_KIND = kindFilter ? 20 : 6;
    const MAX_TOTAL = kindFilter ? 20 : 12;

    // Workflows — first-class @ targets that invoke an agent workflow.
    if (!kindFilter || kindFilter === "workflow") {
      results.push(...fuzzyFilterWorkflows(workflows, q, MAX_PER_KIND));
    }

    // Files — only when no kindFilter or kindFilter is "file"
    if (repoId && (!kindFilter || kindFilter === "file")) {
      const fileMatches = fuzzyFilterEntries(fileEntries, "file", q, MAX_PER_KIND);
      results.push(...fileMatches);
    }

    // Folders — only when no kindFilter or kindFilter is "folder"
    if (repoId && (!kindFilter || kindFilter === "folder")) {
      const folderMatches = fuzzyFilterEntries(fileEntries, "folder", q, MAX_PER_KIND);
      results.push(...folderMatches);
    }

    // Instructions — only when no kindFilter or kindFilter is "instruction"
    if (!kindFilter || kindFilter === "instruction") {
      const instructions = searchInstructions(q).slice(0, MAX_PER_KIND);
      for (const ins of instructions) {
        results.push({
          kind: "instruction",
          id: `ins-${ins.id}`,
          label: ins.title,
          hint: ins.category || "instruction",
          value: ins.body,
        });
      }
    }

    // Skills — only when no kindFilter or kindFilter is "skill"
    if (!kindFilter || kindFilter === "skill") {
      const skills = searchSkills(q).slice(0, MAX_PER_KIND);
      for (const sk of skills) {
        results.push({
          kind: "skill",
          id: `skill-${sk.id}`,
          label: sk.name,
          hint: sk.description || "skill",
          value: sk.name,
        });
      }
    }

    // Commits — search by SHA prefix or subject when kindFilter is commit
    // or during cross-kind search.
    if (repoId && (!kindFilter || kindFilter === "commit")) {
      const commitMatches = fuzzyFilterCommits(commitEntries, q, MAX_PER_KIND);
      results.push(...commitMatches);
    }

    // URL — if the query looks like a URL, offer to attach it directly.
    if ((!kindFilter || kindFilter === "url") && (q.includes(".") || q.startsWith("http"))) {
      const url = q.startsWith("http") ? q : `https://${q}`;
      results.push({
        kind: "url",
        id: `url-${q}`,
        label: url,
        hint: "Attach URL",
        value: url,
      });
    }

    return results.slice(0, MAX_TOTAL);
  }, [filter, fileEntries, commitEntries, repoId, kindFilter, workflows]);

  // Reset active index when items change.
  useEffect(() => {
    setActiveIndex(0);
  }, [items.length, filter, visible]);

  // Scroll active item into view.
  useEffect(() => {
    if (!listRef.current) return;
    const active = listRef.current.querySelector("[data-active='true']");
    active?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  // Handle category header selection.
  const handleCategoryPick = useCallback((kind: AtContextKind) => {
    // For file/folder/instruction/commit/workflow: stay inline and narrow the
    // results to that kind so the user can filter and select.
    if (kind === "file" || kind === "folder" || kind === "instruction" || kind === "commit" || kind === "workflow") {
      setKindFilter(kind);
      setActiveIndex(0);
    } else {
      // For other types (terminal, skill, url): dismiss the dropdown
      // and signal the parent to open the full context picker modal
      // on the corresponding tab.
      onSelect(null);
      onCategorySelect?.(kind);
    }
  }, [onSelect, onCategorySelect]);

  // Keyboard navigation — attached to document so it captures from the textarea.
  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!visible) return;

      if (items.length === 0) {
        if (e.key === "Escape") {
          e.preventDefault();
          if (kindFilter) {
            // Go back to category list instead of dismissing entirely.
            setKindFilter(null);
          } else {
            onSelect(null);
          }
        }
        return;
      }

      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActiveIndex((i) => (i + 1) % items.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActiveIndex((i) => (i - 1 + items.length) % items.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const item = items[activeIndex];
        if (item && item.value) {
          onSelect(item);
        } else if (item && !item.value) {
          // Category header selected — handle based on kind.
          handleCategoryPick(item.kind);
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        if (kindFilter) {
          // Go back to category list.
          setKindFilter(null);
        } else {
          onSelect(null);
        }
      }
    },
    [visible, items, activeIndex, onSelect, kindFilter, handleCategoryPick],
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
      aria-label="Context mentions"
      className="absolute bottom-full left-0 right-0 mb-1 max-h-[min(280px,var(--composer-popup-height))] overflow-auto rounded-lg border border-gx-border bg-gx-bg shadow-lg z-50"
    >
      {/* Kind filter header — shows when narrowed to a specific type */}
      {kindFilter && (
        <div className="sticky top-0 z-10 flex items-center justify-between border-b border-gx-border/50 bg-gx-bg/95 backdrop-blur-sm px-3 py-1.5">
          <span className="text-[10px] font-medium uppercase tracking-wider text-gx-muted">
            {CATEGORIES.find((c) => c.kind === kindFilter)?.label ?? kindFilter}
          </span>
          <button
            type="button"
            onClick={() => setKindFilter(null)}
            className="text-[10px] text-gx-accent hover:underline"
          >
            ← All types
          </button>
        </div>
      )}
      {items.length === 0 ? (
        <div className="px-3 py-2 text-[11px] text-gx-muted">
          {kindFilter
            ? `No ${kindFilter}s matching "${filter}". Keep typing or press Esc to go back.`
            : `No matches for "@${filter}". Keep typing or press Esc.`}
        </div>
      ) : (
        items.map((item, i) => {
          const meta = CATEGORIES.find((c) => c.kind === item.kind);
          const Icon = meta?.Icon ?? FileText;
          const isCategory = !item.value;
          return (
            <button
              key={item.id}
              type="button"
              role="option"
              aria-selected={i === activeIndex}
              data-active={i === activeIndex}
              onClick={() => {
                if (item.value) {
                  onSelect(item);
                } else {
                  // Category header clicked — narrow or open modal.
                  handleCategoryPick(item.kind);
                }
              }}
              onMouseEnter={() => setActiveIndex(i)}
              className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${
                i === activeIndex
                  ? "bg-gx-accent/10 text-gx-text"
                  : "text-gx-text hover:bg-gx-surface-hover"
              } ${isCategory ? "cursor-pointer" : ""}`}
            >
              <Icon size={12} className="shrink-0 text-gx-accent" />
              <span className={`truncate ${isCategory ? "font-semibold text-gx-muted" : "font-medium"}`}>
                {item.label}
              </span>
              {item.hint && (
                <span className="ml-auto shrink-0 truncate text-[10px] text-gx-muted max-w-[40%]">
                  {item.hint}
                </span>
              )}
            </button>
          );
        })
      )}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function fuzzyFilterEntries(
  entries: DirEntry[],
  mode: "file" | "folder",
  query: string,
  max: number,
): AtContextItem[] {
  const pool = entries.filter((e) =>
    mode === "folder" ? e.kind === "dir" : (e.kind !== "dir" && e.kind !== "submodule"),
  );

  const scored: Array<{ entry: DirEntry; score: number }> = [];
  for (const entry of pool) {
    const path = entry.path.toLowerCase();
    const base = path.slice(path.lastIndexOf("/") + 1);
    const score = Math.max(fuzzyScore(base, query) + 500, fuzzyScore(path, query));
    if (score > 0) scored.push({ entry, score });
  }
  scored.sort((a, b) => b.score - a.score);

  return scored.slice(0, max).map((s) => ({
    kind: mode,
    id: `${mode}-${s.entry.path}`,
    label: s.entry.path.split("/").pop() ?? s.entry.path,
    hint: s.entry.path.includes("/")
      ? s.entry.path.slice(0, s.entry.path.lastIndexOf("/"))
      : mode,
    value: s.entry.path,
  }));
}

function fuzzyScore(hay: string, needle: string): number {
  let hi = 0, qi = 0, score = 0;
  let prevHit = -2;
  while (hi < hay.length && qi < needle.length) {
    if (hay[hi] === needle[qi]) {
      score += 10;
      if (prevHit === hi - 1) score += 15;
      prevHit = hi;
      qi++;
    }
    hi++;
  }
  return qi === needle.length ? score - hi : 0;
}

function fuzzyFilterCommits(
  commits: CommitRow[],
  query: string,
  max: number,
): AtContextItem[] {
  if (!query) {
    // No query — show most recent commits.
    return commits.slice(0, max).map((c) => ({
      kind: "commit" as const,
      id: `commit-${c.sha}`,
      label: c.subject ?? c.sha.slice(0, 8),
      hint: c.sha.slice(0, 7),
      value: c.sha,
    }));
  }

  const q = query.toLowerCase();
  const scored: Array<{ commit: CommitRow; score: number }> = [];

  for (const c of commits) {
    let score = 0;
    // SHA prefix match — highest priority.
    if (c.sha.startsWith(q)) {
      score = 1000;
    } else if (c.sha.includes(q)) {
      score = 500;
    }
    // Subject fuzzy match.
    const subject = (c.subject ?? "").toLowerCase();
    if (subject.includes(q)) {
      score = Math.max(score, 300);
    } else {
      const fs = fuzzyScore(subject, q);
      if (fs > 0) score = Math.max(score, fs);
    }
    // Author match.
    const author = (c.author ?? "").toLowerCase();
    if (author.includes(q)) {
      score = Math.max(score, 200);
    }

    if (score > 0) scored.push({ commit: c, score });
  }

  scored.sort((a, b) => b.score - a.score);

  return scored.slice(0, max).map((s) => ({
    kind: "commit" as const,
    id: `commit-${s.commit.sha}`,
    label: s.commit.subject ?? s.commit.sha.slice(0, 8),
    hint: s.commit.sha.slice(0, 7),
    value: s.commit.sha,
  }));
}

function fuzzyFilterWorkflows(
  workflows: AgentWorkflowDefinition[],
  query: string,
  max: number,
): AtContextItem[] {
  const q = query.toLowerCase();
  const scored: Array<{ workflow: AgentWorkflowDefinition; score: number }> = [];

  for (const workflow of workflows) {
    const haystacks = [
      "workflow",
      workflow.name,
      workflow.description ?? "",
      ...(workflow.tags ?? []),
    ].map((item) => item.toLowerCase());

    const score = q
      ? Math.max(...haystacks.map((value) =>
          value.includes(q) ? 500 + q.length : fuzzyScore(value, q),
        ))
      : 100;

    if (score > 0) scored.push({ workflow, score });
  }

  scored.sort((a, b) => b.score - a.score || b.workflow.updatedAt - a.workflow.updatedAt);

  return scored.slice(0, max).map(({ workflow }) => ({
    kind: "workflow" as const,
    id: `workflow-${workflow.id}`,
    label: workflow.name,
    hint: workflow.tags?.length ? workflow.tags.slice(0, 2).join(", ") : "workflow",
    value: workflow.id,
  }));
}
