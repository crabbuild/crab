import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
/**
 * ContextPicker — unified context attachment dialog for the chat composer.
 *
 * Supports multiple context kinds, each with a dedicated search/list UI:
 *   - Instruction library (global prompts; shown first)
 *   - File
 *   - Folder
 *   - URL
 *   - Commit (git log search)
 *   - Terminal output (from the live terminal registry)
 *   - Skill (installed via GitHub URL)
 *
 * The dialog is keyboard-driven: arrow keys navigate the list, Enter
 * selects, Esc dismisses, `/` jumps between kinds.
 */

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import clsx from "clsx";
import {
  AtSign,
  Bookmark,
  Brain,
  FileText,
  FolderOpen,
  GitCommit,
  Link2,
  Pin,
  PinOff,
  Plus,
  RefreshCcw,
  Search,
  Sparkles,
  Terminal,
  Trash2,
  X,
} from "lucide-react";
import { DirEntry, rpc, CommitRow } from "../../ipc";
import {
  Instruction,
  createInstruction,
  deleteInstruction,
  recordUsage,
  searchInstructions,
  togglePinned,
  updateInstruction,
} from "../../lib/instructions-library";
import {
  Skill,
  installSkillFromUrl,
  listSkills,
  removeSkill,
  searchSkills,
} from "../../lib/skills-library";
import {
  TerminalSession,
  subscribe as subscribeTerminals,
  terminalTail,
} from "../../lib/terminal-registry";
import { publishToast } from "../../lib/toasts";
import type { ChatAttachment } from "./ChatInput";

import type { LucideIcon } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

export type ContextKind =
  | "instruction"
  | "file"
  | "folder"
  | "url"
  | "commit"
  | "terminal"
  | "skill";

export interface ContextPickerResult {
  /** The attachment to add to the composer (may be null for instructions). */
  attachment?: ChatAttachment;
  /** Optional text to insert into the textarea (used by instructions). */
  textToInsert?: string;
}

interface Props {
  open: boolean;
  /** Initial kind to show. If absent, defaults to "instruction". */
  initialKind?: ContextKind;
  /**
   * Seed the search box when the picker opens. Used by the `@` trigger
   * in the chat composer so the user's partial query carries over into
   * the picker instead of forcing them to retype.
   */
  initialQuery?: string;
  /** Repo id for file/commit lookups. `null` disables those tabs. */
  repoId: string | null;
  onClose: () => void;
  onPick: (result: ContextPickerResult) => void;
}

interface KindMeta {
  kind: ContextKind;
  label: string;
  Icon: LucideIcon;
  description: string;
}

const KINDS: KindMeta[] = [
  { kind: "instruction", label: "Instructions", Icon: Bookmark, description: "Your saved prompt library" },
  { kind: "file", label: "Files", Icon: FileText, description: "Pick files from the repo" },
  { kind: "folder", label: "Folders", Icon: FolderOpen, description: "Pick a folder from the repo" },
  { kind: "url", label: "URL", Icon: Link2, description: "Attach a web page" },
  { kind: "commit", label: "Commits", Icon: GitCommit, description: "Reference a commit SHA" },
  { kind: "terminal", label: "Terminal", Icon: Terminal, description: "Attach terminal output" },
  { kind: "skill", label: "Skills", Icon: Brain, description: "Install & reuse skills" },
];

// ── Main component ─────────────────────────────────────────────────

