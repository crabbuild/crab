import { useCallback, useEffect, useMemo, useState } from "react";
import {
  FileDiff,
  Filter,
  GitBranch,
  GitMerge,
  GitPullRequest,
  Loader2,
  RefreshCw,
  X,
} from "lucide-react";
import { PanelGroup, Panel, PanelResizeHandle } from "react-resizable-panels";
import { OpenRepo } from "../state";
import {
  rpc,
  DirEntry,
  HydratedWorktreeEntry,
  WorktreeStatus,
  MergeStrategy,
  OpResult,
  RepoHandle,
} from "../ipc";
import { DiffView } from "./DiffView";
import { EditorPane, EditorTab } from "../components/EditorPane";
import { FileTree } from "../components/FileTree";

interface Props {
  repo: OpenRepo;
  worktree: HydratedWorktreeEntry;
  status: WorktreeStatus | undefined;
  onClose: () => void;
}

interface ChangedFile {
  path: string;
  /** Git status code: M, A, D, R, etc. */
  status: string;
}

/** Merge target: a branch the user can merge the worktree branch into. */
interface MergeTarget {
  name: string;
  isCurrent: boolean;
}

/**
 * Detail panel shown when a worktree card is selected. Displays:
 * - Full file tree of the worktree (with filter toggle for changed-only)
 * - Multi-tab editor for viewing/editing files
 * - Diff view for changed files
 * - Merge-back actions to integrate changes into other branches
 */
