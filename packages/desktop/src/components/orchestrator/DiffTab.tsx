import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
} from "react";
import clsx from "clsx";
import {
  FileTree as PierreFileTree,
  useFileTree,
  useFileTreeSelection,
} from "@pierre/trees/react";
import type { GitStatus, GitStatusEntry } from "@pierre/trees";
import { themeToTreeStyles } from "@pierre/trees";
import {
  Check,
  Cherry,
  ChevronRight,
  Columns2,
  FolderTree,
  GitMerge,
  GitPullRequest,
  List,
  Loader2,
  MessageSquare,
  Package,
  RotateCcw,
  ScrollText,
  Send,
  Sparkles,
  SplitSquareHorizontal,
  Trash2,
  X,
} from "lucide-react";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { SafePatchDiff } from "../SafePatchDiff";
import { CommentGutter } from "../CommentGutter";
import type { CommentsByLine, DiffComment } from "../CommentGutter";
import { useEffectiveTheme } from "../../hooks/useEffectiveTheme";
import { diffOptions } from "../../lib/diffTheme";
import { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

interface DiffFile {
  path: string;
  status?: "added" | "modified" | "deleted" | "renamed";
  additions: number;
  deletions: number;
  /** Unified diff patch content for this file. */
  patch?: string;
  oldPath?: string;
}

interface CommitEntry {
  sha: string;
  message: string;
}

/** Extended comment with resolution state. */
interface EnhancedComment extends DiffComment {
  resolved: boolean;
  filePath: string;
  line: number;
}

/** Per-file comment map: filePath → line → comments. */
type FileComments = Record<string, Record<number, EnhancedComment[]>>;

interface ConflictInfo {
  files: string[];
}

type FileViewMode = "list" | "tree" | "unified";
type DiffRenderMode = "inline" | "side-by-side";

const LS_AGENT_FILE_VIEW = "crab.agentDiff.filesMode";
const LS_AGENT_DIFF_RENDER = "crab.agentDiff.renderMode";

function shouldDeferDiffFetch(status?: string): boolean {
  return status === "running" || status === "iterating" || status === "queued";
}

function loadFileViewMode(): FileViewMode {
  const stored = localStorage.getItem(LS_AGENT_FILE_VIEW);
  if (stored === "list" || stored === "tree" || stored === "unified") return stored;
  return "list";
}

function loadDiffRenderMode(): DiffRenderMode {
  const stored = localStorage.getItem(LS_AGENT_DIFF_RENDER);
  if (stored === "inline" || stored === "side-by-side") return stored;
  return "inline";
}

interface Props {
  task: OrchestratorTask;
  /** The repo handle (kept for potential future use). */
  repo: any;
  /** File path to initially select (e.g., from the live file tree). */
  initialActiveFile?: string | null;
  /** If set, show the diff up to this checkpoint SHA. */
  checkpointSha?: string | null;
  /** If set, diff starts from this SHA (previous checkpoint). Null = use baseRef. */
  checkpointFromSha?: string | null;
  /** Externally-managed set of applied file paths (survives tab switches). */
  appliedFiles?: Set<string>;
  /** Callback to update the applied files set in the parent. */
  onAppliedFilesChange?: (updater: Set<string> | ((prev: Set<string>) => Set<string>)) => void;
  onApplyAll: () => Promise<{ success: boolean; conflicts?: string[] }> | void;
  onCherryPick: (files: string[]) => Promise<{ success: boolean; conflicts?: string[] }> | void;
  onDiscard: () => void;
  onCreatePr?: () => void;
  onReviewWithAi?: () => void;
}

// ── Component ───────────────────────────────────────────────────────

export function DiffTab({ task, repo: _repo, initialActiveFile, checkpointSha, checkpointFromSha, appliedFiles: externalAppliedFiles, onAppliedFilesChange, onApplyAll, onCherryPick, onDiscard, onCreatePr, onReviewWithAi }: Props) {
  const [files, setFiles] = useState<DiffFile[]>([]);
  const [selectedFiles, setSelectedFiles] = useState<Set<string>>(new Set());
  const [activeFile, setActiveFile] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const effective = useEffectiveTheme();

  // View mode: list / tree / unified (matches commit timeline page)
  const [fileViewMode, setFileViewMode] = useState<FileViewMode>(loadFileViewMode);
  // Diff render mode: inline / side-by-side
  const [diffRenderMode, setDiffRenderMode] = useState<DiffRenderMode>(loadDiffRenderMode);

  // Inline comments state
  const [fileComments, setFileComments] = useState<FileComments>({});
  const [showResolved, setShowResolved] = useState(false);

  // Commit filtering
  const [commits, setCommits] = useState<CommitEntry[]>([]);
  const [selectedCommit, setSelectedCommit] = useState<string>("all");

  // Conflict handling
  const [conflicts, setConflicts] = useState<ConflictInfo | null>(null);

  // Applied/merged file tracking — use external state if provided, otherwise fall back to local.
  const [localAppliedFiles, setLocalAppliedFiles] = useState<Set<string>>(new Set());
  const appliedFiles = externalAppliedFiles ?? localAppliedFiles;
  const setAppliedFiles = useCallback(
    (updater: Set<string> | ((prev: Set<string>) => Set<string>)) => {
      if (onAppliedFilesChange) {
        onAppliedFilesChange(updater);
      } else {
        setLocalAppliedFiles((prev) => (typeof updater === "function" ? updater(prev) : updater));
      }
    },
    [onAppliedFilesChange],
  );

  // Unified-mode all-diffs pane ref
  const allDiffsRef = useRef<{ scrollToFile: (path: string) => void } | null>(null);

  // Persist view mode preferences
  useEffect(() => {
    localStorage.setItem(LS_AGENT_FILE_VIEW, fileViewMode);
  }, [fileViewMode]);
  useEffect(() => {
    localStorage.setItem(LS_AGENT_DIFF_RENDER, diffRenderMode);
  }, [diffRenderMode]);

  // Fetch diff file list from the orchestrator IPC.
  // When checkpointSha changes, re-fetch the diff at that checkpoint.
  useEffect(() => {
    if (shouldDeferDiffFetch(task.status)) {
      setFiles([]);
      setActiveFile(null);
      setLoading(false);
      return;
    }

    setLoading(true);
    (window as any).crab.orchestrator
      .getDiff(task.id, checkpointSha || undefined, checkpointFromSha || undefined)
      .then((result: { success?: boolean; diff?: { files: DiffFile[] }; files?: DiffFile[] }) => {
        const diffFiles = result?.diff?.files ?? result?.files ?? [];
        setFiles(diffFiles);
        if (diffFiles.length > 0) {
          setActiveFile(diffFiles[0].path);
        } else {
          setActiveFile(null);
        }
      })
      .catch(() => setFiles([]))
      .finally(() => setLoading(false));
  }, [task.id, task.status, checkpointSha, checkpointFromSha]);

  // When initialActiveFile changes (e.g., user clicked in live file tree),
  // select that file in the diff view if it exists.
  useEffect(() => {
    if (initialActiveFile && files.some((f) => f.path === initialActiveFile)) {
      setActiveFile(initialActiveFile);
    }
  }, [initialActiveFile, files]);

  // Fetch commit list for filtering.
  useEffect(() => {
    (window as any).crab.orchestrator
      .getCommits(task.id)
      .then((result: { success: boolean; commits?: CommitEntry[] }) => {
        if (result.success) {
          setCommits(result.commits ?? []);
        }
      })
      .catch(() => setCommits([]));
  }, [task.id]);

  const toggleFile = useCallback((path: string) => {
    setSelectedFiles((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const handleSelectFile = useCallback((path: string) => {
    setActiveFile(path);
    if (fileViewMode === "unified") {
      allDiffsRef.current?.scrollToFile(path);
    }
  }, [fileViewMode]);

  // Cherry-pick with conflict handling
  const handleCherryPick = useCallback(async () => {
    if (selectedFiles.size === 0) return;
    const filesToApply = [...selectedFiles];
    const result = await onCherryPick(filesToApply);
    if (result && !result.success && result.conflicts) {
      setConflicts({ files: result.conflicts });
    } else if (result && result.success) {
      // Only mark as applied on confirmed success.
      setAppliedFiles((prev) => {
        const next = new Set(prev);
        filesToApply.forEach((f) => next.add(f));
        return next;
      });
      setSelectedFiles(new Set());
    }
    // If result is falsy, void, or { success: false } without conflicts,
    // do nothing — the caller (AgentTasksView) shows a toast with the error.
  }, [selectedFiles, onCherryPick]);

  // Apply all with conflict handling
  const handleApplyAll = useCallback(async () => {
    const result = await onApplyAll();
    if (result && !result.success && result.conflicts) {
      setConflicts({ files: result.conflicts });
    } else if (result && result.success) {
      // Only mark as applied on confirmed success.
      setAppliedFiles(new Set(files.map((f) => f.path)));
    }
    // If result is falsy, void, or { success: false } without conflicts,
    // do nothing — the caller shows a toast with the error.
  }, [onApplyAll, files]);

  // Apply remaining (files not yet cherry-picked)
  const remainingFiles = useMemo(
    () => files.filter((f) => !appliedFiles.has(f.path)),
    [files, appliedFiles],
  );

  const handleApplyRemaining = useCallback(async () => {
    const filePaths = remainingFiles.map((f) => f.path);
    const result = await onCherryPick(filePaths);
    if (result && !result.success && result.conflicts) {
      setConflicts({ files: result.conflicts });
    } else if (result && result.success) {
      setAppliedFiles((prev) => {
        const next = new Set(prev);
        filePaths.forEach((f) => next.add(f));
        return next;
      });
    }
  }, [remainingFiles, onCherryPick]);

  // File revert
  const handleRevertFile = useCallback(async (filePath: string) => {
    try {
      await (window as any).crab.orchestrator.revertFile(task.id, filePath);
      setFiles((prev) => prev.filter((f) => f.path !== filePath));
      if (activeFile === filePath) {
        setActiveFile(files.find((f) => f.path !== filePath)?.path ?? null);
      }
    } catch {
      // Revert failed silently.
    }
  }, [task.id, activeFile, files]);

  // Add comment handler
  const handleAddComment = useCallback(
    (line: number, body: string) => {
      if (!activeFile) return;
      const comment: EnhancedComment = {
        id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        author: "You",
        body,
        created_at: new Date().toISOString(),
        resolved: false,
        filePath: activeFile,
        line,
      };
      setFileComments((prev) => {
        const fileMap = { ...(prev[activeFile] ?? {}) };
        const lineComments = [...(fileMap[line] ?? []), comment];
        fileMap[line] = lineComments;
        return { ...prev, [activeFile]: fileMap };
      });
    },
    [activeFile],
  );

  // Send comment to agent as iteration feedback
  const handleSendToAgent = useCallback(
    (comment: EnhancedComment) => {
      const feedback = `[${comment.filePath}:${comment.line}] ${comment.body}`;
      (window as any).crab.orchestrator.iterate(task.id, feedback);
    },
    [task.id],
  );

  // Resolve comment
  const handleResolveComment = useCallback(
    (commentId: string, filePath: string, line: number) => {
      setFileComments((prev) => {
        const fileMap = { ...(prev[filePath] ?? {}) };
        const lineComments = (fileMap[line] ?? []).map((c) =>
          c.id === commentId ? { ...c, resolved: true } : c,
        );
        fileMap[line] = lineComments;
        return { ...prev, [filePath]: fileMap };
      });
    },
    [],
  );

  // Build CommentsByLine for the active file's DiffView
  const activeCommentsByLine: CommentsByLine = useMemo(() => {
    if (!activeFile || !fileComments[activeFile]) return {};
    const result: CommentsByLine = {};
    for (const [lineStr, comments] of Object.entries(fileComments[activeFile])) {
      const line = Number(lineStr);
      const visible = showResolved
        ? comments
        : comments.filter((c) => !c.resolved);
      if (visible.length > 0) {
        result[line] = visible;
      }
    }
    return result;
  }, [activeFile, fileComments, showResolved]);

  // Keyboard navigation (matches commit timeline page)
  useEffect(() => {
    if (!files.length) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
      const fi = Math.max(0, files.findIndex((f) => f.path === activeFile));
      if (e.key === "j" || e.key === "ArrowDown") {
        e.preventDefault();
        handleSelectFile(files[Math.min(files.length - 1, fi + 1)].path);
      } else if (e.key === "k" || e.key === "ArrowUp") {
        e.preventDefault();
        handleSelectFile(files[Math.max(0, fi - 1)].path);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [files, activeFile, handleSelectFile]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader2 size={20} className="animate-spin text-gx-muted" />
      </div>
    );
  }

  if (files.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-xs text-gx-muted">
        {shouldDeferDiffFetch(task.status)
          ? "Diff will be available after the agent finishes."
          : "No changes to display."}
      </div>
    );
  }

  // Count unresolved comments
  const unresolvedCount = Object.values(fileComments).reduce((total, fileMap) => {
    return total + Object.values(fileMap).reduce((sum, comments) => {
      return sum + comments.filter((c) => !c.resolved).length;
    }, 0);
  }, 0);

  return (
    <div className="flex h-full flex-col">
      {/* Toolbar: summary + view mode toggles + commit filter */}
      <DiffToolbar
        files={files}
        commits={commits}
        selectedCommit={selectedCommit}
        onSelectCommit={setSelectedCommit}
        fileViewMode={fileViewMode}
        onSetFileViewMode={setFileViewMode}
        diffRenderMode={diffRenderMode}
        onSetDiffRenderMode={setDiffRenderMode}
        unresolvedCount={unresolvedCount}
        showResolved={showResolved}
        onToggleResolved={() => setShowResolved((v) => !v)}
        appliedCount={appliedFiles.size}
        totalCount={files.length}
        selectedCount={selectedFiles.size}
        onToggleSelectAll={() => {
          const selectable = files.filter((f) => !appliedFiles.has(f.path));
          if (selectedFiles.size >= selectable.length) {
            setSelectedFiles(new Set());
          } else {
            setSelectedFiles(new Set(selectable.map((f) => f.path)));
          }
        }}
      />

      {/* Conflict banner */}
      {conflicts && (
        <ConflictBanner conflicts={conflicts} onDismiss={() => setConflicts(null)} />
      )}

      {/* Partial apply status */}
      {appliedFiles.size > 0 && remainingFiles.length > 0 && (
        <PartialApplyBanner
          appliedCount={appliedFiles.size}
          remainingCount={remainingFiles.length}
          onApplyRemaining={handleApplyRemaining}
        />
      )}

      {/* Main content: file navigation + diff viewer */}
      <PanelGroup direction="horizontal" className="min-h-0 flex-1">
        {/* Left panel: file navigation (list / tree / unified nav) */}
        <Panel defaultSize={20} minSize={10} maxSize={50}>
          {fileViewMode === "list" ? (
            <DiffFileList
              files={files}
              activeFile={activeFile}
              selectedFiles={selectedFiles}
              appliedFiles={appliedFiles}
              onSelectFile={handleSelectFile}
              onToggleFile={toggleFile}
              onRevertFile={handleRevertFile}
              fileComments={fileComments}
            />
          ) : fileViewMode === "tree" ? (
            <DiffFileTree
              files={files}
              activeFile={activeFile}
              appliedFiles={appliedFiles}
              selectedFiles={selectedFiles}
              onSelectFile={handleSelectFile}
              onToggleFile={toggleFile}
              theme={effective}
            />
          ) : (
            <UnifiedFileNav
              files={files}
              activeFile={activeFile}
              appliedFiles={appliedFiles}
              selectedFiles={selectedFiles}
              onSelectFile={handleSelectFile}
              onToggleFile={toggleFile}
              fileComments={fileComments}
            />
          )}
        </Panel>

        <PanelResizeHandle className="w-[3px] cursor-col-resize bg-transparent transition-colors hover:bg-gx-accent/50" />

        {/* Right panel: diff content */}
        <Panel minSize={30}>
          <div className="h-full min-w-0 overflow-hidden">
            {fileViewMode === "unified" ? (
              <AllDiffsPane
                ref={allDiffsRef}
                files={files}
                activeFile={activeFile}
                appliedFiles={appliedFiles}
                onSelectFile={setActiveFile}
                theme={effective}
                diffRenderMode={diffRenderMode}
                commentsByLine={activeCommentsByLine}
                onAddComment={handleAddComment}
              />
            ) : activeFile ? (
              <div className="flex h-full flex-col">
                <SingleFileDiffContent
                  file={files.find((f) => f.path === activeFile)}
                  appliedFiles={appliedFiles}
                  diffRenderMode={diffRenderMode}
                  commentsByLine={activeCommentsByLine}
                  onAddComment={handleAddComment}
                />
                {/* Inline comment panel for active file */}
                <DiffCommentPanel
                  comments={fileComments[activeFile] ?? {}}
                  showResolved={showResolved}
                  onResolve={(id, line) => handleResolveComment(id, activeFile, line)}
                  onSendToAgent={handleSendToAgent}
                />
              </div>
            ) : (
              <div className="flex h-full items-center justify-center text-xs text-gx-muted">
                Select a file to view its diff.
              </div>
            )}
          </div>
        </Panel>
      </PanelGroup>

      {/* Action buttons (bottom bar) */}
      <div className="flex shrink-0 items-center gap-2 border-t border-gx-border px-4 py-2">
        <button
          type="button"
          onClick={handleApplyAll}
          className="flex items-center gap-1.5 rounded-md bg-gx-success px-3 py-1.5 text-xs font-medium text-white hover:bg-gx-success/85 transition"
        >
          <Check size={12} /> Apply all
        </button>
        <button
          type="button"
          onClick={handleCherryPick}
          disabled={selectedFiles.size === 0}
          className="flex items-center gap-1.5 rounded-md border border-gx-border px-3 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover transition disabled:opacity-50 disabled:cursor-not-allowed"
        >
          <Cherry size={12} /> Apply selected ({selectedFiles.size})
        </button>

        <div className="flex-1" />

        {onReviewWithAi && (
          <button
            type="button"
            onClick={onReviewWithAi}
            className="flex items-center gap-1.5 rounded-md border border-gx-border px-3 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover transition"
          >
            <Sparkles size={12} /> Review with AI
          </button>
        )}
        {onCreatePr && (
          <button
            type="button"
            onClick={onCreatePr}
            className="flex items-center gap-1.5 rounded-md border border-gx-border px-3 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover transition"
          >
            <GitPullRequest size={12} /> Create PR
          </button>
        )}
        <button
          type="button"
          onClick={onDiscard}
          className="flex items-center gap-1.5 rounded-md border border-gx-error/40 px-3 py-1.5 text-xs text-gx-error hover:bg-gx-error/10 transition"
        >
          <Trash2 size={12} /> Discard
        </button>
      </div>
    </div>
  );
}


// ── Toolbar ─────────────────────────────────────────────────────────

function DiffToolbar({
  files,
  commits,
  selectedCommit,
  onSelectCommit,
  fileViewMode,
  onSetFileViewMode,
  diffRenderMode,
  onSetDiffRenderMode,
  unresolvedCount,
  showResolved,
  onToggleResolved,
  appliedCount,
  totalCount,
  selectedCount,
  onToggleSelectAll,
}: {
  files: DiffFile[];
  commits: CommitEntry[];
  selectedCommit: string;
  onSelectCommit: (sha: string) => void;
  fileViewMode: FileViewMode;
  onSetFileViewMode: (mode: FileViewMode) => void;
  diffRenderMode: DiffRenderMode;
  onSetDiffRenderMode: (mode: DiffRenderMode) => void;
  unresolvedCount: number;
  showResolved: boolean;
  onToggleResolved: () => void;
  appliedCount: number;
  totalCount: number;
  selectedCount: number;
  onToggleSelectAll: () => void;
}) {
  const selectableCount = totalCount - appliedCount;

  return (
    <div className="flex shrink-0 items-center gap-3 border-b border-gx-border px-4 py-2 text-[11px] text-gx-muted">
      {/* Select all checkbox */}
      {selectableCount > 0 && (
        <input
          type="checkbox"
          checked={selectedCount > 0 && selectedCount >= selectableCount}
          ref={(el) => {
            if (el) el.indeterminate = selectedCount > 0 && selectedCount < selectableCount;
          }}
          onChange={onToggleSelectAll}
          className="h-3 w-3 rounded border-gx-border accent-gx-accent"
          aria-label={selectedCount >= selectableCount ? "Deselect all files" : "Select all files"}
          title={selectedCount >= selectableCount ? "Deselect all" : "Select all"}
        />
      )}
      {/* File count + stats */}
      <span className="flex items-center gap-1.5">
        <span className="font-medium text-gx-text">{files.length}</span> file{files.length !== 1 ? "s" : ""} changed
      </span>
      <span>
        <span className="font-semibold text-gx-success">
          +{files.reduce((s, f) => s + f.additions, 0)}
        </span>
        {" "}
        <span className="font-semibold text-gx-error">
          −{files.reduce((s, f) => s + f.deletions, 0)}
        </span>
      </span>

      {/* Applied progress indicator */}
      {appliedCount > 0 && (
        <span className="flex items-center gap-1 text-gx-success">
          <GitMerge size={10} />
          <span>{appliedCount}/{totalCount} applied</span>
        </span>
      )}

      {/* Unresolved comment count */}
      {unresolvedCount > 0 && (
        <button
          type="button"
          onClick={onToggleResolved}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-gx-accent hover:bg-gx-accent/10"
          aria-label={showResolved ? "Hide resolved comments" : "Show resolved comments"}
        >
          <MessageSquare size={10} />
          <span>{unresolvedCount} unresolved</span>
        </button>
      )}

      <div className="flex-1" />

      {/* Commit filter dropdown */}
      {commits.length > 1 && (
        <select
          value={selectedCommit}
          onChange={(e) => onSelectCommit(e.target.value)}
          className="rounded border border-gx-border bg-gx-surface px-2 py-0.5 text-[11px] text-gx-text focus:border-gx-accent focus-visible:outline-none"
          aria-label="Filter by commit"
        >
          <option value="all">All commits</option>
          {commits.map((c) => (
            <option key={c.sha} value={c.sha}>
              {c.sha.slice(0, 7)} {c.message.slice(0, 40)}
            </option>
          ))}
        </select>
      )}

      {/* File view mode toggle (list / tree / unified) — matches commit page */}
      <div
        role="radiogroup"
        aria-label="Files view"
        className="flex overflow-hidden rounded border border-gx-border"
      >
        <ModeButton
          active={fileViewMode === "list"}
          icon={<List size={11} />}
          label="List"
          onClick={() => onSetFileViewMode("list")}
        />
        <ModeButton
          active={fileViewMode === "tree"}
          icon={<FolderTree size={11} />}
          label="Tree"
          onClick={() => onSetFileViewMode("tree")}
        />
        <ModeButton
          active={fileViewMode === "unified"}
          icon={<ScrollText size={11} />}
          label="Unified"
          onClick={() => onSetFileViewMode("unified")}
        />
      </div>

      {/* Diff render mode toggle (inline / side-by-side) */}
      <div
        role="radiogroup"
        aria-label="Diff render mode"
        className="flex overflow-hidden rounded border border-gx-border"
      >
        <ModeButton
          active={diffRenderMode === "inline"}
          icon={<SplitSquareHorizontal size={11} />}
          label="Inline"
          onClick={() => onSetDiffRenderMode("inline")}
        />
        <ModeButton
          active={diffRenderMode === "side-by-side"}
          icon={<Columns2 size={11} />}
          label="Side-by-side"
          onClick={() => onSetDiffRenderMode("side-by-side")}
        />
      </div>
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

// ── File List (flat list with checkboxes + applied status) ──────────

function DiffFileList({
  files,
  activeFile,
  selectedFiles,
  appliedFiles,
  onSelectFile,
  onToggleFile,
  onRevertFile,
  fileComments,
}: {
  files: DiffFile[];
  activeFile: string | null;
  selectedFiles: Set<string>;
  appliedFiles: Set<string>;
  onSelectFile: (path: string) => void;
  onToggleFile: (path: string) => void;
  onRevertFile: (path: string) => void;
  fileComments: FileComments;
}) {
  return (
    <aside className="h-full w-full overflow-auto border-r border-gx-border bg-gx-surface">
      <ul>
        {files.map((f) => {
          const isApplied = appliedFiles.has(f.path);
          const isActive = f.path === activeFile;
          const commentCount = Object.values(fileComments[f.path] ?? {}).reduce(
            (sum, comments) => sum + comments.filter((c) => !c.resolved).length,
            0,
          );

          return (
            <li key={f.path}>
              <div
                className={clsx(
                  "group flex items-center gap-2 border-b border-gx-border/40 px-3 py-1.5 cursor-pointer transition",
                  isActive
                    ? "bg-gx-accent/15 text-gx-text"
                    : isApplied
                      ? "bg-gx-success/5 text-gx-muted"
                      : "text-gx-text hover:bg-gx-surface-hover",
                )}
                onClick={() => onSelectFile(f.path)}
              >
                {/* Checkbox for cherry-pick selection */}
                <input
                  type="checkbox"
                  checked={selectedFiles.has(f.path)}
                  onChange={() => onToggleFile(f.path)}
                  onClick={(e) => e.stopPropagation()}
                  disabled={isApplied}
                  className="h-3 w-3 shrink-0 rounded border-gx-border accent-gx-accent"
                  aria-label={`Select ${f.path}`}
                />

                {/* Status indicator */}
                <FileStatusIcon status={f.status} isApplied={isApplied} />

                {/* File info */}
                <div className="min-w-0 flex-1">
                  <div className={clsx(
                    "truncate font-mono text-[12px]",
                    isApplied && "line-through opacity-60",
                  )}>
                    {f.path.split("/").pop()}
                  </div>
                  {f.oldPath && (
                    <div className="truncate font-mono text-[10px] text-gx-muted">
                      ← {f.oldPath}
                    </div>
                  )}
                  {f.path.includes("/") && (
                    <div className="truncate text-[10px] text-gx-muted">
                      {f.path.slice(0, f.path.lastIndexOf("/"))}
                    </div>
                  )}
                </div>

                {/* Right side: stats + indicators */}
                <div className="flex shrink-0 items-center gap-1.5">
                  {/* Applied badge */}
                  {isApplied && (
                    <span className="flex items-center gap-0.5 rounded bg-gx-success/10 px-1 py-0.5 text-[9px] font-medium text-gx-success">
                      <Check size={8} /> Applied
                    </span>
                  )}

                  {/* Comment indicator */}
                  {commentCount > 0 && (
                    <span className="flex items-center gap-0.5 text-[9px] text-gx-accent">
                      <MessageSquare size={9} />
                      {commentCount}
                    </span>
                  )}

                  {/* +/- stats */}
                  <span className="text-[10px]">
                    {f.additions > 0 && (
                      <span className="text-gx-success">+{f.additions}</span>
                    )}
                    {f.additions > 0 && f.deletions > 0 && " "}
                    {f.deletions > 0 && (
                      <span className="text-gx-error">−{f.deletions}</span>
                    )}
                  </span>

                  {/* Revert button (visible on hover) */}
                  {!isApplied && (
                    <button
                      type="button"
                      onClick={(e) => {
                        e.stopPropagation();
                        onRevertFile(f.path);
                      }}
                      className="hidden shrink-0 rounded p-0.5 text-gx-muted hover:bg-gx-error/10 hover:text-gx-error group-hover:block"
                      aria-label={`Revert ${f.path}`}
                      title="Revert file"
                    >
                      <RotateCcw size={10} />
                    </button>
                  )}
                </div>
              </div>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}

// ── File Tree (GitHub-style tree using @pierre/trees) ───────────────

function DiffFileTree({
  files,
  activeFile,
  appliedFiles,
  selectedFiles,
  onSelectFile,
  onToggleFile,
  theme,
}: {
  files: DiffFile[];
  activeFile: string | null;
  appliedFiles: Set<string>;
  selectedFiles: Set<string>;
  onSelectFile: (path: string) => void;
  onToggleFile: (path: string) => void;
  theme: "dark" | "light";
}) {
  const paths = useMemo(() => files.map((f) => f.path).sort(), [files]);
  const byPath = useMemo(() => {
    const m = new Map<string, DiffFile>();
    for (const f of files) m.set(f.path, f);
    return m;
  }, [files]);

  const { model } = useFileTree({
    paths,
    initialExpansion: "open",
    dragAndDrop: false,
    renaming: false,
    icons: { set: "complete", colored: true },
    renderRowDecoration: ({ item }) => {
      if (item.kind === "directory") return null;
      const file = byPath.get(item.path);
      if (!file) return null;
      const isApplied = appliedFiles.has(item.path);
      const isSelected = selectedFiles.has(item.path);
      const parts: string[] = [];
      if (isApplied) parts.push("✓ applied");
      else if (isSelected) parts.push("☑");
      if (file.additions > 0) parts.push(`+${file.additions}`);
      if (file.deletions > 0) parts.push(`−${file.deletions}`);
      if (parts.length === 0) return null;
      return {
        text: parts.join(" "),
        title: `${file.status ?? "modified"} · +${file.additions} −${file.deletions}${isApplied ? " (applied)" : isSelected ? " (selected)" : ""}`,
      };
    },
  });

  useEffect(() => {
    model.resetPaths(paths);
  }, [model, paths]);

  useEffect(() => {
    const status: GitStatusEntry[] = files.map((f) => ({
      path: f.path,
      status: toTreesStatus(f.status, f.additions, f.deletions),
    }));
    model.setGitStatus(status);
  }, [model, files]);

  // Selection → activate file and toggle for cherry-pick on double-click.
  const selection = useFileTreeSelection(model);
  const lastOpened = useRef<string | null>(null);
  useEffect(() => {
    const first = selection[0];
    if (!first || first === lastOpened.current) return;
    if (!byPath.has(first)) return;
    lastOpened.current = first;
    onSelectFile(first);
    // Toggle selection for cherry-pick when a file is activated in tree mode.
    if (!appliedFiles.has(first)) {
      onToggleFile(first);
    }
  }, [selection, byPath, onSelectFile, onToggleFile, appliedFiles]);

  useEffect(() => {
    if (!activeFile) return;
    try {
      model.focusPath(activeFile);
    } catch {
      // Not present; no-op.
    }
  }, [model, activeFile]);

  const themeStyles = useMemo(() => buildTreeTheme(theme), [theme]);

  return (
    <aside className="h-full w-full flex flex-col overflow-hidden border-r border-gx-border bg-gx-surface">
      {/* Selection info bar for tree mode */}
      {selectedFiles.size > 0 && (
        <div className="flex shrink-0 items-center gap-2 border-b border-gx-border/50 px-3 py-1 text-[10px] text-gx-muted">
          <Check size={9} className="text-gx-accent" />
          <span>{selectedFiles.size} selected for apply</span>
        </div>
      )}
      <div className="min-h-0 flex-1">
        <PierreFileTree model={model} className="block h-full" style={themeStyles} />
      </div>
    </aside>
  );
}

function toTreesStatus(
  status: DiffFile["status"],
  additions: number,
  deletions: number,
): GitStatus {
  if (status === "renamed") return "renamed";
  if (status === "added" || (additions > 0 && deletions === 0)) return "added";
  if (status === "deleted" || (additions === 0 && deletions > 0)) return "deleted";
  return "modified";
}

// ── Unified File Nav (compact sidebar for unified/all-diffs mode) ───

function UnifiedFileNav({
  files,
  activeFile,
  appliedFiles,
  selectedFiles,
  onSelectFile,
  onToggleFile,
  fileComments,
}: {
  files: DiffFile[];
  activeFile: string | null;
  appliedFiles: Set<string>;
  selectedFiles: Set<string>;
  onSelectFile: (path: string) => void;
  onToggleFile: (path: string) => void;
  fileComments: FileComments;
}) {
  return (
    <aside className="h-full w-full overflow-auto border-r border-gx-border bg-gx-surface">
      <ul className="py-1">
        {files.map((f) => {
          const isActive = f.path === activeFile;
          const isApplied = appliedFiles.has(f.path);
          const commentCount = Object.values(fileComments[f.path] ?? {}).reduce(
            (sum, comments) => sum + comments.filter((c) => !c.resolved).length,
            0,
          );

          return (
            <li key={f.path}>
              <button
                type="button"
                onClick={() => onSelectFile(f.path)}
                className={clsx(
                  "flex w-full items-center gap-2 px-3 py-1 text-left text-[11px] transition",
                  isActive
                    ? "bg-gx-accent/15 text-gx-text"
                    : isApplied
                      ? "bg-gx-success/5 text-gx-muted"
                      : "text-gx-text hover:bg-gx-surface-hover",
                )}
                title={f.path}
              >
                {/* Checkbox */}
                <input
                  type="checkbox"
                  checked={selectedFiles.has(f.path)}
                  onChange={() => onToggleFile(f.path)}
                  onClick={(e) => e.stopPropagation()}
                  disabled={isApplied}
                  className="h-3 w-3 shrink-0 rounded border-gx-border accent-gx-accent"
                  aria-label={`Select ${f.path}`}
                />

                <FileStatusIcon status={f.status} isApplied={isApplied} />

                <span className={clsx(
                  "min-w-0 flex-1 truncate font-mono",
                  isApplied && "line-through opacity-60",
                )}>
                  {f.path}
                </span>

                <span className="shrink-0 flex items-center gap-1 text-[10px]">
                  {isApplied && (
                    <span className="text-gx-success"><Check size={9} /></span>
                  )}
                  {commentCount > 0 && (
                    <span className="text-gx-accent">{commentCount}</span>
                  )}
                  {f.additions > 0 && (
                    <span className="text-gx-success">+{f.additions}</span>
                  )}
                  {f.deletions > 0 && (
                    <span className="text-gx-error">−{f.deletions}</span>
                  )}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}

// ── File Status Icon ────────────────────────────────────────────────

function FileStatusIcon({ status, isApplied }: { status?: string; isApplied: boolean }) {
  if (isApplied) {
    return (
      <span className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-success/20">
        <Check size={9} className="text-gx-success" />
      </span>
    );
  }

  const color = status === "added"
    ? "text-gx-success"
    : status === "deleted"
      ? "text-gx-error"
      : status === "renamed"
        ? "text-gx-accent"
        : "text-gx-warn";

  const letter = status === "added"
    ? "A"
    : status === "deleted"
      ? "D"
      : status === "renamed"
        ? "R"
        : "M";

  return (
    <span className={clsx(
      "flex h-4 w-4 shrink-0 items-center justify-center rounded text-[9px] font-bold",
      color,
    )}>
      {letter}
    </span>
  );
}

/** Status badge shown in diff headers — matches the status indicator in the file list. */
function FileStatusBadge({ status }: { status?: string }) {
  if (!status) return null;

  const tone = status === "added"
    ? "border-gx-success/30 bg-gx-success/10 text-gx-success"
    : status === "deleted"
      ? "border-gx-error/30 bg-gx-error/10 text-gx-error"
      : status === "renamed"
        ? "border-gx-accent/30 bg-gx-accent/10 text-gx-accent"
        : "border-gx-warn/30 bg-gx-warn/10 text-gx-warn";

  const label = status === "added"
    ? "Added"
    : status === "deleted"
      ? "Deleted"
      : status === "renamed"
        ? "Renamed"
        : "Modified";

  return (
    <span className={clsx(
      "rounded border px-1.5 py-0.5 text-[9px] font-medium",
      tone,
    )}>
      {label}
    </span>
  );
}


// ── All Diffs Pane (unified mode — all files rendered sequentially) ──

interface AllDiffsPaneHandle {
  scrollToFile: (path: string) => void;
}

const AllDiffsPane = forwardRef<
  AllDiffsPaneHandle,
  {
    files: DiffFile[];
    activeFile: string | null;
    appliedFiles: Set<string>;
    onSelectFile: (path: string) => void;
    theme: "dark" | "light";
    diffRenderMode: DiffRenderMode;
    commentsByLine: CommentsByLine;
    onAddComment: (line: number, body: string) => void;
  }
>(function AllDiffsPane(
  { files, activeFile, appliedFiles, onSelectFile, theme, diffRenderMode, commentsByLine, onAddComment },
  ref,
) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const fileRefs = useRef<Map<string, HTMLDivElement>>(new Map());

  useImperativeHandle(ref, () => ({
    scrollToFile(path: string) {
      const el = fileRefs.current.get(path);
      if (el && scrollRef.current) {
        el.scrollIntoView({ behavior: "smooth", block: "start" });
      }
    },
  }));

  const setFileRef = useCallback((path: string, el: HTMLDivElement | null) => {
    if (el) {
      fileRefs.current.set(path, el);
    } else {
      fileRefs.current.delete(path);
    }
  }, []);

  return (
    <div ref={scrollRef} className="h-full overflow-auto bg-gx-bg-sub">
      {files.map((file) => {
        const isApplied = appliedFiles.has(file.path);
        const patch = file.patch ?? "";
        const isActiveFile = file.path === activeFile;

        return (
          <div
            key={file.path}
            ref={(el) => setFileRef(file.path, el)}
            className="border-b border-gx-border"
          >
            {/* Sticky file header */}
            <button
              type="button"
              onClick={() => onSelectFile(file.path)}
              className={clsx(
                "sticky top-0 z-10 flex w-full items-center gap-2 border-b border-gx-border/60 px-4 py-2 text-left backdrop-blur-sm transition",
                isActiveFile
                  ? "bg-gx-accent/10"
                  : "bg-gx-surface/95 hover:bg-gx-surface-hover/95",
              )}
            >
              <FileStatusIcon status={file.status} isApplied={isApplied} />
              <span className={clsx(
                "min-w-0 flex-1 truncate font-mono text-[12px] font-medium text-gx-text",
                isApplied && "line-through opacity-60",
              )}>
                {file.path}
              </span>
              <span className="flex items-center gap-2 text-[10px] text-gx-muted">
                <FileStatusBadge status={file.status} />
                {isApplied && (
                  <span className="rounded bg-gx-success/10 px-1.5 py-0.5 text-[9px] font-medium text-gx-success">
                    Applied
                  </span>
                )}
                {file.additions > 0 && (
                  <span className="font-semibold text-gx-success">+{file.additions}</span>
                )}
                {file.deletions > 0 && (
                  <span className="font-semibold text-gx-error">−{file.deletions}</span>
                )}
                {file.status === "renamed" && file.oldPath && (
                  <span className="rounded border border-gx-border px-1 py-0.5 text-[9px]">← {file.oldPath}</span>
                )}
              </span>
            </button>

            {/* Diff content */}
            {isApplied ? (
              <div className="flex items-center justify-center gap-2 py-6 text-[11px] text-gx-muted">
                <Check size={12} className="text-gx-success" />
                This file has been applied to the main branch.
              </div>
            ) : !patch.trim() ? (
              <div className="flex items-center justify-center gap-2 py-6 text-[11px] text-gx-muted">
                <Package size={12} /> No diff content available.
              </div>
            ) : (
              <DiffRenderer
                patch={patch}
                theme={theme}
                renderMode={diffRenderMode}
              />
            )}
          </div>
        );
      })}

      {/* Comment gutter at the bottom of unified view */}
      {Object.keys(commentsByLine).length > 0 && (
        <CommentGutter
          commentsByLine={commentsByLine}
          onAddComment={onAddComment}
        />
      )}
    </div>
  );
});

// ── Single File Diff Content ────────────────────────────────────────

function SingleFileDiffContent({
  file,
  appliedFiles,
  diffRenderMode,
  commentsByLine,
  onAddComment,
}: {
  file: DiffFile | undefined;
  appliedFiles: Set<string>;
  diffRenderMode: DiffRenderMode;
  commentsByLine: CommentsByLine;
  onAddComment: (line: number, body: string) => void;
}) {
  const effective = useEffectiveTheme();
  const hasComments =
    commentsByLine != null && Object.keys(commentsByLine).length > 0;

  if (!file) {
    return (
      <div className="flex h-full items-center justify-center text-xs text-gx-muted">
        Select a file to view its diff.
      </div>
    );
  }

  const isApplied = appliedFiles.has(file.path);
  const patch = file.patch ?? "";

  if (isApplied) {
    return (
      <div className="flex h-full flex-col">
        {/* File header */}
        <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-4 py-2">
          <FileStatusIcon status={file.status} isApplied={true} />
          <span className="font-mono text-[12px] text-gx-text line-through opacity-60">{file.path}</span>
          <FileStatusBadge status={file.status} />
          <span className="rounded bg-gx-success/10 px-1.5 py-0.5 text-[9px] font-medium text-gx-success">
            Applied
          </span>
          <span className="ml-auto flex items-center gap-2 text-[10px] text-gx-muted">
            {file.additions > 0 && (
              <span className="text-gx-success">+{file.additions}</span>
            )}
            {file.deletions > 0 && (
              <span className="text-gx-error">−{file.deletions}</span>
            )}
          </span>
        </div>
        <div className="flex flex-1 items-center justify-center gap-2 text-sm text-gx-muted">
          <Check size={14} className="text-gx-success" />
          This file has been applied to the main branch.
        </div>
      </div>
    );
  }

  if (!patch.trim()) {
    return (
      <div className="flex h-full flex-col">
        <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-4 py-2">
          <FileStatusIcon status={file.status} isApplied={false} />
          <span className="font-mono text-[12px] text-gx-text">{file.path}</span>
          <FileStatusBadge status={file.status} />
        </div>
        <div className="flex flex-1 items-center justify-center gap-2 text-xs text-gx-muted">
          No diff content available for this file.
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* File header */}
      <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-4 py-2">
        <FileStatusIcon status={file.status} isApplied={false} />
        <span className="font-mono text-[12px] text-gx-text">{file.path}</span>
        <FileStatusBadge status={file.status} />
        <span className="ml-auto flex items-center gap-2 text-[10px] text-gx-muted">
          {file.additions > 0 && (
            <span className="font-semibold text-gx-success">+{file.additions}</span>
          )}
          {file.deletions > 0 && (
            <span className="font-semibold text-gx-error">−{file.deletions}</span>
          )}
        </span>
      </div>

      {/* Diff content */}
      <div className="min-h-0 flex-1 overflow-auto">
        <DiffRenderer
          patch={patch}
          theme={effective}
          renderMode={diffRenderMode}
        />
        {hasComments && (
          <CommentGutter
            commentsByLine={commentsByLine ?? {}}
            onAddComment={onAddComment}
          />
        )}
      </div>
    </div>
  );
}

// ── Diff Renderer (inline / side-by-side) ───────────────────────────

function DiffRenderer({
  patch,
  theme,
  renderMode,
}: {
  patch: string;
  theme: "dark" | "light";
  renderMode: DiffRenderMode;
}) {
  if (renderMode === "side-by-side") {
    return <SideBySideDiff patch={patch} theme={theme} />;
  }

  // Default: inline rendering via SafePatchDiff
  return (
    <SafePatchDiff
      patch={patch}
      className="gx-patch-diff"
      options={diffOptions(theme)}
    />
  );
}

/**
 * Side-by-side diff renderer. Parses the unified patch into left/right
 * columns showing deletions on the left and additions on the right.
 */
function SideBySideDiff({ patch }: { patch: string; theme: "dark" | "light" }) {
  const { leftLines, rightLines } = useMemo(() => parseSideBySide(patch), [patch]);

  return (
    <div className="flex min-w-0 overflow-x-auto font-mono text-[11px] leading-[18px]">
      {/* Left (old) */}
      <div className="min-w-0 flex-1 border-r border-gx-border/50">
        {leftLines.map((line, i) => (
          <div
            key={i}
            className={clsx(
              "flex min-h-[18px] px-2",
              line.type === "removed" && "bg-gx-error/10",
              line.type === "context" && "bg-transparent",
              line.type === "empty" && "bg-gx-bg-sub",
              line.type === "hunk" && "bg-gx-accent/5 text-gx-accent",
            )}
          >
            <span className="w-10 shrink-0 select-none pr-2 text-right text-gx-muted/60">
              {line.lineNo ?? ""}
            </span>
            <span className="w-4 shrink-0 select-none text-center">
              {line.type === "removed" ? (
                <span className="text-gx-error">−</span>
              ) : line.type === "hunk" ? (
                <span className="text-gx-accent">@@</span>
              ) : null}
            </span>
            <span className={clsx(
              "flex-1 whitespace-pre",
              line.type === "removed" && "text-gx-text",
              line.type === "context" && "text-gx-muted",
              line.type === "hunk" && "text-gx-accent",
            )}>
              {line.content}
            </span>
          </div>
        ))}
      </div>

      {/* Right (new) */}
      <div className="min-w-0 flex-1">
        {rightLines.map((line, i) => (
          <div
            key={i}
            className={clsx(
              "flex min-h-[18px] px-2",
              line.type === "added" && "bg-gx-success/10",
              line.type === "context" && "bg-transparent",
              line.type === "empty" && "bg-gx-bg-sub",
              line.type === "hunk" && "bg-gx-accent/5 text-gx-accent",
            )}
          >
            <span className="w-10 shrink-0 select-none pr-2 text-right text-gx-muted/60">
              {line.lineNo ?? ""}
            </span>
            <span className="w-4 shrink-0 select-none text-center">
              {line.type === "added" ? (
                <span className="text-gx-success">+</span>
              ) : line.type === "hunk" ? (
                <span className="text-gx-accent">@@</span>
              ) : null}
            </span>
            <span className={clsx(
              "flex-1 whitespace-pre",
              line.type === "added" && "text-gx-text",
              line.type === "context" && "text-gx-muted",
              line.type === "hunk" && "text-gx-accent",
            )}>
              {line.content}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

interface SideBySideLine {
  type: "added" | "removed" | "context" | "empty" | "hunk";
  content: string;
  lineNo: number | null;
}

/**
 * Parses a unified diff into aligned left/right columns for side-by-side rendering.
 *
 * Adjacent remove/add blocks are paired so that changed lines appear on the same
 * row (old on left, new on right). When the block sizes differ, the shorter side
 * gets empty padding rows. Pure context lines appear on both sides.
 */
function parseSideBySide(patch: string): { leftLines: SideBySideLine[]; rightLines: SideBySideLine[] } {
  const leftLines: SideBySideLine[] = [];
  const rightLines: SideBySideLine[] = [];

  const lines = patch.split("\n");
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;

  // Accumulate consecutive remove/add blocks for alignment.
  let pendingRemoves: { content: string; lineNo: number }[] = [];
  let pendingAdds: { content: string; lineNo: number }[] = [];

  function flushPending() {
    // Pair up removes and adds on the same row.
    const maxLen = Math.max(pendingRemoves.length, pendingAdds.length);
    for (let i = 0; i < maxLen; i++) {
      if (i < pendingRemoves.length) {
        leftLines.push({ type: "removed", content: pendingRemoves[i].content, lineNo: pendingRemoves[i].lineNo });
      } else {
        leftLines.push({ type: "empty", content: "", lineNo: null });
      }
      if (i < pendingAdds.length) {
        rightLines.push({ type: "added", content: pendingAdds[i].content, lineNo: pendingAdds[i].lineNo });
      } else {
        rightLines.push({ type: "empty", content: "", lineNo: null });
      }
    }
    pendingRemoves = [];
    pendingAdds = [];
  }

  for (const line of lines) {
    // Skip diff headers
    if (line.startsWith("diff --git") || line.startsWith("index ") ||
        line.startsWith("---") || line.startsWith("+++") ||
        line.startsWith("new file") || line.startsWith("deleted file") ||
        line.startsWith("old mode") || line.startsWith("new mode")) {
      continue;
    }

    // Hunk header
    if (line.startsWith("@@")) {
      flushPending();
      inHunk = true;
      const match = line.match(/@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (match) {
        oldLine = parseInt(match[1], 10);
        newLine = parseInt(match[2], 10);
      }
      const hunkLabel = line.replace(/^@@.*?@@/, "").trim();
      leftLines.push({ type: "hunk", content: hunkLabel, lineNo: null });
      rightLines.push({ type: "hunk", content: hunkLabel, lineNo: null });
      continue;
    }

    if (!inHunk) continue;

    if (line.startsWith("-")) {
      // If we had pending adds without removes before this, flush them first.
      if (pendingAdds.length > 0 && pendingRemoves.length === 0) {
        flushPending();
      }
      pendingRemoves.push({ content: line.slice(1), lineNo: oldLine });
      oldLine++;
    } else if (line.startsWith("+")) {
      pendingAdds.push({ content: line.slice(1), lineNo: newLine });
      newLine++;
    } else {
      // Context line — flush any pending changes first.
      flushPending();
      const content = line.startsWith(" ") ? line.slice(1) : line;
      leftLines.push({ type: "context", content, lineNo: oldLine });
      rightLines.push({ type: "context", content, lineNo: newLine });
      oldLine++;
      newLine++;
    }
  }

  // Flush any trailing pending lines.
  flushPending();

  return { leftLines, rightLines };
}


// ── Conflict Banner ─────────────────────────────────────────────────

function ConflictBanner({
  conflicts,
  onDismiss,
}: {
  conflicts: ConflictInfo;
  onDismiss: () => void;
}) {
  return (
    <div className="flex items-center gap-2 border-b border-gx-error/30 bg-gx-error/5 px-4 py-2 text-[11px]">
      <span className="font-medium text-gx-error">
        Conflicts in {conflicts.files.length} file{conflicts.files.length !== 1 ? "s" : ""}:
      </span>
      <span className="min-w-0 flex-1 truncate font-mono text-gx-error">
        {conflicts.files.join(", ")}
      </span>
      <a
        href="#"
        onClick={(e) => {
          e.preventDefault();
          (window as any).crab.rpc("conflict_list", {});
        }}
        className="shrink-0 text-gx-accent hover:underline"
      >
        Resolve in Conflicts View
      </a>
      <button
        type="button"
        onClick={onDismiss}
        className="shrink-0 rounded p-0.5 text-gx-muted hover:text-gx-text"
        aria-label="Dismiss conflict banner"
      >
        <X size={12} />
      </button>
    </div>
  );
}

// ── Partial Apply Banner ────────────────────────────────────────────

function PartialApplyBanner({
  appliedCount,
  remainingCount,
  onApplyRemaining,
}: {
  appliedCount: number;
  remainingCount: number;
  onApplyRemaining: () => void;
}) {
  return (
    <div className="flex items-center gap-2 border-b border-gx-accent/30 bg-gx-accent/5 px-4 py-2 text-[11px]">
      <Check size={12} className="text-gx-success" />
      <span className="text-gx-text">
        {appliedCount} file{appliedCount !== 1 ? "s" : ""} applied
      </span>
      <span className="text-gx-muted">·</span>
      <span className="text-gx-muted">
        {remainingCount} file{remainingCount !== 1 ? "s" : ""} remaining
      </span>
      <div className="flex-1" />
      <button
        type="button"
        onClick={onApplyRemaining}
        className="flex items-center gap-1 rounded-md bg-gx-accent/10 px-2 py-1 text-[11px] text-gx-accent hover:bg-gx-accent/20 transition"
      >
        <Check size={10} /> Apply remaining
      </button>
    </div>
  );
}

// ── Comment Panel ───────────────────────────────────────────────────

function DiffCommentPanel({
  comments,
  showResolved,
  onResolve,
  onSendToAgent,
}: {
  comments: Record<number, EnhancedComment[]>;
  showResolved: boolean;
  onResolve: (commentId: string, line: number) => void;
  onSendToAgent: (comment: EnhancedComment) => void;
}) {
  const lines = Object.keys(comments)
    .map(Number)
    .sort((a, b) => a - b);

  const hasVisibleComments = lines.some((line) =>
    comments[line].some((c) => showResolved || !c.resolved),
  );

  if (!hasVisibleComments) return null;

  return (
    <div className="shrink-0 border-t border-gx-border bg-gx-bg-sub overflow-auto max-h-[200px]">
      {lines.map((line) => {
        const lineComments = comments[line].filter(
          (c) => showResolved || !c.resolved,
        );
        if (lineComments.length === 0) return null;

        return (
          <div key={line} className="border-b border-gx-border/50 px-3 py-2">
            <div className="mb-1 flex items-center gap-2 text-[10px] text-gx-muted">
              <MessageSquare size={10} className="text-gx-accent" />
              <span className="font-mono">Line {line}</span>
            </div>
            {lineComments.map((comment) => (
              <CommentEntry
                key={comment.id}
                comment={comment}
                line={line}
                onResolve={onResolve}
                onSendToAgent={onSendToAgent}
              />
            ))}
          </div>
        );
      })}
    </div>
  );
}

function CommentEntry({
  comment,
  line,
  onResolve,
  onSendToAgent,
}: {
  comment: EnhancedComment;
  line: number;
  onResolve: (commentId: string, line: number) => void;
  onSendToAgent: (comment: EnhancedComment) => void;
}) {
  const [collapsed, setCollapsed] = useState(comment.resolved);

  if (comment.resolved) {
    return (
      <div
        className="flex items-center gap-2 rounded px-2 py-1 text-[10px] text-gx-muted/60 cursor-pointer hover:bg-gx-surface-hover"
        onClick={() => setCollapsed(!collapsed)}
      >
        <ChevronRight size={10} className={collapsed ? "" : "rotate-90 transition"} />
        <span className="line-through">{comment.body.slice(0, 60)}</span>
        <span className="ml-auto text-gx-success">✓ Resolved</span>
      </div>
    );
  }

  return (
    <div className="mb-1.5 rounded border border-gx-border bg-gx-surface p-2">
      <div className="flex items-center gap-2 text-[10px]">
        <span className="font-medium text-gx-text">{comment.author}</span>
        <span className="text-gx-muted">{formatTimestamp(comment.created_at)}</span>
      </div>
      <div className="mt-1 whitespace-pre-wrap text-[11px] text-gx-text/80">
        {comment.body}
      </div>
      <div className="mt-1.5 flex items-center gap-2">
        <button
          type="button"
          onClick={() => onSendToAgent(comment)}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-accent hover:bg-gx-accent/10"
          aria-label="Send comment to agent as feedback"
        >
          <Send size={9} /> Send to agent
        </button>
        <button
          type="button"
          onClick={() => onResolve(comment.id, line)}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-success"
          aria-label="Resolve comment"
        >
          <Check size={9} /> Resolve
        </button>
      </div>
    </div>
  );
}

// ── Tree Theme Builder ──────────────────────────────────────────────

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
    "list.hoverBackground": rgb("--gx-surface-hover", mode === "dark" ? "#1e222b" : "#e9ecf3"),
    "list.activeSelectionBackground": rgb("--gx-accent", mode === "dark" ? "#4f8cff" : "#2c6def"),
    "list.activeSelectionForeground": "#ffffff",
    focusBorder: rgb("--gx-accent", mode === "dark" ? "#4f8cff" : "#2c6def"),
    "input.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "input.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "scrollbarSlider.background": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "gitDecoration.addedResourceForeground": rgb("--gx-success", mode === "dark" ? "#4ade80" : "#16a34a"),
    "gitDecoration.modifiedResourceForeground": rgb("--gx-warn", mode === "dark" ? "#facc15" : "#b48700"),
    "gitDecoration.deletedResourceForeground": rgb("--gx-error", mode === "dark" ? "#f87171" : "#dc2626"),
  };
  const styles = themeToTreeStyles({
    type: mode,
    bg: colors["sideBar.background"],
    fg: colors["sideBar.foreground"],
    colors,
  });
  return styles as unknown as React.CSSProperties;
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatTimestamp(iso: string): string {
  try {
    const d = new Date(iso);
    const diffMs = Date.now() - d.getTime();
    if (diffMs < 60_000) return "just now";
    if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
    if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}