export function ContextPicker({ open, initialKind = "instruction", initialQuery = "", repoId, onClose, onPick }: Props) {
  const [kind, setKind] = useState<ContextKind>(initialKind);
  const [query, setQuery] = useState(initialQuery);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!open) return;
    setKind(initialKind);
    setQuery(initialQuery);
    queueMicrotask(() => inputRef.current?.focus());
  }, [open, initialKind, initialQuery]);

  // Global key handling (Esc closes).
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose]);

  if (!open) return null;

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/50 pt-[8vh]"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label="Add context"
    >
      <div
        className="flex h-[78vh] w-[min(680px,94vw)] flex-col overflow-hidden rounded-lg border border-border bg-background shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Kind tabs */}
        <div className="flex shrink-0 items-center gap-0.5 overflow-x-auto border-b border-border px-2 py-1">
          {KINDS.map((k) => {
            const active = k.kind === kind;
            return (
              <button
                key={k.kind}
                type="button"
                onClick={() => {
                  setKind(k.kind);
                  setQuery("");
                }}
                className={clsx(
                  "flex shrink-0 items-center gap-1.5 rounded-md px-2.5 py-1 text-xs transition",
                  active
                    ? "bg-primary/15 text-foreground"
                    : "text-muted-foreground hover:bg-accent hover:text-foreground",
                )}
                title={k.description}
                aria-pressed={active}
              >
                <k.Icon size={12} />
                {k.label}
              </button>
            );
          })}
          <div className="flex-1" />
          <button
            type="button"
            onClick={onClose}
            className="rounded p-1 text-muted-foreground hover:text-foreground"
            aria-label="Close"
          >
            <X size={14} />
          </button>
        </div>

        {/* Kind-specific panel */}
        <div className="min-h-0 flex-1 overflow-hidden">
          {kind === "instruction" && (
            <InstructionPanel
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              onPick={(ins) => {
                recordUsage(ins.id);
                onPick({ textToInsert: ins.body });
                onClose();
              }}
            />
          )}
          {kind === "file" && (
            <FileOrFolderPanel
              mode="file"
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              repoId={repoId}
              onPick={(entry) => {
                onPick({
                  attachment: {
                    id: `file-${Date.now()}`,
                    kind: "file",
                    value: entry.path,
                    label: entry.path,
                  },
                });
                onClose();
              }}
            />
          )}
          {kind === "folder" && (
            <FileOrFolderPanel
              mode="folder"
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              repoId={repoId}
              onPick={(entry) => {
                onPick({
                  attachment: {
                    id: `folder-${Date.now()}`,
                    kind: "folder",
                    value: entry.path,
                    label: entry.path + "/",
                  },
                });
                onClose();
              }}
            />
          )}
          {kind === "url" && (
            <UrlPanel
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              onPick={(url) => {
                const label = (() => {
                  try { return new URL(url).hostname.replace(/^www\./, ""); } catch { return url; }
                })();
                onPick({
                  attachment: {
                    id: `url-${Date.now()}`,
                    kind: "url",
                    value: url,
                    label,
                  },
                });
                onClose();
              }}
            />
          )}
          {kind === "commit" && (
            <CommitPanel
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              repoId={repoId}
              onPick={(commit) => {
                onPick({
                  attachment: {
                    id: `commit-${Date.now()}`,
                    kind: "commit",
                    value: commit.sha,
                    label: `${commit.sha.slice(0, 7)} ${commit.subject.slice(0, 40)}`,
                  },
                });
                onClose();
              }}
            />
          )}
          {kind === "terminal" && (
            <TerminalPanel
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              onPick={(session) => {
                const tail = terminalTail(session.id, 4000);
                onPick({
                  attachment: {
                    id: `term-${Date.now()}`,
                    kind: "terminal",
                    value: tail || `(no recent output for ${session.label})`,
                    label: session.label,
                  },
                });
                onClose();
              }}
              onPickFullLog={async (session) => {
                try {
                  const result = await (window.crab as any).termLogRead(session.id);
                  const content = result?.content ?? `(no session log found for ${session.label})`;
                  onPick({
                    attachment: {
                      id: `term-log-${Date.now()}`,
                      kind: "terminal",
                      value: content,
                      label: `${session.label} (full log)`,
                    },
                  });
                } catch {
                  const tail = terminalTail(session.id, 4000);
                  onPick({
                    attachment: {
                      id: `term-${Date.now()}`,
                      kind: "terminal",
                      value: tail || `(no session log for ${session.label})`,
                      label: session.label,
                    },
                  });
                }
                onClose();
              }}
            />
          )}
          {kind === "skill" && (
            <SkillPanel
              query={query}
              setQuery={setQuery}
              inputRef={inputRef}
              onPick={(skill) => {
                onPick({
                  attachment: {
                    id: `skill-${Date.now()}`,
                    kind: "skill",
                    value: skill.name,
                    label: skill.name,
                  },
                });
                onClose();
              }}
            />
          )}
        </div>
      </div>
    </div>,
    document.body,
  );
}

// ── Shared pieces ───────────────────────────────────────────────────

/**
 * Incremental rendering for long lists — avoids the cost of rendering
 * 50k+ DOM nodes when the user only scrolls through the first screen.
 *
 * Returns the visible slice and a sentinel ref that, when attached to
 * an element at the bottom of the list, triggers another batch when it
 * scrolls into view. The optional `onEndReached` fires once when we've
 * rendered every in-memory item — callers use this to fetch more from
 * the network (e.g. paginating git_log).
 */
function useInfiniteSlice<T>(
  items: T[],
  opts?: { batch?: number; onEndReached?: () => void },
): {
  visible: T[];
  sentinelRef: (el: HTMLElement | null) => void;
  hasMore: boolean;
} {
  const batch = opts?.batch ?? 40;
  const [visibleCount, setVisibleCount] = useState(batch);
  const endReachedFiredRef = useRef(false);
  const onEndReached = opts?.onEndReached;
  const itemsLengthRef = useRef(items.length);
  const observerRef = useRef<IntersectionObserver | null>(null);

  // Reset slice when the underlying list changes (new query/filter).
  useEffect(() => {
    setVisibleCount(batch);
    endReachedFiredRef.current = false;
    itemsLengthRef.current = items.length;
  }, [items, batch]);

  const visible = useMemo(() => items.slice(0, visibleCount), [items, visibleCount]);
  const hasMore = visibleCount < items.length;

  // Fire network-end-reached once when the in-memory list is exhausted.
  useEffect(() => {
    if (!hasMore && onEndReached && !endReachedFiredRef.current) {
      endReachedFiredRef.current = true;
      onEndReached();
    }
  }, [hasMore, onEndReached]);

  // Clean up the observer on unmount.
  useEffect(() => {
    return () => {
      observerRef.current?.disconnect();
      observerRef.current = null;
    };
  }, []);

  const sentinelRef = useCallback((el: HTMLElement | null) => {
    // Recreate the observer every time the sentinel element changes. We
    // keep the previous observer around so we can disconnect it cleanly.
    observerRef.current?.disconnect();
    observerRef.current = null;
    if (!el) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setVisibleCount((n) => Math.min(itemsLengthRef.current, n + batch));
        }
      },
      { rootMargin: "120px" },
    );
    observer.observe(el);
    observerRef.current = observer;
  }, [batch]);

  return { visible, sentinelRef, hasMore };
}

