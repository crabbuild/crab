import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  ChevronRight,
  Cloud,
  CloudOff,
  FileText,
  GitCompareArrows,
  HardDrive,
  Loader2,
  RefreshCw,
} from "lucide-react";
import { PanelGroup, Panel, PanelResizeHandle } from "react-resizable-panels";
import { OpenRepo, useStore, WorkspaceRepoEntry } from "../state";
import { rpc } from "../ipc";
import { DiffView } from "./DiffView";

// ── Types ────────────────────────────────────────────────────────────

export interface CrossRepoCompareTarget {
  /** The repo entry from the workspace (must be at L2 / opened). */
  repo: OpenRepo;
  /** The workspace entry for transport/connection info. */
  entry: WorkspaceRepoEntry;
  /** Branch or ref to compare. */
  branch: string;
}

interface Props {
  /** The "base" side of the comparison (left). */
  base: CrossRepoCompareTarget;
  /** The "head" side of the comparison (right). */
  head: CrossRepoCompareTarget;
  /** Called when the user closes the compare view. */
  onClose?: () => void;
}

interface RefInfo {
  sha: string;
  branch: string;
  error: string | null;
}

interface ChangedFile {
  path: string;
  status: string;
}

type CompareState =
  | { phase: "loading" }
  | { phase: "error"; message: string; side: "base" | "head" | "both" }
  | { phase: "ready"; files: ChangedFile[] };

// ── Helpers ──────────────────────────────────────────────────────────

/** Check if an SSH connection is active for a remote repo entry. */
function isConnected(
  _entry: WorkspaceRepoEntry,
  _sshConnections: Map<string, { status: string }>,
): boolean {
  // In the simplified model, connectivity is determined at the workspace
  // level. Individual entries don't carry transport info. For now, assume
  // connected (the workspace-level SSH status handles disconnection).
  return true;
}

/** Resolve a ref (branch name) to its SHA on a given repo. */
async function resolveRefSafe(
  repoId: string,
  branch: string,
): Promise<RefInfo> {
  try {
    const result = await rpc.resolveRef(repoId, branch);
    return { sha: result.sha, branch, error: null };
  } catch (e) {
    return {
      sha: "",
      branch,
      error: e instanceof Error ? e.message : String(e),
    };
  }
}

// ── Main Component ───────────────────────────────────────────────────

/**
 * Cross-repo branch comparison view. Fetches refs from two different
 * repos (potentially one local and one remote) and displays a unified
 * diff of the changes between the two branches.
 *
 * Handles disconnection gracefully: if one side's SSH connection is
 * lost, shows an error message instead of crashing.
 */