export function WorktreeDetailPanel({ repo, worktree, status, onClose }: Props) {
  const [changedFiles, setChangedFiles] = useState<ChangedFile[] | null>(null);
  const [allEntries, setAllEntries] = useState<DirEntry[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showMergePanel, setShowMergePanel] = useState(false);
  const [filterChanged, setFilterChanged] = useState(false);

  // Multi-tab editor state
  const [editorTabs, setEditorTabs] = useState<EditorTab[]>([]);
  const [activeTabKey, setActiveTabKey] = useState<string | null>(null);

  // Worktree repo handle (opened lazily for file listing/editing)
  const [wtRepo, setWtRepo] = useState<(RepoHandle & { label: string; path: string; openedAt: number }) | null>(null);

  const branch = shortBranch(worktree.branch);
  const changedPathSet = useMemo(
    () => new Set(changedFiles?.map((f) => f.path) ?? []),
    [changedFiles],
  );
  const changedStatusMap = useMemo(() => {
    const m = new Map<string, string>();
    for (const f of changedFiles ?? []) m.set(f.path, f.status);
    return m;
  }, [changedFiles]);

  // Open the worktree as a repo context for file operations
  useEffect(() => {
    let cancelled = false;
    rpc
      .openLocal(worktree.path)
      .then((handle) => {
        if (!cancelled) {
          setWtRepo({
            ...handle,
            label: worktree.meta?.display_name || dirName(worktree.path),
            path: worktree.path,
            openedAt: Date.now(),
          });
        }
      })
      .catch(() => {
        // If we can't open the worktree, fall back to using the parent repo
        if (!cancelled) setWtRepo(null);
      });
    return () => { cancelled = true; };
  }, [worktree.path, worktree.meta?.display_name]);

  // Fetch the full file listing for the worktree.
  // Falls back to the parent repo if the worktree can't be opened
  // independently (e.g. the main worktree is already the active repo).
  useEffect(() => {
    const repoId = wtRepo?.repo_id ?? repo.repo_id;
    let cancelled = false;
    rpc
      .list(repoId)
      .then((r) => {
        if (!cancelled) setAllEntries(r.entries);
      })
      .catch(() => {
        if (!cancelled) setAllEntries([]);
      });
    return () => { cancelled = true; };
  }, [wtRepo, repo.repo_id]);

  // Fetch changed files using git_branch_info
  const fetchChanges = useCallback(() => {
    setLoading(true);
    setError(null);
    setChangedFiles(null);

    rpc
      .gitBranchInfo(repo.repo_id, branch)
      .then((info) => {
        const files: ChangedFile[] = info.changed_files.map((f) => ({
          path: f.path,
          status:
            f.additions > 0 && f.deletions === 0 ? "A"
            : f.additions === 0 && f.deletions > 0 ? "D"
            : "M",
        }));
        setChangedFiles(files);
      })
      .catch((e: Error) => {
        // Fallback: try git status
        const targetRepoId = wtRepo?.repo_id ?? repo.repo_id;
        rpc
          .gitStatus(targetRepoId)
          .then((r) => {
            const files: ChangedFile[] = r.entries.map((entry) => ({
              path: entry.path,
              status: entry.code.trim().charAt(0) || "M",
            }));
            setChangedFiles(files);
          })
          .catch(() => {
            setError(e.message);
            setChangedFiles([]);
          });
      })
      .finally(() => setLoading(false));
  }, [repo.repo_id, branch, wtRepo]);

  useEffect(() => {
    fetchChanges();
  }, [fetchChanges]);

  // Build the git status record for the FileTree component
  const gitStatusRecord = useMemo<Record<string, string>>(() => {
    const record: Record<string, string> = {};
    for (const [path, status] of changedStatusMap) {
      record[path] = status;
    }
    return record;
  }, [changedStatusMap]);

  // Entries to display: either all entries or only changed files
  const displayEntries = useMemo<DirEntry[]>(() => {
    if (filterChanged) {
      if (!changedFiles) return [];
      return changedFiles.map((f) => ({
        path: f.path,
        kind: "file" as const,
        size: null,
        pointer_hash: null,
      }));
    }
    // When showing all files, use allEntries if available.
    // Fall back to changed files if allEntries hasn't loaded yet
    // (wtRepo may still be initializing).
    if (allEntries && allEntries.length > 0) return allEntries;
    if (!changedFiles) return [];
    return changedFiles.map((f) => ({
      path: f.path,
      kind: "file" as const,
      size: null,
      pointer_hash: null,
    }));
  }, [filterChanged, changedFiles, allEntries]);

  // The repo context to use for editor operations
  const editorRepo = useMemo<OpenRepo>(() => {
    if (wtRepo) return wtRepo;
    return repo;
  }, [wtRepo, repo]);

  // Open a file in the multi-tab editor
  function openFile(path: string, entry: DirEntry) {
    const key = `file::HEAD::${path}`;
    setEditorTabs((prev) =>
      prev.some((t) => t.key === key)
        ? prev
        : [...prev, { key, path, rev: null, entry }],
    );
    setActiveTabKey(key);
  }

  // Open a diff tab for a changed file
  function openDiff(path: string, entry: DirEntry) {
    const key = `diff::${path}`;
    setEditorTabs((prev) =>
      prev.some((t) => t.key === key)
        ? prev
        : [...prev, { key, path, rev: null, entry }],
    );
    setActiveTabKey(key);
  }

  function closeTab(key: string) {
    setEditorTabs((prev) => prev.filter((t) => t.key !== key));
    setActiveTabKey((prev) => {
      if (prev !== key) return prev;
      const remaining = editorTabs.filter((t) => t.key !== key);
      return remaining.length > 0 ? remaining[remaining.length - 1].key : null;
    });
  }

  function closeAllTabs() {
    setEditorTabs([]);
    setActiveTabKey(null);
  }

  // Handle file selection from the tree
  function handleFileSelect(path: string, entry: DirEntry) {
    // If it's a changed file, open as diff; otherwise open as editor
    if (changedPathSet.has(path)) {
      openDiff(path, entry);
    } else {
      openFile(path, entry);
    }
  }

  // Active tab content
  const isActiveDiff = activeTabKey?.startsWith("diff::");
  const activeDiffPath = isActiveDiff ? activeTabKey!.slice("diff::".length) : null;

  return (
    <div className="flex h-full flex-col bg-gx-bg">
      {/* Header */}
      <div className="flex shrink-0 items-center gap-2 border-b border-gx-border px-3 py-2">
        <GitBranch size={13} className="text-gx-accent" />
        <span className="truncate text-[12px] font-medium text-gx-text">
          {worktree.meta?.display_name || dirName(worktree.path)}
        </span>
        <span className="font-mono text-[11px] text-gx-muted">{branch}</span>
        {status && status.dirty_count > 0 && (
          <span className="rounded bg-gx-warn/15 px-1.5 py-0.5 text-[10px] font-medium text-gx-warn">
            {status.dirty_count} uncommitted
          </span>
        )}
        <div className="ml-auto flex items-center gap-1">
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={fetchChanges}
            title="Refresh"
            aria-label="Refresh changes"
          >
            <RefreshCw size={12} />
          </button>
          <button
            type="button"
            className="rounded bg-gx-accent/15 px-2.5 py-1 text-[11px] font-medium text-gx-accent hover:bg-gx-accent/25 transition"
            onClick={() => setShowMergePanel(true)}
            title="Merge changes back to another branch"
          >
            <GitMerge size={11} className="mr-1 inline -mt-px" />
            Merge Back
          </button>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={onClose}
            title="Close detail panel"
            aria-label="Close"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      {/* Merge panel overlay */}
      {showMergePanel && (
        <MergeBackPanel
          repo={repo}
          worktreeBranch={branch}
          worktreeLabel={worktree.meta?.display_name || dirName(worktree.path)}
          onClose={() => setShowMergePanel(false)}
          onSuccess={() => {
            setShowMergePanel(false);
            fetchChanges();
          }}
        />
      )}

      {/* Main content: file tree + editor/diff */}
      {!showMergePanel && (
        <PanelGroup direction="horizontal" className="min-h-0 flex-1">
          {/* Left: File tree with filter toggle */}
          <Panel defaultSize={25} minSize={15} maxSize={45}>
            <div className="flex h-full flex-col border-r border-gx-border">
              {/* Tree header with filter button */}
              <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-1.5">
                <span className="text-[10px] uppercase tracking-wider text-gx-muted">
                  {filterChanged ? "Changed Files" : "Files"}
                  {filterChanged && changedFiles && (
                    <span className="ml-1 text-gx-text">({changedFiles.length})</span>
                  )}
                </span>
                <button
                  type="button"
                  className={`rounded p-1 transition ${
                    filterChanged
                      ? "bg-gx-accent/15 text-gx-accent"
                      : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                  }`}
                  onClick={() => setFilterChanged(!filterChanged)}
                  title={filterChanged ? "Show all files" : "Show only changed files"}
                  aria-label={filterChanged ? "Show all files" : "Show only changed files"}
                >
                  <Filter size={12} />
                </button>
              </div>

              {/* Tree content */}
              <div className="min-h-0 flex-1 overflow-hidden flex flex-col">
                {(loading || (!filterChanged && !allEntries && !changedFiles)) && (
                  <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
                    <Loader2 size={12} className="animate-spin" /> Loading…
                  </div>
                )}
                {error && (
                  <div className="m-2 rounded border border-gx-error/40 bg-gx-error/10 p-2 text-[11px] text-gx-error">
                    {error}
                  </div>
                )}
                {!loading && filterChanged && changedFiles && changedFiles.length === 0 && (
                  <div className="p-4 text-center text-[11px] text-gx-muted">
                    No changes detected.
                  </div>
                )}
                {displayEntries.length > 0 && (
                  <FileTree
                    entries={displayEntries}
                    gitStatus={gitStatusRecord}
                    onOpenFile={(path, entry) => handleFileSelect(path, entry)}
                    workspacePath={worktree.path}
                    initialExpansion="closed"
                    onRefresh={fetchChanges}
                  />
                )}
              </div>
            </div>
          </Panel>

          <PanelResizeHandle className="w-px cursor-col-resize bg-gx-border hover:bg-gx-accent/50 transition-colors" />

          {/* Right: Multi-tab editor / diff view */}
          <Panel minSize={40}>
            <div className="flex h-full flex-col">
              {editorTabs.length > 0 ? (
                <EditorPane
                  repo={editorRepo}
                  tabs={editorTabs.filter((t) => !t.key.startsWith("diff::"))}
                  diffTabs={editorTabs
                    .filter((t) => t.key.startsWith("diff::"))
                    .map((t) => ({ key: t.key, path: t.path }))}
                  activeKey={activeTabKey}
                  onActivate={setActiveTabKey}
                  onClose={closeTab}
                  onCloseAll={closeAllTabs}
                  pinnedKeys={new Set()}
                  renderDiff={
                    isActiveDiff && activeDiffPath ? (
                      <DiffView repo={editorRepo} path={activeDiffPath} />
                    ) : null
                  }
                />
              ) : (
                <div className="flex h-full flex-col items-center justify-center gap-2 text-gx-muted">
                  <FileDiff size={32} className="opacity-30" />
                  <span className="text-[12px]">Click a file to open it</span>
                  {changedFiles && changedFiles.length > 0 && (
                    <span className="text-[11px] opacity-60">
                      {changedFiles.length} file{changedFiles.length !== 1 ? "s" : ""} changed in this worktree
                    </span>
                  )}
                </div>
              )}
            </div>
          </Panel>
        </PanelGroup>
      )}
    </div>
  );
}

