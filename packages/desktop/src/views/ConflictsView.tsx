import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import clsx from "clsx";
import {
  AlertTriangle,
  ArrowLeft,
  ArrowRight,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Columns,
  GitBranch,
  GitMerge,
  Loader2,
  Play,
  Square,
  XCircle,
} from "lucide-react";
import { forwardRef, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MenuItemDef } from "../components/ContextMenu";
import { ContextMenu } from "../components/ContextMenu";
import type { ToolbarAction } from "../components/ViewToolbar";
import { ViewToolbar } from "../components/ViewToolbar";
import { getWatchPath, useFileWatcher } from "../hooks/useFileWatcher";
import { OpStatusResult, rpc } from "../ipc";
import type { CommandContext } from "../lib/command-registry";
import { executeCommand } from "../lib/command-registry";
import { publishToast } from "../lib/toasts";
import { OpenRepo } from "../state";

interface ConflictFile {
  path: string;
  status: string;
}

interface Props {
  repo: OpenRepo;
  selectedPath?: string | null;
  onSelectFile?: (path: string) => void;
  /** Notifies parent when conflict count changes. */
  onConflictCountChange?: (count: number) => void;
}

/** Human-readable labels for git conflict status codes. */
const STATUS_LABELS: Record<string, string> = {
  UU: "both modified",
  AA: "both added",
  DD: "both deleted",
  DU: "deleted by us",
  UD: "deleted by them",
  AU: "added by us",
  UA: "added by them",
};

function statusLabel(code: string): string {
  return STATUS_LABELS[code] ?? code;
}

/** Derive a human-friendly operation name from the OpInput kind. */
function opLabel(op: OpStatusResult["op"]): string {
  if (!op) return "operation";
  switch (op.kind) {
    case "Merge":
      return "merge";
    case "Rebase":
      return "rebase";
    case "CherryPick":
      return "cherry-pick";
    default:
      return "operation";
  }
}

/** Get the source branch/ref for the operation. */
function opSource(op: OpStatusResult["op"]): string | null {
  if (!op) return null;
  switch (op.kind) {
    case "Merge":
      return op.from;
    case "Rebase":
      return op.onto;
    case "CherryPick":
      return op.shas?.[0] ?? null;
    default:
      return null;
  }
}

/** Get the target branch for the operation. */
function opTarget(op: OpStatusResult["op"]): string | null {
  if (!op) return null;
  switch (op.kind) {
    case "Merge":
      return op.into;
    case "Rebase":
      return op.branch;
    case "CherryPick":
      return op.target ?? null;
    default:
      return null;
  }
}

/** Map the paused op kind to the CommandContext activeOperation field. */
function activeOperationFromOp(
  op: OpStatusResult["op"],
): "merge" | "rebase" | "cherry-pick" | null {
  if (!op) return null;
  switch (op.kind) {
    case "Merge":
      return "merge";
    case "Rebase":
      return "rebase";
    case "CherryPick":
      return "cherry-pick";
    default:
      return null;
  }
}

/**
 * Context-aware labels for "ours" and "theirs" based on the operation type.
 * During a rebase, "ours" is actually the upstream (onto) branch and "theirs"
 * is the branch being rebased — the opposite of merge semantics.
 */
function oursTheirsLabels(op: OpStatusResult["op"]): { ours: string; theirs: string } {
  if (!op) return { ours: "Ours (current)", theirs: "Theirs (incoming)" };
  switch (op.kind) {
    case "Rebase":
      return {
        ours: `Ours (${op.onto})`,
        theirs: `Theirs (${op.branch})`,
      };
    case "Merge":
      return {
        ours: `Ours (${op.into})`,
        theirs: `Theirs (${op.from})`,
      };
    case "CherryPick":
      return {
        ours: "Ours (current branch)",
        theirs: `Theirs (${op.shas?.[0]?.slice(0, 7) ?? "picked"})`,
      };
    default:
      return { ours: "Ours", theirs: "Theirs" };
  }
}