/**
 * A thin "Loading more…" row users see while the next batch is prepared
 * or when the network is paginating.
 */
function LoadMoreRow({
  label = "Loading more…",
  sentinelRef,
}: {
  label?: string;
  sentinelRef?: (el: HTMLElement | null) => void;
}) {
  return (
    <div
      ref={sentinelRef}
      className="flex items-center justify-center gap-2 px-3 py-3 text-[10px] text-muted-foreground"
    >
      <span className="h-1 w-1 animate-pulse rounded-full bg-muted-foreground" />
      {label}
    </div>
  );
}

function SearchBar({
  query,
  setQuery,
  placeholder,
  inputRef,
  right,
}: {
  query: string;
  setQuery: (v: string) => void;
  placeholder: string;
  inputRef: React.RefObject<HTMLInputElement>;
  right?: React.ReactNode;
}) {
  return (
    <div className="flex shrink-0 items-center gap-1.5 border-b border-border px-3 py-2">
      <Search size={12} className="text-muted-foreground" />
      <Input
        ref={inputRef}
        type="text"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        placeholder={placeholder}
        className="flex-1 bg-transparent text-xs text-foreground placeholder:text-muted-foreground focus:outline-none"
        aria-label={placeholder}
      />
      {right}
    </div>
  );
}

interface ListItemProps {
  active: boolean;
  onMouseEnter: () => void;
  onClick: () => void;
  leading?: React.ReactNode;
  primary: string;
  /** Small pill rendered inline right after the primary text. */
  badge?: string;
  secondary?: string;
  trailing?: React.ReactNode;
}

function ListItem({ active, onMouseEnter, onClick, leading, primary, badge, secondary, trailing }: ListItemProps) {
  return (
    <button
      type="button"
      role="option"
      aria-selected={active}
      onMouseEnter={onMouseEnter}
      onClick={onClick}
      className={clsx(
        "group flex w-full items-center gap-2 px-3 py-2 text-left text-xs transition",
        active ? "bg-primary/15 text-foreground" : "text-foreground hover:bg-accent",
      )}
    >
      {leading && <span className="shrink-0 text-primary">{leading}</span>}
      <span className="min-w-0 flex-1 flex items-center gap-1.5 truncate">
        <span className="truncate">{primary}</span>
        {badge && (
          <span className="shrink-0 rounded-full bg-accent px-1.5 py-0.5 text-[10px] text-muted-foreground">
            {badge}
          </span>
        )}
      </span>
      {secondary && (
        <span className="shrink-0 truncate text-[10px] text-muted-foreground max-w-[40%]">{secondary}</span>
      )}
      {trailing && <span className="shrink-0">{trailing}</span>}
    </button>
  );
}

function useListKeyboardNav(length: number) {
  const [sel, setSel] = useState(0);
  useEffect(() => { setSel(0); }, [length]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent, onEnter: () => void) => {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSel((s) => Math.min(length - 1, s + 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSel((s) => Math.max(0, s - 1));
      } else if (e.key === "Enter") {
        e.preventDefault();
        onEnter();
      }
    },
    [length],
  );

  return [sel, setSel, onKeyDown] as const;
}

// ── Instructions panel ─────────────────────────────────────────────

