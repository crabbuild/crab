import { useCallback, useEffect, useState } from "react";
import { GitCompareArrows, Loader2, ChevronRight, FileText } from "lucide-react";
import { PanelGroup, Panel, PanelResizeHandle } from "react-resizable-panels";
import { OpenRepo } from "../state";
import { rpc } from "../ipc";
import { DiffView } from "./DiffView";

interface Props {
  repo: OpenRepo;
  /** Branch (or ref) of the "base" worktree. */
  baseBranch: string;
  /** Branch (or ref) of the "head" worktree. */
  headBranch: string;
  /** Display name for the base worktree. */
  baseLabel: string;
  /** Display name for the head worktree. */
  headLabel: string;
  /** Called when the user closes the compare view. */
  onClose?: () => void;
}

interface ChangedFile {
  path: string;
  status: string;
}

/**
 * Cross-worktree branch comparison view. Shows a list of changed files
 * between two branches (from different worktrees) on the left, and a
 * split-pane DiffView on the right for the selected file.
 *
 * Reuses the existing DiffView component with base/head refs and the
 * split-view layout from EditorGroups (Pillar L.1) via
 * react-resizable-panels.
 */
export function WorktreeCompareView({
  repo,
  baseBranch,
  headBranch,
  baseLabel,
  headLabel,
  onClose,
}: Props) {
  const [files, setFiles] = useState<ChangedFile[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selectedFile, setSelectedFile] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setFiles(null);
    setSelectedFile(null);

    rpc
      .gitDiffNameOnly(repo.repo_id, baseBranch, headBranch)
      .then((r) => {
        if (!cancelled) {
          setFiles(r.files);
          // Auto-select the first file if available
          if (r.files.length > 0) {
            setSelectedFile(r.files[0].path);
          }
        }
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, baseBranch, headBranch]);

  const handleFileSelect = useCallback((path: string) => {
    setSelectedFile(path);
  }, []);

  return (
    <div className="flex h-full min-h-0 flex-col bg-background">
      {/* Header */}
      <div className="flex shrink-0 items-center gap-3 border-b border-border bg-card px-3 py-2">
        <GitCompareArrows size={14} className="text-primary" />
        <span className="text-[12px] font-medium text-foreground">
          Compare Worktree Branches
        </span>
        <span className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <span className="rounded bg-primary/15 px-1.5 py-0.5 font-mono text-primary">
            {baseLabel}
          </span>
          <ChevronRight size={10} />
          <span className="rounded bg-primary/15 px-1.5 py-0.5 font-mono text-primary">
            {headLabel}
          </span>
        </span>
        {files && (
          <span className="text-[10px] text-muted-foreground">
            {files.length} file{files.length !== 1 ? "s" : ""} changed
          </span>
        )}
        {onClose && (
          <button
            type="button"
            className="ml-auto rounded px-2 py-0.5 text-[11px] text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onClose}
            aria-label="Close comparison"
          >
            Close
          </button>
        )}
      </div>

      {/* Content: split pane with file list on left, diff on right */}
      <div className="min-h-0 flex-1">
        {loading && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
            <Loader2 size={14} className="animate-spin" /> Comparing branches…
          </div>
        )}
        {error && (
          <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {error}
          </div>
        )}
        {!loading && !error && files && files.length === 0 && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
            <GitCompareArrows size={14} /> No differences between these branches.
          </div>
        )}
        {!loading && !error && files && files.length > 0 && (
          <PanelGroup direction="horizontal" className="h-full">
            {/* File list panel */}
            <Panel minSize={15} defaultSize={25} maxSize={40}>
              <div className="h-full overflow-auto border-r border-border bg-card">
                <div className="px-2 py-1.5 text-[10px] uppercase tracking-wider text-muted-foreground">
                  Changed Files
                </div>
                {files.map((file) => (
                  <button
                    key={file.path}
                    type="button"
                    className={`flex w-full items-center gap-2 px-2 py-1.5 text-left text-[11px] transition ${
                      selectedFile === file.path
                        ? "bg-primary/10 text-foreground"
                        : "text-muted-foreground hover:bg-accent hover:text-foreground"
                    }`}
                    onClick={() => handleFileSelect(file.path)}
                    title={file.path}
                  >
                    <FileStatusBadge status={file.status} />
                    <FileText size={11} className="shrink-0 text-muted-foreground" />
                    <span className="truncate font-mono">
                      {file.path.split("/").pop() ?? file.path}
                    </span>
                  </button>
                ))}
              </div>
            </Panel>

            <PanelResizeHandle className="w-1 bg-border hover:bg-primary transition-colors" />

            {/* Diff panel */}
            <Panel minSize={40}>
              {selectedFile ? (
                <DiffView
                  repo={repo}
                  path={selectedFile}
                  base={baseBranch}
                  head={headBranch}
                />
              ) : (
                <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
                  Select a file to view its diff.
                </div>
              )}
            </Panel>
          </PanelGroup>
        )}
      </div>
    </div>
  );
}

/** Small badge showing the git status letter (A/M/D/R). */
function FileStatusBadge({ status }: { status: string }) {
  const letter = status.charAt(0).toUpperCase();
  const colorClass =
    letter === "A"
      ? "text-success bg-success/15"
      : letter === "D"
        ? "text-destructive bg-destructive/15"
        : letter === "R"
          ? "text-info bg-info/15"
          : "text-warning bg-warning/15";

  return (
    <span
      className={`inline-flex h-4 w-4 shrink-0 items-center justify-center rounded text-[9px] font-bold ${colorClass}`}
    >
      {letter}
    </span>
  );
}