/** Quick-resolve strategies for different conflict types. */
function quickResolveStrategies(
  status: string,
  labels: { ours: string; theirs: string },
): Array<{ id: string; label: string; tooltip: string }> {
  switch (status) {
    case "DU": // deleted by us — they modified
    case "UA": // added by them
      return [
        { id: "git.conflict.accept-theirs", label: "Theirs", tooltip: `Accept: ${labels.theirs}` },
        { id: "git.conflict.resolve", label: "Delete", tooltip: "Keep file deleted" },
      ];
    case "UD": // deleted by them — we modified
    case "AU": // added by us
      return [
        { id: "git.conflict.accept-ours", label: "Ours", tooltip: `Accept: ${labels.ours}` },
        { id: "git.conflict.resolve", label: "Delete", tooltip: "Keep file deleted" },
      ];
    case "DD": // both deleted
      return [{ id: "git.conflict.resolve", label: "Resolved", tooltip: "Mark as resolved" }];
    default: // UU, AA — both modified/added
      return [
        { id: "git.conflict.accept-ours", label: "Ours", tooltip: `Accept: ${labels.ours}` },
        { id: "git.conflict.accept-theirs", label: "Theirs", tooltip: `Accept: ${labels.theirs}` },
      ];
  }
}

/**
 * Conflicts view — lists conflicting files during a merge, rebase, or
 * cherry-pick with full operation context, inline quick-resolve actions,
 * resolved/unresolved separation, keyboard navigation, and rebase step
 * progress. Integrates with the MergeEditor for detailed resolution.
 */
