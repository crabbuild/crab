import { useCallback, useEffect, forwardRef, useImperativeHandle, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { SafePatchDiff } from "../components/SafePatchDiff";
import { LargeFileDiffView } from "../components/LargeFileDiffView";
import {
  FileTree as PierreFileTree,
  useFileTree,
  useFileTreeSelection,
} from "@pierre/trees/react";
import type { GitStatus, GitStatusEntry } from "@pierre/trees";
import { themeToTreeStyles } from "@pierre/trees";
import {
  ChevronLeft,
  ChevronRight,
  Copy,
  FileDiff,
  FolderTree,
  GitMerge,
  List,
  Loader2,
  ScrollText,
} from "lucide-react";
import { DirEntry } from "../ipc";
import { OpenRepo } from "../state";
import { CommitChanges, CommitFileChange, CommitRow, rpc, fileDiffCancellable, type FileComparison, type DiffVersion } from "../ipc";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { diffOptions } from "../lib/diffTheme";

interface FilePatchState {
  result: FileComparison | null;
  err: string | null;
  loading: boolean;
}

interface Props {
  repo: OpenRepo;
  commits: CommitRow[];
  selectedSha: string;
  onSelectCommit: (sha: string) => void;
  /** Optional: open a file from this commit, pinned at its sha,
   *  as a new editor tab. Wired by RepoDetail — when undefined
   *  the "open file at revision" affordance is hidden. */
  onOpenFileAtRev?: (path: string, entry: DirEntry, pinRev: string) => void;
}

type ViewMode = "list" | "tree" | "unified";
const LS_FILE_VIEW_MODE = "crab.commitDetail.filesMode";

function loadViewMode(): ViewMode {
  const stored = localStorage.getItem(LS_FILE_VIEW_MODE);
  if (stored === "list" || stored === "tree") return stored;
  return "unified";
}

/**
 * Main-pane renderer for a selected commit. Three logical panes:
 *
 *   - Header: subject, sha, author, navigation.
 *   - Left of body: changed files (toggle between flat list and
 *     a GitHub-style tree).
 *   - Right of body: unified patch for the selected file.
 *
 * Handles merge commits (diff is against the first parent — same
 * default as most git UIs) and root commits (diff against the
 * empty tree so the commit's whole introduction is visible).
 */
export function CommitDetailView({
  repo,
  commits,
  selectedSha,
  onSelectCommit,
  onOpenFileAtRev,
}: Props) {
  const [changes, setChanges] = useState<CommitChanges | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [activeFile, setActiveFile] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>(loadViewMode);
  const effective = useEffectiveTheme();

  // All-diffs state: patches for every file, rendered in a scrollable pane
  const [allPatches, setAllPatches] = useState<Map<string, FilePatchState>>(new Map());
  const allDiffsRef = useRef<{ scrollToFile: (path: string) => void } | null>(null);

  const scrollToFileInUnified = useCallback((path: string) => {
    setActiveFile(path);
    allDiffsRef.current?.scrollToFile(path);
  }, []);

  useEffect(() => {
    localStorage.setItem(LS_FILE_VIEW_MODE, viewMode);
  }, [viewMode]);

  const idx = useMemo(
    () => commits.findIndex((c) => c.sha === selectedSha),
    [commits, selectedSha],
  );
  const prev = idx > 0 ? commits[idx - 1] : null;
  const next = idx >= 0 && idx < commits.length - 1 ? commits[idx + 1] : null;

  useEffect(() => {
    let cancelled = false;
    setChanges(null);
    setErr(null);
    setLoading(true);
    setActiveFile(null);
    setAllPatches(new Map());
    rpc
      .gitCommitChanges(repo.repo_id, selectedSha)
      .then((r) => {
        if (cancelled) return;
        setChanges(r);
        if (r.entries.length > 0) setActiveFile(r.entries[0].path);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, selectedSha]);

  useEffect(() => {
    if (!changes) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
      const files = changes.entries;
      if (!files.length) return;
      const fi = Math.max(
        0,
        files.findIndex((f) => f.path === activeFile),
      );
      if (e.key === "j" || e.key === "ArrowDown") {
        e.preventDefault();
        setActiveFile(files[Math.min(files.length - 1, fi + 1)].path);
      } else if (e.key === "k" || e.key === "ArrowUp") {
        e.preventDefault();
        setActiveFile(files[Math.max(0, fi - 1)].path);
      } else if (e.key === "h" || e.key === "ArrowLeft") {
        if (prev) {
          e.preventDefault();
          onSelectCommit(prev.sha);
        }
      } else if (e.key === "l" || e.key === "ArrowRight") {
        if (next) {
          e.preventDefault();
          onSelectCommit(next.sha);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [changes, activeFile, prev, next, onSelectCommit]);

  const openActiveFileAsTab = useCallback(() => {
    if (!activeFile || !onOpenFileAtRev) return;
    // Construct a minimal DirEntry from what we know about the
    // file. Editors only need kind + size + path; for a historical
    // file, `file` is the safe default — the smudge filter still
    // runs on pointer content, producing full bytes.
    const entry: DirEntry = {
      path: activeFile,
      kind: "file",
      size: null,
      pointer_hash: null,
    };
    onOpenFileAtRev(activeFile, entry, selectedSha);
  }, [activeFile, onOpenFileAtRev, selectedSha]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Loading commit…
      </div>
    );
  }
  if (err) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
        {err}
      </div>
    );
  }
  if (!changes) return null;

  return (
    <div className="flex h-full min-h-0 flex-col bg-gx-bg-sub">
      <div className="shrink-0 border-b border-gx-border bg-gx-surface px-4 py-3">
        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="truncate text-sm font-semibold">{changes.commit.subject}</div>
            <div className="mt-0.5 flex items-center gap-2 text-[11px] text-gx-muted">
              <code className="font-mono">{changes.commit.sha.slice(0, 12)}</code>
              <button
                type="button"
                className="text-gx-muted hover:text-gx-text"
                onClick={() => void navigator.clipboard.writeText(changes.commit.sha)}
                title="Copy full sha"
                aria-label="Copy sha"
              >
                <Copy size={11} />
              </button>
              <span>· {changes.commit.author}</span>
              <span>· {new Date(changes.commit.date).toLocaleString()}</span>
              {changes.is_merge && (
                <span className="chip text-gx-accent">
                  <GitMerge size={10} /> merge · {changes.commit.parents.length} parents
                </span>
              )}
            </div>
          </div>
          <div className="flex shrink-0 gap-1">
            <button
              type="button"
              className="btn text-xs"
              disabled={!prev}
              onClick={() => prev && onSelectCommit(prev.sha)}
              title="Newer commit"
            >
              <ChevronLeft size={12} />
            </button>
            <button
              type="button"
              className="btn text-xs"
              disabled={!next}
              onClick={() => next && onSelectCommit(next.sha)}
              title="Older commit"
            >
              <ChevronRight size={12} />
            </button>
          </div>
        </div>
        <div className="mt-2 flex items-center gap-3 text-[12px] text-gx-text">
          <span>
            <strong>{changes.entries.length}</strong> file{changes.entries.length === 1 ? "" : "s"} changed
            {" · "}
            <span className="font-semibold text-gx-success">
              +{changes.entries.reduce((s, e) => s + e.additions, 0)}
            </span>
            {" "}
            <span className="font-semibold text-gx-error">
              −{changes.entries.reduce((s, e) => s + e.deletions, 0)}
            </span>
          </span>
          <div
            role="radiogroup"
            aria-label="Files view"
            className="flex overflow-hidden rounded border border-gx-border"
          >
            <ModeButton
              active={viewMode === "list"}
              icon={<List size={11} />}
              label="List"
              onClick={() => setViewMode("list")}
            />
            <ModeButton
              active={viewMode === "tree"}
              icon={<FolderTree size={11} />}
              label="Tree"
              onClick={() => setViewMode("tree")}
            />
            <ModeButton
              active={viewMode === "unified"}
              icon={<ScrollText size={11} />}
              label="Unified"
              onClick={() => setViewMode("unified")}
            />
          </div>
          {onOpenFileAtRev && activeFile && (
            <button
              type="button"
              className="ml-auto text-gx-muted hover:text-gx-text"
              onClick={openActiveFileAsTab}
              title={`Open ${activeFile} at ${selectedSha.slice(0, 7)} as a tab`}
            >
              Open as tab
            </button>
          )}
        </div>
      </div>

      {changes.entries.length === 0 ? (
        <div className="flex flex-1 items-center justify-center text-sm text-gx-muted">
          No file changes in this commit.
        </div>
      ) : (
        <div className="grid min-h-0 flex-1 grid-cols-[300px_1fr]">
          {viewMode === "list" ? (
            <FileList
              entries={changes.entries}
              active={activeFile}
              onSelect={scrollToFileInUnified}
            />
          ) : viewMode === "tree" ? (
            <FileTreePane
              entries={changes.entries}
              active={activeFile}
              onSelect={scrollToFileInUnified}
              theme={effective}
            />
          ) : (
            <UnifiedFileNav
              entries={changes.entries}
              active={activeFile}
              onSelect={scrollToFileInUnified}
              patches={allPatches}
            />
          )}
          <AllDiffsPane
            ref={allDiffsRef}
            repo={repo}
            entries={changes.entries}
            selectedSha={selectedSha}
            parentSha={changes.commit.parents[0] ?? null}
            activeFile={activeFile}
            onSelectFile={setActiveFile}
            theme={effective}
            patches={allPatches}
            setPatches={setAllPatches}
          />
        </div>
      )}
    </div>
  );
}

function ModeButton({
  active,
  icon,
  label,
  onClick,
}: {
  active: boolean;
  icon: React.ReactNode;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={active}
      onClick={onClick}
      className={clsx(
        "flex items-center gap-1 px-2 py-0.5 text-[11px] transition",
        active
          ? "bg-gx-accent/20 text-gx-text"
          : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
      )}
      title={`${label} view`}
    >
      {icon}
      {label}
    </button>
  );
}

function FileList({
  entries,
  active,
  onSelect,
}: {
  entries: CommitFileChange[];
  active: string | null;
  onSelect: (path: string) => void;
}) {
  return (
    <aside className="min-h-0 overflow-auto border-r border-gx-border bg-gx-surface">
      <ul>
        {entries.map((e) => {
          const isActive = e.path === active;
          return (
            <li key={e.path}>
              <button
                type="button"
                onClick={() => onSelect(e.path)}
                className={clsx(
                  "flex w-full items-start gap-2 border-b border-gx-border/40 px-3 py-1.5 text-left text-[12px] transition",
                  isActive
                    ? "bg-gx-accent/15 text-gx-text"
                    : "text-gx-text hover:bg-gx-surface-hover",
                )}
                title={e.path}
              >
                <FileDiff size={12} className="mt-[2px] shrink-0 text-gx-muted" />
                <div className="min-w-0 flex-1">
                  <div className="truncate font-mono text-[12.5px]">
                    {e.path.split("/").pop()}
                  </div>
                  {e.old_path && (
                    <div className="truncate font-mono text-[10px] text-gx-muted">
                      ← {e.old_path}
                    </div>
                  )}
                  <div className="mt-0.5 flex items-center gap-2 text-[10px] text-gx-muted">
                    {e.additions > 0 && (
                      <span className="text-gx-success">+{e.additions}</span>
                    )}
                    {e.deletions > 0 && (
                      <span className="text-gx-error">−{e.deletions}</span>
                    )}
                    {e.status === "renamed" && <span className="chip text-[9px]">rn</span>}
                  </div>
                  {e.path.includes("/") && (
                    <div className="truncate text-[10px] text-gx-muted">
                      {e.path.slice(0, e.path.lastIndexOf("/"))}
                    </div>
                  )}
                </div>
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}

/**
 * GitHub-style changed-files tree. Uses `@pierre/trees` with:
 *  - `gitStatus` prop so the library paints native A/M/D colouring
 *  - `renderRowDecoration` to append `+x/-y` deltas at the row edge
 *  - `initialExpansion: "open"` so every touched directory is
 *    pre-expanded. In a commit view the user always wants to see
 *    files, not hunt through collapsed folders.
 */
function FileTreePane({
  entries,
  active,
  onSelect,
  theme,
}: {
  entries: CommitFileChange[];
  active: string | null;
  onSelect: (path: string) => void;
  theme: "dark" | "light";
}) {
  const paths = useMemo(() => entries.map((e) => e.path).sort(), [entries]);
  const byPath = useMemo(() => {
    const m = new Map<string, CommitFileChange>();
    for (const e of entries) m.set(e.path, e);
    return m;
  }, [entries]);

  const { model } = useFileTree({
    paths,
    initialExpansion: "open",
    dragAndDrop: false,
    renaming: false,
    icons: { set: "complete", colored: true },
    renderRowDecoration: ({ item }) => {
      if (item.kind === "directory") return null;
      const change = byPath.get(item.path);
      if (!change) return null;
      const parts: string[] = [];
      if (change.additions > 0) parts.push(`+${change.additions}`);
      if (change.deletions > 0) parts.push(`−${change.deletions}`);
      if (parts.length === 0) return null;
      return {
        text: parts.join(" "),
        title: `${change.status} · +${change.additions} −${change.deletions}`,
      };
    },
  });

  useEffect(() => {
    model.resetPaths(paths);
  }, [model, paths]);

  useEffect(() => {
    const status: GitStatusEntry[] = entries.map((e) => ({
      path: e.path,
      status: toTreesStatus(e.status, e.additions, e.deletions),
    }));
    model.setGitStatus(status);
  }, [model, entries]);

  // Selection → activate file.
  const selection = useFileTreeSelection(model);
  const lastOpened = useRef<string | null>(null);
  useEffect(() => {
    const first = selection[0];
    if (!first || first === lastOpened.current) return;
    if (!byPath.has(first)) return;
    lastOpened.current = first;
    onSelect(first);
  }, [selection, byPath, onSelect]);

  useEffect(() => {
    if (!active) return;
    try {
      model.focusPath(active);
    } catch {
      // Not present (just switched commits); no-op.
    }
  }, [model, active]);

  const themeStyles = useMemo(() => buildTreeTheme(theme), [theme]);

  return (
    <aside className="min-h-0 overflow-hidden border-r border-gx-border bg-gx-surface">
      <PierreFileTree model={model} className="block h-full" style={themeStyles} />
    </aside>
  );
}

function toTreesStatus(
  status: CommitFileChange["status"],
  additions: number,
  deletions: number,
): GitStatus {
  if (status === "renamed") return "renamed";
  // Pure additions / deletions → show those colours; otherwise fall
  // back to modified (dual-sided change).
  if (additions > 0 && deletions === 0) return "added";
  if (additions === 0 && deletions > 0) return "deleted";
  return "modified";
}

/**
 * Compact file navigation sidebar for the "unified" left-panel mode.
 * Shows file paths with loading indicators and +/- stats.
 */
function UnifiedFileNav({
  entries,
  active,
  onSelect,
  patches,
}: {
  entries: CommitFileChange[];
  active: string | null;
  onSelect: (path: string) => void;
  patches: Map<string, FilePatchState>;
}) {
  return (
    <aside className="min-h-0 overflow-auto border-r border-gx-border bg-gx-surface">
      <ul className="py-1">
        {entries.map((e) => {
          const isActive = e.path === active;
          const state = patches.get(e.path);
          return (
            <li key={e.path}>
              <button
                type="button"
                onClick={() => onSelect(e.path)}
                className={clsx(
                  "flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] transition",
                  isActive
                    ? "bg-gx-accent/15 text-gx-text"
                    : "text-gx-text hover:bg-gx-surface-hover",
                )}
                title={e.path}
              >
                <FileDiff size={11} className="shrink-0 text-gx-muted" />
                <span className="min-w-0 flex-1 truncate font-mono">
                  {e.path}
                </span>
                <span className="shrink-0 flex items-center gap-1 text-[10px]">
                  {e.additions > 0 && (
                    <span className="text-gx-success">+{e.additions}</span>
                  )}
                  {e.deletions > 0 && (
                    <span className="text-gx-error">−{e.deletions}</span>
                  )}
                  {state?.loading && <Loader2 size={9} className="animate-spin text-gx-muted" />}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}

interface AllDiffsPaneHandle {
  scrollToFile: (path: string) => void;
}

/**
 * Scrollable pane that renders ALL file diffs for a commit sequentially.
 * Each file section has a sticky header. Exposes `scrollToFile` via ref
 * so the parent can programmatically scroll to a specific file.
 */
const AllDiffsPane = forwardRef<
  AllDiffsPaneHandle,
  {
    repo: OpenRepo;
    entries: CommitFileChange[];
    selectedSha: string;
    parentSha: string | null;
    activeFile: string | null;
    onSelectFile: (path: string) => void;
    theme: "dark" | "light";
    patches: Map<string, FilePatchState>;
    setPatches: React.Dispatch<React.SetStateAction<Map<string, FilePatchState>>>;
  }
>(function AllDiffsPane(
  { repo, entries, selectedSha, parentSha, activeFile, onSelectFile, theme, patches, setPatches },
  ref,
) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const fileRefs = useRef<Map<string, HTMLDivElement>>(new Map());

  // Expose scrollToFile to parent
  useImperativeHandle(ref, () => ({
    scrollToFile(path: string) {
      const el = fileRefs.current.get(path);
      if (el && scrollRef.current) {
        el.scrollIntoView({ behavior: "smooth", block: "start" });
      }
    },
  }));

  // Fetch all patches on mount / when commit changes.
  // Concurrency is capped at 3 simultaneous requests to avoid
  // flooding the agent and accumulating hundreds of large diff
  // strings in memory at once.
  useEffect(() => {
    let cancelled = false;
    const requests = new Set<() => void>();
    const initial = new Map<string, FilePatchState>();
    for (const entry of entries) initial.set(entry.path, { result: null, err: null, loading: true });
    setPatches(initial);
    const before: DiffVersion = parentSha ? { kind: "revision", rev: parentSha } : { kind: "empty" };
    const after: DiffVersion = { kind: "revision", rev: selectedSha };
    const CONCURRENCY = 3;
    let idx = 0;
    const fetchDiff = async (entry: CommitFileChange) => {
      const request = fileDiffCancellable(repo.repo_id, entry.path, { before, after, old_path: entry.old_path ?? undefined });
      requests.add(request.cancel);
      try {
        const result = await request.promise;
        if (!cancelled) setPatches((prev) => {
          const next = new Map(prev);
          next.set(entry.path, { result, err: null, loading: false });
          return next;
        });
      } catch (error) {
        if (!cancelled) setPatches((prev) => {
          const next = new Map(prev);
          next.set(entry.path, { result: null, err: error instanceof Error ? error.message : String(error), loading: false });
          return next;
        });
      } finally {
        requests.delete(request.cancel);
      }
    };

    const runNext = async () => {
      while (idx < entries.length && !cancelled) {
        const entry = entries[idx++];
        await fetchDiff(entry);
      }
    };

    // Spawn CONCURRENCY workers that each pull from the queue.
    const workers = Array.from({ length: Math.min(CONCURRENCY, entries.length) }, runNext);
    void Promise.all(workers);

    return () => { cancelled = true; for (const cancel of requests) cancel(); };
  }, [repo.repo_id, selectedSha, parentSha, entries, setPatches]);

  const setFileRef = useCallback((path: string, el: HTMLDivElement | null) => {
    if (el) {
      fileRefs.current.set(path, el);
    } else {
      fileRefs.current.delete(path);
    }
  }, []);

  return (
    <div ref={scrollRef} className="min-h-0 overflow-auto bg-gx-bg-sub">
      {entries.map((entry) => {
        const state = patches.get(entry.path);
        return (
          <div
            key={entry.path}
            ref={(el) => setFileRef(entry.path, el)}
            className="border-b border-gx-border"
          >
            {/* Sticky file header — click to focus this file */}
            <button
              type="button"
              onClick={() => onSelectFile(entry.path)}
              className={clsx(
                "sticky top-0 z-10 flex w-full items-center gap-2 border-b border-gx-border/60 px-4 py-2 text-left backdrop-blur-sm transition",
                entry.path === activeFile
                  ? "bg-gx-accent/10"
                  : "bg-gx-surface/95 hover:bg-gx-surface-hover/95",
              )}
            >
              <FileDiff size={12} className="shrink-0 text-gx-muted" />
              <span className="min-w-0 flex-1 truncate font-mono text-[12px] font-medium text-gx-text">
                {entry.path}
              </span>
              <span className="flex items-center gap-2 text-[10px] text-gx-muted">
                {entry.additions > 0 && (
                  <span className="font-semibold text-gx-success">+{entry.additions}</span>
                )}
                {entry.deletions > 0 && (
                  <span className="font-semibold text-gx-error">−{entry.deletions}</span>
                )}
                {entry.status === "renamed" && entry.old_path && (
                  <span className="chip text-[9px]">← {entry.old_path}</span>
                )}
              </span>
            </button>

            {/* Diff content */}
            {state?.loading ? (
              <div className="flex items-center justify-center gap-2 py-8 text-sm text-gx-muted">
                <Loader2 size={14} className="animate-spin" /> Loading diff…
              </div>
            ) : state?.err ? (
              <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
                {state.err}
              </div>
            ) : state?.result?.kind === "large_file" ? (
              <LargeFileDiffView data={state.result} />
            ) : state?.result?.kind === "git" ? (
              state.result.patch.trim().length === 0 ? (
                <p className="px-4 py-6 text-xs text-gx-muted">No changes for this file.</p>
              ) : <SafePatchDiff patch={state.result.patch} options={diffOptions(theme)} />
            ) : null}
          </div>
        );
      })}
    </div>
  );
});


// Duplicate of the helper in ChangesView — small enough that the
// copy is cheaper than a shared module while the tree-theming
// surface is still evolving.
function buildTreeTheme(mode: "dark" | "light"): React.CSSProperties {
  const cs =
    typeof window !== "undefined" ? getComputedStyle(document.documentElement) : null;
  const rgb = (name: string, fallback: string): string => {
    const raw = cs?.getPropertyValue(name).trim();
    if (!raw) return fallback;
    return `rgb(${raw})`;
  };
  const colors: Record<string, string> = {
    "sideBar.background": rgb("--gx-surface", mode === "dark" ? "#171a21" : "#f4f5f9"),
    "editor.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "sideBar.foreground": rgb("--gx-text", mode === "dark" ? "#e7eaf1" : "#171b23"),
    "sideBarSectionHeader.foreground": rgb("--gx-muted", "#8a93a5"),
    "sideBar.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "list.hoverBackground": rgb(
      "--gx-surface-hover",
      mode === "dark" ? "#1e222b" : "#e9ecf3",
    ),
    "list.activeSelectionBackground": rgb(
      "--gx-accent",
      mode === "dark" ? "#4f8cff" : "#2c6def",
    ),
    "list.activeSelectionForeground": "#ffffff",
    focusBorder: rgb("--gx-accent", mode === "dark" ? "#4f8cff" : "#2c6def"),
    "input.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "input.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "scrollbarSlider.background": rgb(
      "--gx-border",
      mode === "dark" ? "#242832" : "#dde0e8",
    ),
    "gitDecoration.addedResourceForeground": rgb(
      "--gx-success",
      mode === "dark" ? "#4ade80" : "#16a34a",
    ),
    "gitDecoration.modifiedResourceForeground": rgb(
      "--gx-warn",
      mode === "dark" ? "#facc15" : "#b48700",
    ),
    "gitDecoration.deletedResourceForeground": rgb(
      "--gx-error",
      mode === "dark" ? "#f87171" : "#dc2626",
    ),
  };
  const styles = themeToTreeStyles({
    type: mode,
    bg: colors["sideBar.background"],
    fg: colors["sideBar.foreground"],
    colors,
  });
  return styles as unknown as React.CSSProperties;
}