// ── Merge Back Panel ─────────────────────────────────────────────────

interface MergeBackPanelProps {
  repo: OpenRepo;
  worktreeBranch: string;
  worktreeLabel: string;
  onClose: () => void;
  onSuccess: () => void;
}

function MergeBackPanel({ repo, worktreeBranch, worktreeLabel, onClose, onSuccess }: MergeBackPanelProps) {
  const [targets, setTargets] = useState<MergeTarget[]>([]);
  const [loading, setLoading] = useState(true);
  const [selectedTarget, setSelectedTarget] = useState<string>("");
  const [strategy, setStrategy] = useState<MergeStrategy>("MergeCommit");
  const [commitMessage, setCommitMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ success: boolean; message: string } | null>(null);

  useEffect(() => {
    setLoading(true);
    rpc
      .listRefs(repo.repo_id)
      .then((r) => {
        const branches: MergeTarget[] = r.refs
          .filter((ref) => ref.kind === "branch")
          .map((ref) => ({
            name: ref.name.replace(/^refs\/heads\//, ""),
            isCurrent: ref.is_head ?? false,
          }))
          .filter((b) => b.name !== worktreeBranch);
        setTargets(branches);
        const defaultTarget =
          branches.find((b) => b.name === "main") ||
          branches.find((b) => b.name === "master") ||
          branches.find((b) => b.isCurrent) ||
          branches[0];
        if (defaultTarget) setSelectedTarget(defaultTarget.name);
      })
      .catch(() => setTargets([]))
      .finally(() => setLoading(false));
  }, [repo.repo_id, worktreeBranch]);

  async function handleMerge() {
    if (!selectedTarget) return;
    setBusy(true);
    setResult(null);
    try {
      const opResult: OpResult = await rpc.opStart(repo.repo_id, {
        kind: "Merge",
        from: worktreeBranch,
        into: selectedTarget,
        strategy,
        message: commitMessage.trim() || undefined,
      });

      if (opResult.state === "done") {
        setResult({
          success: true,
          message: `Successfully merged "${worktreeBranch}" into "${selectedTarget}"`,
        });
        setTimeout(onSuccess, 1500);
      } else if (opResult.state === "paused") {
        setResult({
          success: false,
          message: `Merge paused due to conflicts in: ${opResult.conflicts?.join(", ") || "unknown files"}. Resolve conflicts in the Conflicts view.`,
        });
      } else {
        setResult({ success: false, message: `Merge ${opResult.state}` });
      }
    } catch (e) {
      setResult({
        success: false,
        message: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="shrink-0 border-b border-gx-border bg-gx-surface px-4 py-3">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <GitMerge size={15} className="text-gx-accent" />
            <span className="text-[13px] font-medium text-gx-text">Merge Back</span>
          </div>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={onClose}
            aria-label="Close merge panel"
          >
            <X size={14} />
          </button>
        </div>
        <p className="mt-1 text-[11px] text-gx-muted">
          Integrate changes from <span className="font-mono text-gx-text">{worktreeBranch}</span> into another branch.
        </p>
      </div>

      <div className="min-h-0 flex-1 overflow-auto p-4">
        {loading ? (
          <div className="flex items-center gap-2 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading branches…
          </div>
        ) : (
          <div className="max-w-md space-y-4">
            {/* Visual merge flow */}
            <div className="rounded-lg border border-gx-border bg-gx-surface p-4">
              <div className="flex items-center gap-3">
                <div className="flex-1">
                  <div className="text-[10px] uppercase tracking-wider text-gx-muted mb-1">From</div>
                  <div className="flex items-center gap-1.5 rounded bg-gx-accent/10 px-2.5 py-1.5">
                    <GitBranch size={12} className="text-gx-accent" />
                    <span className="font-mono text-[11.5px] text-gx-text">{worktreeBranch}</span>
                  </div>
                  <div className="mt-1 text-[10px] text-gx-muted">{worktreeLabel}</div>
                </div>
                <GitPullRequest size={18} className="shrink-0 text-gx-muted" />
                <div className="flex-1">
                  <div className="text-[10px] uppercase tracking-wider text-gx-muted mb-1">Into</div>
                  <select
                    className="w-full rounded border border-gx-border bg-gx-bg-sub px-2.5 py-1.5 font-mono text-[11.5px] text-gx-text focus-visible:outline-none focus:border-gx-accent"
                    value={selectedTarget}
                    onChange={(e) => setSelectedTarget(e.target.value)}
                    disabled={busy}
                  >
                    {targets.map((t) => (
                      <option key={t.name} value={t.name}>
                        {t.name}{t.isCurrent ? " (current)" : ""}
                      </option>
                    ))}
                  </select>
                </div>
              </div>
            </div>

            {/* Strategy selection */}
            <div>
              <label className="mb-1.5 block text-[11px] font-medium text-gx-muted">
                Merge Strategy
              </label>
              <div className="flex gap-1">
                <StrategyButton
                  active={strategy === "MergeCommit"}
                  onClick={() => setStrategy("MergeCommit")}
                  label="Merge Commit"
                  description="Creates a merge commit preserving full history"
                  disabled={busy}
                />
                <StrategyButton
                  active={strategy === "Squash"}
                  onClick={() => setStrategy("Squash")}
                  label="Squash"
                  description="Combines all commits into a single commit"
                  disabled={busy}
                />
                <StrategyButton
                  active={strategy === "FastForwardOnly"}
                  onClick={() => setStrategy("FastForwardOnly")}
                  label="Fast-Forward"
                  description="Only merge if a fast-forward is possible"
                  disabled={busy}
                />
              </div>
            </div>

            {/* Commit message */}
            <div>
              <label className="mb-1.5 block text-[11px] font-medium text-gx-muted">
                Commit Message <span className="text-gx-muted/60">(optional)</span>
              </label>
              <textarea
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-xs text-gx-text placeholder:text-gx-muted/50 focus-visible:outline-none focus:border-gx-accent resize-none"
                rows={3}
                placeholder={`Merge ${worktreeBranch} into ${selectedTarget || "..."}`}
                value={commitMessage}
                onChange={(e) => setCommitMessage(e.target.value)}
                disabled={busy}
              />
            </div>

            {result && (
              <div
                className={`rounded border p-3 text-xs ${
                  result.success
                    ? "border-gx-success/40 bg-gx-success/10 text-gx-success"
                    : "border-gx-error/40 bg-gx-error/10 text-gx-error"
                }`}
              >
                {result.message}
              </div>
            )}

            <button
              type="button"
              className="btn btn-primary w-full justify-center text-xs"
              onClick={handleMerge}
              disabled={busy || !selectedTarget}
            >
              {busy ? (
                <Loader2 size={13} className="animate-spin" />
              ) : (
                <GitMerge size={13} />
              )}
              {busy ? "Merging…" : `Merge into ${selectedTarget || "…"}`}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function StrategyButton({
  active,
  onClick,
  label,
  description,
  disabled,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
  description: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      className={`flex-1 rounded border px-2 py-2 text-left transition ${
        active
          ? "border-gx-accent bg-gx-accent/10 text-gx-text"
          : "border-gx-border bg-gx-surface text-gx-muted hover:border-gx-muted/60 hover:text-gx-text"
      }`}
      onClick={onClick}
      disabled={disabled}
      title={description}
    >
      <div className="text-[11px] font-medium">{label}</div>
      <div className="mt-0.5 text-[9px] leading-tight opacity-70">{description}</div>
    </button>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────

function shortBranch(branch: string): string {
  if (branch.startsWith("refs/heads/")) return branch.slice("refs/heads/".length);
  if (branch.startsWith("refs/")) return branch.slice("refs/".length);
  return branch;
}

function dirName(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] || path;
}