function InstructionPanel({
  query,
  setQuery,
  inputRef,
  onPick,
}: {
  query: string;
  setQuery: (v: string) => void;
  inputRef: React.RefObject<HTMLInputElement>;
  onPick: (ins: Instruction) => void;
}) {
  const [items, setItems] = useState<Instruction[]>(() => searchInstructions(""));
  const [editing, setEditing] = useState<Instruction | null>(null);
  const [creating, setCreating] = useState(false);

  const filtered = useMemo(() => searchInstructions(query), [query, items]); // items re-triggers

  const refresh = useCallback(() => setItems(searchInstructions("")), []);

  const [sel, setSel, onKeyDown] = useListKeyboardNav(filtered.length);
  const { visible, sentinelRef, hasMore } = useInfiniteSlice(filtered, { batch: 40 });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SearchBar
        query={query}
        setQuery={setQuery}
        placeholder="Search instructions…"
        inputRef={inputRef}
        right={
          <button
            type="button"
            onClick={() => setCreating(true)}
            className="flex items-center gap-1 rounded border border-border bg-card px-2 py-0.5 text-[10px] text-foreground hover:bg-accent"
          >
            <Plus size={10} /> New
          </button>
        }
      />
      <div
        role="listbox"
        aria-label="Instructions"
        tabIndex={0}
        onKeyDown={(e) => onKeyDown(e, () => filtered[sel] && onPick(filtered[sel]))}
        className="min-h-0 flex-1 overflow-y-auto"
      >
        {filtered.length === 0 && (
          <EmptyState
            icon={<Bookmark size={18} />}
            title="No matching instructions"
            body="Save your common prompts here. They're available across every repo and task."
            action={{ label: "Create instruction", onClick: () => setCreating(true) }}
          />
        )}
        {visible.map((ins, i) => (
          <div key={ins.id} className="group relative">
            <ListItem
              active={i === sel}
              onMouseEnter={() => setSel(i)}
              onClick={() => onPick(ins)}
              leading={ins.pinned ? <Pin size={12} className="text-primary" /> : <Sparkles size={12} />}
              primary={ins.title}
              badge={ins.category}
            />
            <div className="pointer-events-none absolute inset-y-0 right-2 flex items-center gap-0.5 opacity-0 transition group-hover:pointer-events-auto group-hover:opacity-100">
              <IconAction
                label={ins.pinned ? "Unpin" : "Pin"}
                onClick={() => { togglePinned(ins.id); refresh(); }}
                icon={ins.pinned ? <PinOff size={11} /> : <Pin size={11} />}
              />
              <IconAction
                label="Edit"
                onClick={() => setEditing(ins)}
                icon={<Sparkles size={11} />}
              />
              <IconAction
                label="Delete"
                onClick={() => {
                  if (window.confirm(`Delete "${ins.title}"?`)) {
                    deleteInstruction(ins.id);
                    refresh();
                  }
                }}
                icon={<Trash2 size={11} />}
                danger
              />
            </div>
          </div>
        ))}
        {hasMore && <LoadMoreRow sentinelRef={sentinelRef} />}
      </div>

      {(editing || creating) && (
        <InstructionEditor
          initial={editing ?? undefined}
          onCancel={() => { setEditing(null); setCreating(false); }}
          onSave={(payload) => {
            if (editing) {
              updateInstruction(editing.id, payload);
            } else {
              createInstruction(payload);
            }
            setEditing(null);
            setCreating(false);
            refresh();
          }}
        />
      )}
    </div>
  );
}