export function ConflictsView({ repo, selectedPath, onSelectFile, onConflictCountChange }: Props) {
  const [files, setFiles] = useState<ConflictFile[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Paused operation state
  const [pausedOp, setPausedOp] = useState<OpStatusResult | null>(null);
  const [bannerBusy, setBannerBusy] = useState(false);
  const [bannerError, setBannerError] = useState<string | null>(null);

  // Resolved files tracking (session-local)
  const [resolvedFiles, setResolvedFiles] = useState<ConflictFile[]>([]);
  const [showResolved, setShowResolved] = useState(true);

  // Bulk action confirmation
  const [confirmBulk, setConfirmBulk] = useState<"ours" | "theirs" | null>(null);

  // Stable ref for the onConflictCountChange callback to avoid
  // re-creating `refresh` (and triggering an infinite fetch loop)
  // every time the parent re-renders with a new inline function.
  const onConflictCountChangeRef = useRef(onConflictCountChange);
  onConflictCountChangeRef.current = onConflictCountChange;

  // Context menu state
  const [contextMenu, setContextMenu] = useState<{
    position: { x: number; y: number };
    filePath: string;
    status: string;
  } | null>(null);

  // Keyboard selection
  const [selectedIdx, setSelectedIdx] = useState<number>(0);
  const listRef = useRef<HTMLDivElement>(null);
  const rowRefs = useRef<Map<number, HTMLLIElement>>(new Map());

  // ── Data fetching ──────────────────────────────────────────────────

  const refresh = useCallback(() => {
    setLoading(true);
    setError(null);
    setBannerError(null);

    Promise.all([
      rpc.conflictList(repo.repo_id),
      rpc.opStatus(repo.repo_id),
    ])
      .then(([conflictResult, statusResult]) => {
        const newFiles = conflictResult.files;
        setFiles(newFiles);
        onConflictCountChangeRef.current?.(newFiles.length);

        if (statusResult.op_id && (statusResult.state === "Paused" || statusResult.state === "paused")) {
          setPausedOp(statusResult);
        } else {
          setPausedOp(null);
        }
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repo.repo_id]);

  useEffect(refresh, [refresh]);
  useFileWatcher(getWatchPath(repo), refresh);

  // Track resolved files: detect transitions from conflicted → not conflicted
  const prevFilesRef = useRef<ConflictFile[] | null>(null);
  useEffect(() => {
    if (!files) {
      prevFilesRef.current = files;
      return;
    }
    if (!prevFilesRef.current) {
      prevFilesRef.current = files;
      return;
    }

    const currentPaths = new Set(files.map((f) => f.path));

    // Files that were in prev but not in current = newly resolved
    const newlyResolved = prevFilesRef.current.filter((f) => !currentPaths.has(f.path));
    if (newlyResolved.length > 0) {
      setResolvedFiles((prev) => {
        const existing = new Set(prev.map((f) => f.path));
        const additions = newlyResolved.filter((f) => !existing.has(f.path));
        return [...prev, ...additions];
      });
    }

    // Remove from resolved list any files that are back in conflict
    if (files.length > 0) {
      setResolvedFiles((prev) => prev.filter((f) => !currentPaths.has(f.path)));
    }

    prevFilesRef.current = files;
  }, [files]);

  // ── Ours/Theirs labels (context-aware) ─────────────────────────────

  const labels = useMemo(() => oursTheirsLabels(pausedOp?.op ?? null), [pausedOp]);

  // ── CommandContext ─────────────────────────────────────────────────

  const commandContext: CommandContext = useMemo(
    () => ({
      repoId: repo.repo_id,
      repoPath: repo.path,
      currentBranch: "",
      upstream: null,
      ahead: 0,
      behind: 0,
      dirtyCount: files?.length ?? 0,
      hasStagedChanges: false,
      activeOperation: activeOperationFromOp(pausedOp?.op ?? null),
      forgeProvider: null,
    }),
    [repo.repo_id, repo.path, files, pausedOp],
  );

  // ── Toolbar actions ────────────────────────────────────────────────

  const toolbarActions: ToolbarAction[] = useMemo(() => {
    const noActiveOp = commandContext.activeOperation === null;
    const noConflicts = !files || files.length === 0;
    return [
      {
        id: "git.op.continue",
        icon: Play,
        label: "Continue",
        shortcut: "⌘⇧C",
        disabled: noActiveOp || !noConflicts,
      },
      {
        id: "git.op.abort",
        icon: Square,
        label: "Abort",
        shortcut: "⌘⇧A",
        disabled: noActiveOp,
      },
      {
        id: "bulk.accept-all-ours",
        icon: ArrowLeft,
        label: "All Ours",
        disabled: noConflicts,
      },
      {
        id: "bulk.accept-all-theirs",
        icon: ArrowRight,
        label: "All Theirs",
        disabled: noConflicts,
      },
    ];
  }, [commandContext.activeOperation, files]);

  const handleToolbarAction = useCallback(
    async (actionId: string) => {
      if (actionId === "bulk.accept-all-ours") {
        setConfirmBulk("ours");
        return;
      }
      if (actionId === "bulk.accept-all-theirs") {
        setConfirmBulk("theirs");
        return;
      }
      const input = pausedOp?.op_id ?? undefined;
      await executeCommand(actionId, commandContext, input);
      refresh();
    },
    [commandContext, pausedOp, refresh],
  );

  // ── Bulk resolve with confirmation ─────────────────────────────────

  const handleBulkResolve = useCallback(
    async (side: "ours" | "theirs") => {
      setBannerBusy(true);
      setConfirmBulk(null);
      try {
        const { files: conflictFiles } = await rpc.conflictList(repo.repo_id);
        for (const file of conflictFiles) {
          const sides = await rpc.conflictReadSides(repo.repo_id, file.path);
          const content = side === "ours"
            ? (sides.current ?? "")
            : (sides.incoming ?? "");
          await rpc.gitApplyResolution(repo.repo_id, file.path, content);
        }
        const sideLabel = side === "ours" ? labels.ours : labels.theirs;
        publishToast({
          kind: "success",
          title: "All conflicts resolved",
          body: `Accepted "${sideLabel}" for ${conflictFiles.length} file(s)`,
        });
        refresh();
      } catch (e) {
        publishToast({
          kind: "error",
          title: "Bulk resolve failed",
          body: e instanceof Error ? e.message : String(e),
        });
      } finally {
        setBannerBusy(false);
      }
    },
    [repo.repo_id, refresh, labels],
  );

  // ── Inline quick-resolve ───────────────────────────────────────────

  const handleQuickResolve = useCallback(
    async (actionId: string, filePath: string) => {
      try {
        await executeCommand(actionId, commandContext, filePath);
        refresh();
      } catch (e) {
        publishToast({
          kind: "error",
          title: "Resolution failed",
          body: e instanceof Error ? e.message : String(e),
        });
      }
    },
    [commandContext, refresh],
  );

  // ── Context menu ──────────────────────────────────────────────────

  const contextMenuItems: MenuItemDef[] = useMemo(
    () => [
      { id: "open.merge-editor", label: "Open in Merge Editor", icon: Columns },
      { id: "git.conflict.accept-ours", label: `Accept ${labels.ours}`, icon: ArrowLeft, separator: true },
      { id: "git.conflict.accept-theirs", label: `Accept ${labels.theirs}`, icon: ArrowRight },
      { id: "git.conflict.resolve", label: "Mark as Resolved", icon: Check, separator: true },
    ],
    [labels],
  );

  const handleContextMenu = useCallback((e: React.MouseEvent, filePath: string, status: string) => {
    e.preventDefault();
    setContextMenu({ position: { x: e.clientX, y: e.clientY }, filePath, status });
  }, []);

  const handleMenuSelect = useCallback(
    async (actionId: string) => {
      if (!contextMenu) return;
      if (actionId === "open.merge-editor") {
        onSelectFile?.(contextMenu.filePath);
      } else {
        await executeCommand(actionId, commandContext, contextMenu.filePath);
        refresh();
      }
      setContextMenu(null);
    },
    [contextMenu, commandContext, refresh, onSelectFile],
  );

  // ── Continue / Abort handlers ─────────────────────────────────────

  const handleContinue = useCallback(async () => {
    if (!pausedOp?.op_id) return;
    setBannerBusy(true);
    setBannerError(null);
    try {
      const result = await rpc.opContinue(repo.repo_id, pausedOp.op_id);
      if (result.state === "done") {
        publishToast({ kind: "success", title: "Operation completed", body: `${opLabel(pausedOp.op)} finished successfully` });
        setPausedOp(null);
        setResolvedFiles([]);
        refresh();
      } else if (result.state === "paused") {
        publishToast({ kind: "warning", title: "New conflicts", body: "More conflicts surfaced. Resolve them to continue." });
        refresh();
      }
    } catch (e) {
      setBannerError(e instanceof Error ? e.message : String(e));
    } finally {
      setBannerBusy(false);
    }
  }, [pausedOp, repo.repo_id, refresh]);

  const handleAbort = useCallback(async () => {
    if (!pausedOp?.op_id) return;
    setBannerBusy(true);
    setBannerError(null);
    try {
      await rpc.opAbort(repo.repo_id, pausedOp.op_id);
      publishToast({ kind: "success", title: "Operation aborted", body: `${opLabel(pausedOp.op)} was cancelled` });
      setPausedOp(null);
      setResolvedFiles([]);
      refresh();
    } catch (e) {
      setBannerError(e instanceof Error ? e.message : String(e));
    } finally {
      setBannerBusy(false);
    }
  }, [pausedOp, repo.repo_id, refresh]);

  // ── Keyboard navigation ───────────────────────────────────────────

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      const fileCount = files?.length ?? 0;

      // Global shortcuts (with modifier) — work even with 0 files
      if (e.metaKey && e.shiftKey) {
        if (e.key === "C" || e.key === "c") {
          e.preventDefault();
          if (pausedOp && fileCount === 0) void handleContinue();
          return;
        }
        if (e.key === "A" || e.key === "a") {
          e.preventDefault();
          if (pausedOp) void handleAbort();
          return;
        }
      }

      if (fileCount === 0) return;

      switch (e.key) {
        case "ArrowDown": {
          e.preventDefault();
          const next = Math.min(selectedIdx + 1, fileCount - 1);
          setSelectedIdx(next);
          rowRefs.current.get(next)?.scrollIntoView({ block: "nearest" });
          break;
        }
        case "ArrowUp": {
          e.preventDefault();
          const prev = Math.max(selectedIdx - 1, 0);
          setSelectedIdx(prev);
          rowRefs.current.get(prev)?.scrollIntoView({ block: "nearest" });
          break;
        }
        case "Enter": {
          e.preventDefault();
          const file = files?.[selectedIdx];
          if (file) onSelectFile?.(file.path);
          break;
        }
        case " ": {
          e.preventDefault();
          const file = files?.[selectedIdx];
          if (file) {
            void handleQuickResolve("git.conflict.resolve", file.path);
          }
          break;
        }
      }
    },
    [files, selectedIdx, pausedOp, handleContinue, handleAbort, onSelectFile, handleQuickResolve],
  );

  // Keep selectedIdx in bounds
  useEffect(() => {
    if (files && selectedIdx >= files.length) {
      setSelectedIdx(Math.max(0, files.length - 1));
    }
  }, [files, selectedIdx]);

  const count = files?.length ?? 0;

  return (
    <>
      {/* View Toolbar */}
      <ViewToolbar
        title="Conflicts"
        icon={AlertTriangle}
        actions={toolbarActions}
        onAction={handleToolbarAction}
        trailing={
          count > 0 ? (
            <span className="inline-flex h-4 min-w-4 items-center justify-center rounded-full bg-destructive/20 px-1 text-[10px] font-medium text-destructive">
              {count}
            </span>
          ) : undefined
        }
      />

      <div
        ref={listRef}
        className="min-h-0 flex-1 overflow-auto focus:outline-none"
        tabIndex={0}
        onKeyDown={handleKeyDown}
        role="listbox"
        aria-label="Conflict files"
      >
        {/* ── Operation context banner ─────────────────────────────── */}
        {pausedOp && pausedOp.op && count > 0 && (
          <OperationBanner
            op={pausedOp}
            conflictCount={count}
            resolvedCount={resolvedFiles.length}
            labels={labels}
          />
        )}

        {/* ── Operation context banner ─────────────────────────────── */}
        {!loading && pausedOp && files && files.length === 0 && (
          <div className="m-2 rounded-md border border-success/40 bg-success/10 p-3">
            <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-foreground">
              <CheckCircle2 size={13} className="text-success" />
              All conflicts resolved. Ready to continue the{" "}
              {opLabel(pausedOp.op)}.
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="ghost"
                size="compact"
                type="button"
                disabled={bannerBusy}
                onClick={handleContinue}
                className="rounded bg-success px-3 py-1.5 text-[11px] font-medium text-success-foreground transition hover:bg-success/80 disabled:opacity-50"
                aria-label={`Continue ${opLabel(pausedOp.op)}`}
              >
                {bannerBusy ? (
                  <Loader2 size={11} className="inline animate-spin" />
                ) : (
                  <Play size={11} className="inline" />
                )}{" "}
                Continue
              </Button>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                disabled={bannerBusy}
                onClick={handleAbort}
                className="rounded border border-border px-3 py-1.5 text-[11px] font-medium text-muted-foreground transition hover:border-destructive/60 hover:text-destructive disabled:opacity-50"
                aria-label={`Abort ${opLabel(pausedOp.op)}`}
              >
                Abort
              </Button>
            </div>
            {bannerError && (
              <div className="mt-2 flex items-center gap-1 text-[11px] text-destructive">
                <XCircle size={11} /> {bannerError}
              </div>
            )}
          </div>
        )}

        {loading && (
          <div className="flex items-center gap-2 p-4 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Checking for
            conflicts…
          </div>
        )}

        {error && (
          <div className="m-3 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {error}
          </div>
        )}

        {/* ── Empty state ──────────────────────────────────────────── */}
        {!loading && !error && files && files.length === 0 && !pausedOp && (
          <div className="flex flex-col items-center px-6 py-10 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-full bg-success/10">
              <CheckCircle2 size={22} className="text-success" />
            </div>
            <p className="mt-3 text-sm font-medium text-foreground">
              No conflicts
            </p>
            <p className="mt-1 max-w-[240px] text-xs leading-relaxed text-muted-foreground">
              This view activates automatically when a merge, rebase, or
              cherry-pick encounters conflicting changes.
            </p>
            <div className="mt-4 rounded border border-border bg-card px-3 py-2 text-[11px] text-muted-foreground">
              <div className="flex items-center gap-2">
                <GitMerge size={12} className="text-primary" />
                <span>Start a merge or rebase from the Branches view</span>
              </div>
            </div>
          </div>
        )}

        {/* ── Unresolved conflict files ────────────────────────────── */}
        {files && files.length > 0 && (
          <ul role="group" aria-label="Unresolved conflicts">
            {files.map((f, idx) => (
              <ConflictFileRow
                key={f.path}
                ref={(el) => {
                  if (el) rowRefs.current.set(idx, el);
                  else rowRefs.current.delete(idx);
                }}
                file={f}
                isSelected={selectedPath === f.path || selectedIdx === idx}
                isResolved={false}
                labels={labels}
                onSelect={() => {
                  setSelectedIdx(idx);
                }}
                onDoubleClick={() => onSelectFile?.(f.path)}
                onContextMenu={(e) => handleContextMenu(e, f.path, f.status)}
                onQuickResolve={handleQuickResolve}
              />
            ))}
          </ul>
        )}

        {/* ── Resolved files section ───────────────────────────────── */}
        {resolvedFiles.length > 0 && (
          <div className="border-t border-border/60">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="flex w-full items-center gap-1.5 px-3 py-1.5 text-[11px] font-medium text-muted-foreground hover:text-foreground"
              onClick={() => setShowResolved(!showResolved)}
              aria-expanded={showResolved}
            >
              {showResolved ? (
                <ChevronDown size={12} />
              ) : (
                <ChevronRight size={12} />
              )}
              Resolved ({resolvedFiles.length})
            </Button>
            {showResolved && (
              <ul role="group" aria-label="Resolved files">
                {resolvedFiles.map((f) => (
                  <ConflictFileRow
                    key={`resolved-${f.path}`}
                    file={f}
                    isSelected={false}
                    isResolved={true}
                    labels={labels}
                    onSelect={() => {}}
                    onDoubleClick={() => {}}
                    onContextMenu={() => {}}
                    onQuickResolve={handleQuickResolve}
                  />
                ))}
              </ul>
            )}
          </div>
        )}

        {/* ── Keyboard shortcut hints ──────────────────────────────── */}
        {files && files.length > 0 && (
          <div className="border-t border-border/40 px-3 py-2 text-[10px] text-muted-foreground">
            <span className="kbd">Enter</span> open editor
            {" · "}
            <span className="kbd">Space</span> mark resolved
            {" · "}
            <span className="kbd">⌘⇧C</span> continue
            {" · "}
            <span className="kbd">⌘⇧A</span> abort
          </div>
        )}
      </div>

      {/* Context menu */}
      {contextMenu && (
        <ContextMenu
          items={contextMenuItems}
          position={contextMenu.position}
          onSelect={handleMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}

      {/* ── Bulk resolve confirmation dialog ───────────────────────── */}
      {confirmBulk && (
        <Dialog
          open
          onOpenChange={(open) => {
            if (!open) {
              setConfirmBulk(null);
            }
          }}
        >
          <DialogContent
            showCloseButton={false}
            aria-describedby={undefined}
            className="block gap-0 p-0 w-80 rounded-lg border border-border bg-card p-5 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <DialogTitle className="text-sm font-medium text-foreground">
              Accept all {confirmBulk === "ours" ? labels.ours : labels.theirs}?
            </DialogTitle>
            <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
              This will resolve {count} conflicted file{count !== 1 ? "s" : ""}{" "}
              by accepting the{" "}
              <span className="font-medium text-foreground">
                {confirmBulk === "ours" ? "current" : "incoming"}
              </span>{" "}
              version for each file. Individual conflict regions within files
              will not be merged.
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <Button variant="outline"
                type="button"
                className="text-xs"
                onClick={() => setConfirmBulk(null)}
                autoFocus
              >
                Cancel
              </Button>
              <Button variant="default"
                type="button"
                className={clsx(
 " text-xs",

 )}
                onClick={() => void handleBulkResolve(confirmBulk)}
              >
                Accept All
              </Button>
            </div>
          </DialogContent>
        </Dialog>
      )}
    </>
  );
}

// ── Operation Context Banner ─────────────────────────────────────────

interface OperationBannerProps {
  op: OpStatusResult;
  conflictCount: number;
  resolvedCount: number;
  labels: { ours: string; theirs: string };
}

function OperationBanner({ op, conflictCount, resolvedCount, labels }: OperationBannerProps) {
  const opType = opLabel(op.op);
  const source = opSource(op.op);
  const target = opTarget(op.op);

  // Rebase step progress
  const stepMatch = op.paused_reason?.match(/(\d+)\s*(?:of|\/)\s*(\d+)/);
  const stepCurrent = stepMatch ? parseInt(stepMatch[1], 10) : null;
  const stepTotal = stepMatch ? parseInt(stepMatch[2], 10) : null;

  const total = conflictCount + resolvedCount;
  const progressPct = total > 0 ? (resolvedCount / total) * 100 : 0;

  return (
    <div className="border-b border-border bg-card px-3 py-2.5">
      {/* Operation type + branch info */}
      <div className="flex items-center gap-2 text-xs">
        <GitMerge size={13} className="shrink-0 text-warning" />
        <span className="font-medium capitalize text-foreground">
          {opType} in progress
        </span>
        {source && target && (
          <span className="truncate text-muted-foreground">
            <span className="font-mono text-primary">{source}</span>
            {" → "}
            <span className="font-mono text-primary">{target}</span>
          </span>
        )}
      </div>

      {/* Progress info */}
      <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
        <span className="flex items-center gap-1">
          <AlertTriangle size={10} className="text-destructive" />
          {conflictCount} remaining
        </span>
        {resolvedCount > 0 && (
          <span className="flex items-center gap-1">
            <CheckCircle2 size={10} className="text-success" />
            {resolvedCount} resolved
          </span>
        )}
        {stepCurrent !== null && stepTotal !== null && (
          <span className="flex items-center gap-1">
            <GitBranch size={10} />
            Step {stepCurrent}/{stepTotal}
          </span>
        )}
      </div>

      {/* Ours/Theirs legend */}
      <div className="mt-1.5 flex items-center gap-3 text-[10px] text-muted-foreground">
        <span className="flex items-center gap-1">
          <ArrowLeft size={9} /> {labels.ours}
        </span>
        <span className="flex items-center gap-1">
          <ArrowRight size={9} /> {labels.theirs}
        </span>
      </div>

      {/* Progress bar */}
      {total > 0 && (
        <div className="mt-2 h-1 w-full overflow-hidden rounded-full bg-border">
          <div
            className="h-full rounded-full bg-success transition-all duration-300"
            style={{ width: `${progressPct}%` }}
          />
        </div>
      )}
    </div>
  );
}

// ── Conflict File Row ────────────────────────────────────────────────

interface ConflictFileRowProps {
  file: ConflictFile;
  isSelected: boolean;
  isResolved: boolean;
  labels: { ours: string; theirs: string };
  onSelect: () => void;
  onDoubleClick: () => void;
  onContextMenu: (e: React.MouseEvent) => void;
  onQuickResolve: (actionId: string, filePath: string) => void;
}

const ConflictFileRow = forwardRef<HTMLLIElement, ConflictFileRowProps>(
  function ConflictFileRow(
    { file, isSelected, isResolved, labels, onSelect, onDoubleClick, onContextMenu, onQuickResolve },
    ref,
  ) {
    const strategies = quickResolveStrategies(file.status, labels);
    const fileName = file.path.split("/").pop() ?? file.path;
    const dirPath = file.path.includes("/")
      ? file.path.slice(0, file.path.lastIndexOf("/"))
      : "";

    return (
      <li
        ref={ref}
        role="option"
        aria-selected={isSelected}
        className={clsx(
          "group flex cursor-pointer items-center gap-2 border-b border-border/40 px-3 py-2 text-[12.5px] transition",
          isResolved
            ? "opacity-50"
            : isSelected
              ? "bg-primary/10"
              : "hover:bg-accent",
        )}
        onClick={onSelect}
        onDoubleClick={onDoubleClick}
        onContextMenu={onContextMenu}
        tabIndex={-1}
      >
        {/* Status icon */}
        {isResolved ? (
          <CheckCircle2 size={14} className="shrink-0 text-success" />
        ) : (
          <AlertTriangle size={14} className="shrink-0 text-warning" />
        )}

        {/* File path */}
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline gap-1">
            <span className="truncate font-mono text-[11.5px] text-foreground" title={file.path}>
              {fileName}
            </span>
            {dirPath && (
              <span className="truncate text-[10px] text-muted-foreground">
                {dirPath}/
              </span>
            )}
          </div>
        </div>

        {/* Status chip */}
        <span
          className={clsx(
            "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium",
            isResolved
              ? "bg-success/15 text-success"
              : "bg-destructive/15 text-destructive",
          )}
          title={`${file.status} — ${statusLabel(file.status)}`}
        >
          {isResolved ? "resolved" : statusLabel(file.status)}
        </span>

        {/* Inline quick-resolve buttons (visible on hover or when selected) */}
        {!isResolved && (
          <div
            className={clsx(
              "ml-1 flex shrink-0 gap-0.5 transition-opacity",
              isSelected ? "opacity-100" : "opacity-0 group-hover:opacity-100",
            )}
          >
            {strategies.map((s) => (
              <Button
                variant="ghost"
                size="compact"
                key={s.id}
                type="button"
                className="rounded px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground hover:bg-primary/15 hover:text-primary"
                onClick={(e) => {
                  e.stopPropagation();
                  onQuickResolve(s.id, file.path);
                }}
                title={s.tooltip}
                aria-label={s.tooltip}
              >
                {s.label}
              </Button>
            ))}
          </div>
        )}
      </li>
    );
  },
);