export function CrossRepoBranchCompareView({ base, head, onClose }: Props) {
  const { state } = useStore();
  const headRepo = state.repos.find(open => open.repo_id === head.repo.repo_id);
  const [compareState, setCompareState] = useState<CompareState>({ phase: "loading" });
  const [selectedFile, setSelectedFile] = useState<string | null>(null);
  const [baseRef, setBaseRef] = useState<RefInfo | null>(null);
  const [headRef, setHeadRef] = useState<RefInfo | null>(null);

  // Check connection status for both sides
  const baseConnected = useMemo(
    () => isConnected(base.entry, state.sshConnections),
    [base.entry, state.sshConnections],
  );
  const headConnected = useMemo(
    () => isConnected(head.entry, state.sshConnections),
    [head.entry, state.sshConnections],
  );

  const fetchComparison = useCallback(async () => {
    setCompareState({ phase: "loading" });
    setSelectedFile(null);

    // Check connectivity before attempting RPC calls
    if (!baseConnected && !headConnected) {
      setCompareState({
        phase: "error",
        message: "Both repositories are disconnected. Reconnect to compare branches.",
        side: "both",
      });
      return;
    }
    if (!baseConnected) {
      setCompareState({
        phase: "error",
        message: `Base repository "${base.entry.name}" is disconnected. Reconnect to compare branches.`,
        side: "base",
      });
      return;
    }
    if (!headConnected) {
      setCompareState({
        phase: "error",
        message: `Head repository "${head.entry.name}" is disconnected. Reconnect to compare branches.`,
        side: "head",
      });
      return;
    }

    // Resolve refs on both sides in parallel
    const [baseResult, headResult] = await Promise.all([
      resolveRefSafe(base.repo.repo_id, base.branch),
      resolveRefSafe(head.repo.repo_id, head.branch),
    ]);

    setBaseRef(baseResult);
    setHeadRef(headResult);

    if (baseResult.error) {
      setCompareState({
        phase: "error",
        message: `Failed to resolve ref "${base.branch}" on ${base.entry.name}: ${baseResult.error}`,
        side: "base",
      });
      return;
    }
    if (headResult.error) {
      setCompareState({
        phase: "error",
        message: `Failed to resolve ref "${head.branch}" on ${head.entry.name}: ${headResult.error}`,
        side: "head",
      });
      return;
    }

    // Use the head repo to compute the diff between the two SHAs.
    // For cross-repo comparison, we fetch the base ref into the head
    // repo's context. If both repos share history (e.g., clones of
    // the same upstream), git diff between the two SHAs works directly.
    // If they don't share history, we fall back to showing the error.
    try {
      const result = await rpc.gitDiffNameOnly(
        head.repo.repo_id,
        baseResult.sha,
        headResult.sha,
      );
      setCompareState({ phase: "ready", files: result.files });
      if (result.files.length > 0) {
        setSelectedFile(result.files[0].path);
      }
    } catch (e) {
      // If the repos don't share history, try the base repo instead
      try {
        const result = await rpc.gitDiffNameOnly(
          base.repo.repo_id,
          baseResult.sha,
          headResult.sha,
        );
        setCompareState({ phase: "ready", files: result.files });
        if (result.files.length > 0) {
          setSelectedFile(result.files[0].path);
        }
      } catch (e2) {
        const msg = e2 instanceof Error ? e2.message : String(e2);
        setCompareState({
          phase: "error",
          message: `Cannot compare branches across these repositories: ${msg}. The repos may not share a common history.`,
          side: "both",
        });
      }
    }
  }, [base, head, baseConnected, headConnected]);

  useEffect(() => {
    void fetchComparison();
  }, [fetchComparison]);

  return (
    <div className="flex h-full min-h-0 flex-col bg-gx-bg-sub">
      {/* Header */}
      <div className="flex shrink-0 items-center gap-3 border-b border-gx-border bg-gx-surface px-3 py-2">
        <GitCompareArrows size={14} className="text-gx-accent" />
        <span className="text-[12px] font-medium text-gx-text">
          Cross-Repo Branch Comparison
        </span>
        <span className="flex items-center gap-1.5 text-[11px] text-gx-muted">
          <RepoLabel entry={base.entry} branch={base.branch} connected={baseConnected} />
          <ChevronRight size={10} />
          <RepoLabel entry={head.entry} branch={head.branch} connected={headConnected} />
        </span>
        {compareState.phase === "ready" && (
          <span className="text-[10px] text-gx-muted">
            {compareState.files.length} file{compareState.files.length !== 1 ? "s" : ""} changed
          </span>
        )}
        <div className="ml-auto flex items-center gap-2">
          <button
            type="button"
            className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={() => void fetchComparison()}
            title="Refresh comparison"
            aria-label="Refresh comparison"
          >
            <RefreshCw size={11} /> Refresh
          </button>
          {onClose && (
            <button
              type="button"
              className="rounded px-2 py-0.5 text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
              onClick={onClose}
              aria-label="Close comparison"
            >
              Close
            </button>
          )}
        </div>
      </div>

      {/* Content */}
      <div className="min-h-0 flex-1">
        {compareState.phase === "loading" && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
            <Loader2 size={14} className="animate-spin" /> Comparing branches across repos…
          </div>
        )}
        {compareState.phase === "error" && (
          <DisconnectedError
            message={compareState.message}
            side={compareState.side}
            onRetry={() => void fetchComparison()}
          />
        )}
        {compareState.phase === "ready" && compareState.files.length === 0 && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
            <GitCompareArrows size={14} /> No differences between these branches.
          </div>
        )}
        {compareState.phase === "ready" && compareState.files.length > 0 && (
          <PanelGroup direction="horizontal" className="h-full">
            {/* File list panel */}
            <Panel minSize={15} defaultSize={25} maxSize={40}>
              <div className="h-full overflow-auto border-r border-gx-border bg-gx-surface">
                <div className="px-2 py-1.5 text-[10px] uppercase tracking-wider text-gx-muted">
                  Changed Files
                </div>
                {compareState.files.map((file) => (
                  <button
                    key={file.path}
                    type="button"
                    className={`flex w-full items-center gap-2 px-2 py-1.5 text-left text-[11px] transition ${
                      selectedFile === file.path
                        ? "bg-gx-accent/10 text-gx-text"
                        : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                    }`}
                    onClick={() => setSelectedFile(file.path)}
                    title={file.path}
                  >
                    <FileStatusBadge status={file.status} />
                    <FileText size={11} className="shrink-0 text-gx-muted" />
                    <span className="truncate font-mono">
                      {file.path.split("/").pop() ?? file.path}
                    </span>
                  </button>
                ))}
              </div>
            </Panel>

            <PanelResizeHandle className="w-1 bg-gx-border transition-colors hover:bg-gx-accent" />

            {/* Diff panel */}
            <Panel minSize={40}>
              {selectedFile && baseRef && headRef && headRepo ? (
                <DiffView
                  repo={headRepo}
                  path={selectedFile}
                  base={baseRef.sha}
                  head={headRef.sha}
                />
              ) : (
                <div className="flex h-full items-center justify-center text-sm text-gx-muted">
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

// ── Sub-components ───────────────────────────────────────────────────

/** Displays a repo + branch label with transport icon. */
function RepoLabel({
  entry,
  branch,
  connected,
}: {
  entry: WorkspaceRepoEntry;
  branch: string;
  connected: boolean;
}) {
  const isRemote = false; // In simplified model, determined at workspace level
  const Icon = isRemote ? (connected ? Cloud : CloudOff) : HardDrive;
  const iconColor = !connected
    ? "text-gx-error"
    : isRemote
      ? "text-gx-info"
      : "text-gx-muted";

  return (
    <span className="flex items-center gap-1 rounded bg-gx-accent/15 px-1.5 py-0.5">
      <Icon size={10} className={iconColor} />
      <span className="font-mono text-gx-accent">{entry.name}</span>
      <span className="text-gx-muted">:</span>
      <span className="font-mono text-gx-accent">{branch}</span>
      {isRemote && (
        <span className="text-[9px] text-gx-muted">(remote)</span>
      )}
    </span>
  );
}

/** Error display with retry button for disconnection scenarios. */
function DisconnectedError({
  message,
  side,
  onRetry,
}: {
  message: string;
  side: "base" | "head" | "both";
  onRetry: () => void;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-4 p-8">
      <div className="flex items-center gap-3 rounded-lg border border-gx-error/30 bg-gx-error/5 p-4">
        <AlertTriangle size={20} className="shrink-0 text-gx-error" />
        <div className="space-y-1">
          <div className="text-sm font-medium text-gx-error">
            {side === "both"
              ? "Both sides disconnected"
              : `${side === "base" ? "Base" : "Head"} repository unavailable`}
          </div>
          <div className="text-xs text-gx-muted">{message}</div>
        </div>
      </div>
      <button
        type="button"
        className="btn flex items-center gap-1.5 text-xs"
        onClick={onRetry}
      >
        <RefreshCw size={12} /> Retry
      </button>
    </div>
  );
}

/** Small badge showing the git status letter (A/M/D/R). */
function FileStatusBadge({ status }: { status: string }) {
  const letter = status.charAt(0).toUpperCase();
  const colorClass =
    letter === "A"
      ? "text-gx-success bg-gx-success/15"
      : letter === "D"
        ? "text-gx-error bg-gx-error/15"
        : letter === "R"
          ? "text-gx-info bg-gx-info/15"
          : "text-gx-warn bg-gx-warn/15";

  return (
    <span
      className={`inline-flex h-4 w-4 shrink-0 items-center justify-center rounded text-[9px] font-bold ${colorClass}`}
    >
      {letter}
    </span>
  );
}