function InstructionEditor({
  initial,
  onCancel,
  onSave,
}: {
  initial?: Instruction;
  onCancel: () => void;
  onSave: (payload: { title: string; body: string; category?: string; pinned?: boolean }) => void;
}) {
  const [title, setTitle] = useState(initial?.title ?? "");
  const [body, setBody] = useState(initial?.body ?? "");
  const [category, setCategory] = useState(initial?.category ?? "");
  const [pinned, setPinned] = useState(initial?.pinned ?? false);

  const canSave = title.trim().length > 0 && body.trim().length > 0;

  return (
    <div className="shrink-0 border-t border-border bg-background p-3 space-y-2">
      <div className="flex items-center justify-between">
        <span className="text-[11px] font-semibold text-foreground">
          {initial ? "Edit instruction" : "New instruction"}
        </span>
        <button
          type="button"
          onClick={onCancel}
          className="rounded p-0.5 text-muted-foreground hover:text-foreground"
          aria-label="Cancel"
        >
          <X size={12} />
        </button>
      </div>
      <div className="flex gap-2">
        <Input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder="Title"
          className="flex-1 rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
        />
        <Input
          value={category}
          onChange={(e) => setCategory(e.target.value)}
          placeholder="Category (optional)"
          className="w-36 rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
        />
      </div>
      <Textarea
        value={body}
        onChange={(e) => setBody(e.target.value)}
        placeholder="Prompt body — what should the agent do?"
        rows={5}
        className="w-full resize-none rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
      />
      <div className="flex items-center justify-between">
        <label className="flex items-center gap-1.5 text-[11px] text-foreground">
          <input
            type="checkbox"
            checked={pinned}
            onChange={(e) => setPinned(e.target.checked)}
            className="h-3 w-3 accent-primary"
          />
          Pin to top
        </label>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded px-3 py-1 text-[11px] text-muted-foreground hover:text-foreground"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={!canSave}
            onClick={() => onSave({ title: title.trim(), body: body.trim(), category: category.trim() || undefined, pinned })}
            className="rounded bg-primary px-3 py-1 text-[11px] font-medium text-primary-foreground hover:bg-primary-hover disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {initial ? "Save" : "Create"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ── File / Folder panel ────────────────────────────────────────────

function FileOrFolderPanel({
  mode,
  query,
  setQuery,
  inputRef,
  repoId,
  onPick,
}: {
  mode: "file" | "folder";
  query: string;
  setQuery: (v: string) => void;
  inputRef: React.RefObject<HTMLInputElement>;
  repoId: string | null;
  onPick: (entry: DirEntry) => void;
}) {
  const [entries, setEntries] = useState<DirEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    if (!repoId) {
      setEntries([]);
      return;
    }
    setLoading(true);
    setError(null);
    rpc.list(repoId, {})
      .then((result) => {
        if (!cancelled) setEntries(result.entries);
      })
      .catch((err: Error) => {
        if (!cancelled) setError(err?.message ?? "Failed to list files");
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => { cancelled = true; };
  }, [repoId]);

  const filtered = useMemo(() => {
    if (!entries) return [];
    const pool = entries.filter((e) =>
      mode === "folder" ? e.kind === "dir" : (e.kind !== "dir" && e.kind !== "submodule"),
    );
    if (!query.trim()) return pool;
    return fuzzyRankPath(pool, query.trim().toLowerCase());
  }, [entries, query, mode]);

  const [sel, setSel, onKeyDown] = useListKeyboardNav(filtered.length);
  const { visible, sentinelRef, hasMore } = useInfiniteSlice(filtered, { batch: 60 });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SearchBar
        query={query}
        setQuery={setQuery}
        placeholder={mode === "folder" ? "Search folders…" : "Search files…"}
        inputRef={inputRef}
      />
      <div
        role="listbox"
        tabIndex={0}
        onKeyDown={(e) => onKeyDown(e, () => filtered[sel] && onPick(filtered[sel]))}
        className="min-h-0 flex-1 overflow-y-auto"
      >
        {!repoId && (
          <EmptyState
            icon={<AtSign size={18} />}
            title="No repository open"
            body="Open a repo to browse its files and folders."
          />
        )}
        {loading && <div className="p-6 text-center text-xs text-muted-foreground">Loading…</div>}
        {error && <div className="p-6 text-center text-xs text-destructive">{error}</div>}
        {!loading && !error && repoId && filtered.length === 0 && (
          <div className="p-6 text-center text-xs text-muted-foreground">No matches</div>
        )}
        {visible.map((entry, i) => (
          <ListItem
            key={entry.path}
            active={i === sel}
            onMouseEnter={() => setSel(i)}
            onClick={() => onPick(entry)}
            leading={mode === "folder" ? <FolderOpen size={12} /> : <FileText size={12} />}
            primary={entry.path.split("/").pop() ?? entry.path}
            secondary={entry.path.includes("/") ? entry.path.slice(0, entry.path.lastIndexOf("/")) : ""}
          />
        ))}
        {hasMore && <LoadMoreRow sentinelRef={sentinelRef} />}
        {!hasMore && filtered.length > 0 && (
          <div className="px-3 py-2 text-center text-[10px] text-muted-foreground">
            {filtered.length} {mode === "folder" ? "folders" : "files"}
          </div>
        )}
      </div>
    </div>
  );
}

// ── URL panel ──────────────────────────────────────────────────────

function UrlPanel({
  query,
  setQuery,
  inputRef,
  onPick,
}: {
  query: string;
  setQuery: (v: string) => void;
  inputRef: React.RefObject<HTMLInputElement>;
  onPick: (url: string) => void;
}) {
  const looksValid = /^https?:\/\//i.test(query.trim());

  const submit = useCallback(() => {
    const trimmed = query.trim();
    if (!trimmed) return;
    // Allow bare domains — prepend https:// for convenience.
    const url = looksValid ? trimmed : `https://${trimmed}`;
    onPick(url);
  }, [query, looksValid, onPick]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center gap-1.5 border-b border-border px-3 py-2">
        <Link2 size={12} className="text-muted-foreground" />
        <Input
          ref={inputRef}
          type="url"
          inputMode="url"
          autoComplete="off"
          spellCheck={false}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); submit(); } }}
          placeholder="https://example.com/docs/page"
          className="flex-1 bg-transparent text-xs text-foreground placeholder:text-muted-foreground focus:outline-none"
          aria-label="URL"
        />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        <div className="rounded-md border border-border bg-card p-3">
          <div className="mb-1 flex items-center gap-1.5 text-[11px] font-medium text-foreground">
            <Link2 size={11} className="text-primary" />
            Attach a web URL
          </div>
          <p className="text-[11px] text-muted-foreground">
            The agent will fetch and read the page when it works on this message. Handy for docs, API references, blog posts, and issue trackers.
          </p>
          {query.trim() && (
            <div className="mt-2 rounded border border-border bg-background px-2 py-1 text-[11px] text-muted-foreground font-mono break-all">
              {looksValid ? query.trim() : `https://${query.trim()}`}
            </div>
          )}
        </div>
      </div>
      <div className="flex shrink-0 justify-end gap-2 border-t border-border p-2">
        <button
          type="button"
          onClick={submit}
          disabled={!query.trim()}
          className="rounded bg-primary px-3 py-1 text-[11px] font-medium text-primary-foreground hover:bg-primary-hover disabled:opacity-50 disabled:cursor-not-allowed"
        >
          Attach URL
        </button>
      </div>
    </div>
  );
}

// ── Commit panel ───────────────────────────────────────────────────

function CommitPanel({
  query,
  setQuery,
  inputRef,
  repoId,
  onPick,
}: {
  query: string;
  setQuery: (v: string) => void;
  inputRef: React.RefObject<HTMLInputElement>;
  repoId: string | null;
  onPick: (commit: CommitRow) => void;
}) {
  const [commits, setCommits] = useState<CommitRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [paginating, setPaginating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [exhausted, setExhausted] = useState(false);

  const COMMIT_PAGE = 100;

  // Initial load when the repo changes.
  useEffect(() => {
    let cancelled = false;
    if (!repoId) {
      setCommits([]);
      setExhausted(true);
      return;
    }
    setCommits([]);
    setExhausted(false);
    setLoading(true);
    setError(null);
    rpc.gitLog(repoId, { max: COMMIT_PAGE })
      .then((r) => {
        if (cancelled) return;
        setCommits(r.commits);
        if (r.commits.length < COMMIT_PAGE) setExhausted(true);
      })
      .catch((err: Error) => {
        if (!cancelled) setError(err?.message ?? "git log failed");
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => { cancelled = true; };
  }, [repoId]);

  const loadMoreFromServer = useCallback(async () => {
    if (!repoId || paginating || exhausted) return;
    setPaginating(true);
    try {
      const r = await rpc.gitLog(repoId, { max: COMMIT_PAGE, skip: commits.length });
      if (r.commits.length === 0) {
        setExhausted(true);
      } else {
        // Deduplicate defensively — skip-based pagination can overlap if
        // commits were added concurrently.
        setCommits((prev) => {
          const seen = new Set(prev.map((c) => c.sha));
          const fresh = r.commits.filter((c) => !seen.has(c.sha));
          return [...prev, ...fresh];
        });
        if (r.commits.length < COMMIT_PAGE) setExhausted(true);
      }
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setPaginating(false);
    }
  }, [repoId, paginating, exhausted, commits.length]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return commits;
    return commits.filter((c) =>
      c.sha.toLowerCase().startsWith(q) ||
      c.subject.toLowerCase().includes(q) ||
      c.author.toLowerCase().includes(q),
    );
  }, [commits, query]);

  // When the user reaches the end of filtered results AND the list isn't
  // filtered (plain browse), kick off a network load. Filtered searches
  // stay client-side because the search API's pagination semantics
  // differ.
  const shouldPaginateFromServer = query.trim().length === 0 && !exhausted;

  const [sel, setSel, onKeyDown] = useListKeyboardNav(filtered.length);
  const { visible, sentinelRef, hasMore } = useInfiniteSlice(filtered, {
    batch: 40,
    onEndReached: shouldPaginateFromServer ? loadMoreFromServer : undefined,
  });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SearchBar
        query={query}
        setQuery={setQuery}
        placeholder="Search commits by SHA, subject, or author…"
        inputRef={inputRef}
      />
      <div
        role="listbox"
        tabIndex={0}
        onKeyDown={(e) => onKeyDown(e, () => filtered[sel] && onPick(filtered[sel]))}
        className="min-h-0 flex-1 overflow-y-auto"
      >
        {!repoId && <EmptyState icon={<GitCommit size={18} />} title="No repository open" body="Open a repo to browse its commits." />}
        {loading && <div className="p-6 text-center text-xs text-muted-foreground">Loading commits…</div>}
        {error && <div className="p-6 text-center text-xs text-destructive">{error}</div>}
        {!loading && !error && repoId && filtered.length === 0 && (
          <div className="p-6 text-center text-xs text-muted-foreground">No matches</div>
        )}
        {visible.map((c, i) => (
          <ListItem
            key={c.sha}
            active={i === sel}
            onMouseEnter={() => setSel(i)}
            onClick={() => onPick(c)}
            leading={<GitCommit size={12} />}
            primary={c.subject}
            secondary={`${c.sha.slice(0, 7)} · ${c.author}`}
          />
        ))}
        {(hasMore || paginating) && (
          <LoadMoreRow
            sentinelRef={sentinelRef}
            label={paginating ? "Fetching more from git log…" : "Loading more…"}
          />
        )}
        {!hasMore && !paginating && exhausted && commits.length > 0 && (
          <div className="px-3 py-2 text-center text-[10px] text-muted-foreground">
            {commits.length} commits · end of history
          </div>
        )}
      </div>
    </div>
  );
}

// ── Terminal panel ─────────────────────────────────────────────────

function TerminalPanel({
  query,
  setQuery,
  inputRef,
  onPick,
  onPickFullLog,
}: {
  query: string;
  setQuery: (v: string) => void;
  inputRef: React.RefObject<HTMLInputElement>;
  onPick: (session: TerminalSession) => void;
  onPickFullLog?: (session: TerminalSession) => void;
}) {
  const [sessions, setSessions] = useState<TerminalSession[]>([]);

  useEffect(() => subscribeTerminals(setSessions), []);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return sessions;
    return sessions.filter((s) =>
      s.label.toLowerCase().includes(q) ||
      (s.cwd ?? "").toLowerCase().includes(q),
    );
  }, [sessions, query]);

  const [sel, setSel, onKeyDown] = useListKeyboardNav(filtered.length);
  const { visible, sentinelRef, hasMore } = useInfiniteSlice(filtered, { batch: 20 });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SearchBar
        query={query}
        setQuery={setQuery}
        placeholder="Search terminal sessions…"
        inputRef={inputRef}
      />
      <div
        role="listbox"
        tabIndex={0}
        onKeyDown={(e) => onKeyDown(e, () => filtered[sel] && onPick(filtered[sel]))}
        className="min-h-0 flex-1 overflow-y-auto"
      >
        {sessions.length === 0 && (
          <EmptyState
            icon={<Terminal size={18} />}
            title="No terminal sessions yet"
            body="Open a terminal and run a command. Its output will show up here so you can attach it to a message."
          />
        )}
        {visible.map((s, i) => (
          <div key={s.id} className="group relative">
            <ListItem
              active={i === sel}
              onMouseEnter={() => setSel(i)}
              onClick={() => onPick(s)}
              leading={<Terminal size={12} className={s.active ? "text-success" : "text-muted-foreground"} />}
              primary={s.label}
              secondary={s.active ? `${humanBytes(s.buffer.length)} · live` : `${humanBytes(s.buffer.length)} · closed`}
            />
            {onPickFullLog && (
              <button
                onClick={(e) => { e.stopPropagation(); onPickFullLog(s); }}
                className="absolute right-2 top-1/2 -translate-y-1/2 hidden group-hover:flex items-center gap-1 px-2 py-0.5 rounded text-[10px] bg-secondary hover:bg-accent text-muted-foreground hover:text-foreground transition-colors"
                title="Attach full session log from disk"
              >
                <FileText size={10} />
                Full log
              </button>
            )}
          </div>
        ))}
        {hasMore && <LoadMoreRow sentinelRef={sentinelRef} />}
      </div>
    </div>
  );
}

// ── Skills panel ───────────────────────────────────────────────────

function SkillPanel({
  query,
  setQuery,
  inputRef,
  onPick,
}: {
  query: string;
  setQuery: (v: string) => void;
  inputRef: React.RefObject<HTMLInputElement>;
  onPick: (skill: Skill) => void;
}) {
  const [skills, setSkills] = useState<Skill[]>(() => listSkills());
  const [installUrl, setInstallUrl] = useState("");
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);
  const [showInstaller, setShowInstaller] = useState(false);

  const refresh = useCallback(() => setSkills(listSkills()), []);

  const filtered = useMemo(() => searchSkills(query), [query, skills]);
  const [sel, setSel, onKeyDown] = useListKeyboardNav(filtered.length);
  const { visible, sentinelRef, hasMore } = useInfiniteSlice(filtered, { batch: 30 });

  const handleInstall = useCallback(async () => {
    const url = installUrl.trim();
    if (!url) return;
    setInstalling(true);
    setInstallError(null);
    try {
      const skill = await installSkillFromUrl(url);
      refresh();
      setInstallUrl("");
      setShowInstaller(false);
      publishToast({ kind: "success", title: `Installed "${skill.name}"` });
    } catch (err) {
      const msg = (err as Error).message;
      setInstallError(msg);
      publishToast({ kind: "error", title: "Skill install failed", body: msg });
    } finally {
      setInstalling(false);
    }
  }, [installUrl, refresh]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SearchBar
        query={query}
        setQuery={setQuery}
        placeholder="Search installed skills…"
        inputRef={inputRef}
        right={
          <button
            type="button"
            onClick={() => setShowInstaller((v) => !v)}
            className="flex items-center gap-1 rounded border border-border bg-card px-2 py-0.5 text-[10px] text-foreground hover:bg-accent"
          >
            <Plus size={10} /> Install
          </button>
        }
      />

      {showInstaller && (
        <div className="shrink-0 border-b border-border bg-background p-3 space-y-2">
          <div className="text-[11px] font-semibold text-foreground">Install skill from GitHub</div>
          <Input
            value={installUrl}
            onChange={(e) => setInstallUrl(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter" && !installing) { e.preventDefault(); handleInstall(); } }}
            placeholder="https://github.com/owner/repo/blob/main/SKILL.md"
            className="w-full rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground font-mono focus:border-primary focus:outline-none"
          />
          <p className="text-[10px] text-muted-foreground">
            Paste a GitHub URL pointing at a <code className="font-mono text-foreground">SKILL.md</code> (or any markdown).
            Also accepts <code className="font-mono">owner/repo/path/SKILL.md</code>.
          </p>
          {installError && <div className="text-[11px] text-destructive">{installError}</div>}
          <div className="flex justify-end gap-2">
            <button
              type="button"
              onClick={() => { setShowInstaller(false); setInstallError(null); }}
              className="rounded px-2 py-1 text-[10px] text-muted-foreground hover:text-foreground"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={handleInstall}
              disabled={installing || !installUrl.trim()}
              className="rounded bg-primary px-3 py-1 text-[10px] font-medium text-primary-foreground hover:bg-primary-hover disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {installing ? "Installing…" : "Install"}
            </button>
          </div>
        </div>
      )}

      <div
        role="listbox"
        tabIndex={0}
        onKeyDown={(e) => onKeyDown(e, () => filtered[sel] && onPick(filtered[sel]))}
        className="min-h-0 flex-1 overflow-y-auto"
      >
        {filtered.length === 0 && (
          <EmptyState
            icon={<Brain size={18} />}
            title="No skills installed"
            body="Install a skill by pasting a GitHub URL. Once installed, reference it here with @ to include it in your prompt."
            action={!showInstaller ? { label: "Install from GitHub", onClick: () => setShowInstaller(true) } : undefined}
          />
        )}
        {visible.map((s, i) => (
          <div key={s.id} className="group relative">
            <ListItem
              active={i === sel}
              onMouseEnter={() => setSel(i)}
              onClick={() => onPick(s)}
              leading={<Brain size={12} />}
              primary={s.name}
              secondary={s.description || humanBytes(s.length)}
            />
            <div className="pointer-events-none absolute inset-y-0 right-2 flex items-center gap-0.5 opacity-0 transition group-hover:pointer-events-auto group-hover:opacity-100">
              <IconAction
                label="Reinstall / update"
                onClick={async () => {
                  try {
                    await installSkillFromUrl(s.sourceUrl);
                    refresh();
                    publishToast({ kind: "success", title: `Updated "${s.name}"` });
                  } catch (err) {
                    publishToast({ kind: "error", title: "Update failed", body: (err as Error).message });
                  }
                }}
                icon={<RefreshCcw size={11} />}
              />
              <IconAction
                label="Remove"
                onClick={() => {
                  if (window.confirm(`Remove skill "${s.name}"?`)) {
                    removeSkill(s.id);
                    refresh();
                  }
                }}
                icon={<Trash2 size={11} />}
                danger
              />
            </div>
          </div>
        ))}
        {hasMore && <LoadMoreRow sentinelRef={sentinelRef} />}
      </div>
    </div>
  );
}

// ── Small reusable bits ────────────────────────────────────────────

function IconAction({
  icon,
  label,
  onClick,
  danger,
}: {
  icon: React.ReactNode;
  label: string;
  onClick: () => void;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={(e) => { e.stopPropagation(); onClick(); }}
      title={label}
      aria-label={label}
      className={clsx(
        "rounded p-1 transition",
        danger
          ? "text-muted-foreground hover:bg-destructive/15 hover:text-destructive"
          : "text-muted-foreground hover:bg-accent hover:text-foreground",
      )}
    >
      {icon}
    </button>
  );
}

function EmptyState({
  icon,
  title,
  body,
  action,
}: {
  icon: React.ReactNode;
  title: string;
  body?: string;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-6 py-10 text-center">
      <div className="text-muted-foreground/60">{icon}</div>
      <div className="text-xs font-medium text-foreground">{title}</div>
      {body && <p className="max-w-[320px] text-[11px] leading-relaxed text-muted-foreground">{body}</p>}
      {action && (
        <button
          type="button"
          onClick={action.onClick}
          className="mt-2 rounded bg-primary px-3 py-1 text-[11px] font-medium text-primary-foreground hover:bg-primary-hover"
        >
          {action.label}
        </button>
      )}
    </div>
  );
}

// ── Helpers ────────────────────────────────────────────────────────

function humanBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function fuzzyRankPath(entries: DirEntry[], q: string): DirEntry[] {
  const scored: Array<{ e: DirEntry; score: number }> = [];
  for (const e of entries) {
    const path = e.path.toLowerCase();
    const base = path.slice(path.lastIndexOf("/") + 1);
    const score = Math.max(matchScore(base, q) + 500, matchScore(path, q));
    if (score > 0) scored.push({ e, score });
  }
  scored.sort((a, b) => b.score - a.score);
  return scored.map((s) => s.e);
}

function matchScore(hay: string, q: string): number {
  let hi = 0, qi = 0, score = 0;
  let prevHit = -2;
  while (hi < hay.length && qi < q.length) {
    if (hay[hi] === q[qi]) {
      score += 10;
      if (prevHit === hi - 1) score += 15;
      prevHit = hi;
      qi++;
    }
    hi++;
  }
  return qi === q.length ? score - hi : 0;
}
