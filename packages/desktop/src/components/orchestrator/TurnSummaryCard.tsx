import { Button } from "@/components/ui/button";
/**
 * Turn-level summary card rendered at the bottom of a completed Turn
 * when the agent edited at least one file.
 *
 * Shows: "Edited N files  +X −Y  [Undo]  [Review]"
 * Body: per-file rows with +X −Y deltas, first three visible by
 * default with a "Show N more files ›" disclosure for the rest.
 * Each file row expands inline to a <UnifiedDiff> of that file's diff.
 */

import {
  ChevronDown,
  ChevronRight,
  Eye,
  FileText,
  SearchCheck,
  ThumbsDown,
  ThumbsUp,
  Undo2,
} from "lucide-react";
import { useCallback, useMemo, useState } from "react";
import type { AgentEvent } from "../../../electron/orchestrator-adapters";
import {
  isPartialDiff,
  isUnifiedDiff,
  synthesizeUnifiedDiff,
} from "../../lib/codex-diff";
import { publishToast } from "../../lib/toasts";
import type { Turn } from "../../lib/turn-derivation";
import { parseDiffStats } from "../../lib/unified-diff-stats";
import { UnifiedDiff } from "./UnifiedDiff";
import { useHunkActions } from "./tool-cards/HunkActionsContext";

// ── Types ───────────────────────────────────────────────────────────

export interface FileEditInfo {
  path: string;
  added: number;
  removed: number;
  /** The raw diff string from the event, used for inline expansion. */
  diff: string;
  /** The event for revert purposes. */
  event: AgentEvent;
}

interface Props {
  turn: Turn;
  /** Called when the user clicks Review — opens the Diff tab. */
  onReview?: () => void;
  /** Called after the user requests a review in the owning conversation. */
  onAgentReview?: () => void;
  /** Called when user rates this turn (thumbs up/down). */
  onFeedback?: (rating: number) => void;
  /** Task ID for agent review RPC. */
  taskId?: string;
}

// ── Helpers ─────────────────────────────────────────────────────────

/** Default number of files shown before the disclosure. */
const VISIBLE_FILE_COUNT = 3;

/**
 * Extract per-file edit info from the turn's events.
 * Groups by path, accumulating stats. Keeps the last event per path
 * for the diff body (later edits to the same file supersede earlier ones).
 */
export function extractFileEdits(events: AgentEvent[]): FileEditInfo[] {
  const byPath = new Map<string, FileEditInfo>();

  for (const event of events) {
    if (event.type !== "file_edit" || !event.path) continue;

    const input = event.input as Record<string, unknown> | undefined;
    const diffField = String(event.diff ?? input?.diff ?? "");
    const stats = parseDiffStats(diffField);

    const existing = byPath.get(event.path);
    if (existing) {
      existing.added += stats.added;
      existing.removed += stats.removed;
      // Keep the latest diff for inline expansion
      if (diffField) existing.diff = diffField;
      existing.event = event;
    } else {
      byPath.set(event.path, {
        path: event.path,
        added: stats.added,
        removed: stats.removed,
        diff: diffField,
        event,
      });
    }
  }

  return [...byPath.values()];
}

// ── Component ───────────────────────────────────────────────────────

export function TurnSummaryCard({ turn, onReview, onAgentReview, onFeedback, taskId }: Props) {
  // Only render when the turn is complete and has file edits
  if (!turn.complete || turn.fileEditCount === 0) return null;

  const [showAll, setShowAll] = useState(false);
  const [expandedFiles, setExpandedFiles] = useState<Set<string>>(new Set());
  const [undoState, setUndoState] = useState<"idle" | "reverting" | "reverted" | "error">("idle");

  const hunkContext = useHunkActions();

  const fileEdits = useMemo(() => extractFileEdits(turn.events), [turn.events]);

  const visibleFiles = showAll
    ? fileEdits
    : fileEdits.slice(0, VISIBLE_FILE_COUNT);
  const hiddenCount = fileEdits.length - VISIBLE_FILE_COUNT;

  const toggleFileExpanded = useCallback((path: string) => {
    setExpandedFiles((prev) => {
      const next = new Set(prev);
      if (next.has(path)) {
        next.delete(path);
      } else {
        next.add(path);
      }
      return next;
    });
  }, []);

  /**
   * Undo action: iterate the turn's file_edit events in reverse arrival
   * order, calling revertHunk for every hunk of every edited file.
   */
  const handleUndo = useCallback(async () => {
    if (!hunkContext?.taskId) return;
    setUndoState("reverting");

    // Collect file_edit events in reverse arrival order
    const fileEditEvents = [...turn.events]
      .filter((e) => e.type === "file_edit" && e.path)
      .reverse();

    let reverted = 0;
    try {
      for (const event of fileEditEvents) {
        const input = event.input as Record<string, unknown> | undefined;
        const filePath = event.path ?? String(input?.file_path ?? input?.path ?? "");
        const oldString = String(input?.old_string ?? "");
        const newString = String(input?.new_string ?? "");
        const content = String(input?.content ?? "");
        const kind = input?.kind as string | undefined;

        const patch = {
          filePath,
          oldString: kind === "add" ? "" : oldString,
          newString: kind === "add" ? content : newString,
        };

        const result = await (window as any).crab?.orchestrator?.revertHunk(
          hunkContext.taskId,
          patch,
        );

        if (!result?.success) {
          throw new Error(result?.error ?? `Could not undo ${filePath}`);
        }
        reverted++;
      }

      setUndoState("reverted");
      publishToast({
        kind: "success",
        title: "File edits undone",
        body: `${fileEditEvents.length} file edit(s) reverted`,
      });
    } catch (err: unknown) {
      setUndoState("error");
      publishToast({
        kind: "error",
        title: "Undo failed",
        body: `${reverted ? `${reverted} edit(s) were undone before the failure. ` : ""}${err instanceof Error ? err.message : String(err)}`,
      });
    }
  }, [turn.events, hunkContext]);

  /**
   * Review action: open the existing Diff tab, scrolling to the first file.
   */
  const handleReview = useCallback(() => {
    onReview?.();
  }, [onReview]);

  /** Review task changes through their conversation owner. */
  const handleAgentReview = useCallback(async () => {
    if (!taskId) return;
    try {
      const result = await window.crab.orchestrator.startReview(taskId);
      if (!result.success) throw new Error(result.error ?? "Could not start agent review");
      publishToast({ kind: "success", title: "Agent review started", body: "The agent will review the task changes in this conversation." });
    } catch (err: unknown) {
      publishToast({ kind: "error", title: "Review failed", body: err instanceof Error ? err.message : "Could not start agent review" });
    }
    onAgentReview?.();
  }, [taskId, onAgentReview]);

  /** Send turn feedback (thumbs up/down) to Codex. */
  const [feedbackSent, setFeedbackSent] = useState(false);
  const handleFeedback = useCallback(async (rating: number) => {
    if (!taskId || feedbackSent) return;
    try {
      const result = await window.crab.orchestrator.uploadFeedback(taskId, { rating, comment: rating >= 4 ? "Good" : "Needs improvement" });
      if (!result.success) throw new Error(result.error ?? "Could not send feedback");
      setFeedbackSent(true);
      onFeedback?.(rating);
    } catch (err: unknown) {
      publishToast({ kind: "error", title: "Feedback failed", body: err instanceof Error ? err.message : "Could not send feedback" });
    }
  }, [taskId, feedbackSent, onFeedback]);

  /** Short filename for display. */
  const shortPath = (fullPath: string) => {
    const parts = fullPath.split("/");
    return parts[parts.length - 1] ?? fullPath;
  };

  /** Directory portion of the path. */
  const dirPath = (fullPath: string) => {
    const idx = fullPath.lastIndexOf("/");
    return idx > 0 ? fullPath.slice(0, idx) : "";
  };

  return (
    <div className="rounded-md border border-border/40 bg-card/30 overflow-hidden">
      {/* Header */}
      <div className="flex items-center gap-2 px-3 py-2">
        <FileText size={12} className="shrink-0 text-primary" />
        <span className="text-[11px] font-medium text-foreground">
          Edited {turn.fileEditCount} file{turn.fileEditCount !== 1 ? "s" : ""}
        </span>

        {/* Header */}
        {(turn.totalAdded > 0 || turn.totalRemoved > 0) && (
          <span className="flex items-center gap-1.5 text-[10px] font-mono">
            {turn.totalAdded > 0 && (
              <span className="text-success">+{turn.totalAdded}</span>
            )}
            {turn.totalRemoved > 0 && (
              <span className="text-destructive">−{turn.totalRemoved}</span>
            )}
          </span>
        )}

        <span className="flex-1" />

        {/* Undo button */}
        {undoState === "reverted" ? (
          <span className="inline-flex items-center gap-0.5 text-[10px] text-warning">
            <Undo2 size={10} /> Reverted
          </span>
        ) : (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            disabled={undoState === "reverting" || !hunkContext?.taskId}
            onClick={() => void handleUndo()}
            className="inline-flex items-center gap-1 rounded border border-destructive/30 bg-destructive/5 px-1.5 py-0.5 text-[10px] text-destructive hover:bg-destructive/15 transition disabled:opacity-50"
            title="Undo these file edits"
          >
            <Undo2 size={10} />
            Undo
          </Button>
        )}

        {/* Feedback buttons */}
        {!feedbackSent ? (
          <>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => handleFeedback(4)}
              className="inline-flex items-center gap-1 rounded border border-success/30 bg-success/5 px-1.5 py-0.5 text-[10px] text-success hover:bg-success/15 transition"
              title="Good result"
            >
              <ThumbsUp size={10} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => handleFeedback(1)}
              className="inline-flex items-center gap-1 rounded border border-destructive/30 bg-destructive/5 px-1.5 py-0.5 text-[10px] text-destructive hover:bg-destructive/15 transition"
              title="Needs improvement"
            >
              <ThumbsDown size={10} />
            </Button>
          </>
        ) : (
          <span className="text-[10px] text-muted-foreground italic">
            Thanks!
          </span>
        )}

        {/* Agent Review button — triggers codex review mode */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={handleAgentReview}
          className="inline-flex items-center gap-1 rounded border border-info/30 bg-info/5 px-1.5 py-0.5 text-[10px] text-info hover:bg-info/15 transition"
          title="Ask the agent to review the task changes"
        >
          <SearchCheck size={10} />
          Agent Review
        </Button>

        {/* Review button — opens Diff tab */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={handleReview}
          className="inline-flex items-center gap-1 rounded border border-primary/30 bg-primary/5 px-1.5 py-0.5 text-[10px] text-primary hover:bg-primary/15 transition"
          title="Open Diff tab to review changes"
        >
          <Eye size={10} />
          Review
        </Button>
      </div>

      {/* File list */}
      <div className="border-t border-border/30">
        {visibleFiles.map((file) => (
          <FileRow
            key={file.path}
            file={file}
            expanded={expandedFiles.has(file.path)}
            onToggle={() => toggleFileExpanded(file.path)}
            shortPath={shortPath(file.path)}
            dirPath={dirPath(file.path)}
          />
        ))}

        {/* "Show N more files" disclosure */}
        {hiddenCount > 0 && !showAll && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setShowAll(true)}
            className="flex w-full items-center gap-1.5 px-3 py-1.5 text-[10px] text-primary hover:bg-accent transition"
          >
            <ChevronRight size={9} />
            Show {hiddenCount} more file{hiddenCount !== 1 ? "s" : ""} ›
          </Button>
        )}
      </div>
    </div>
  );
}

// ── FileRow sub-component ───────────────────────────────────────────

function FileRow({
  file,
  expanded,
  onToggle,
  shortPath: short,
  dirPath: dir,
}: {
  file: FileEditInfo;
  expanded: boolean;
  onToggle: () => void;
  shortPath: string;
  dirPath: string;
}) {
  /** Prepare the diff for rendering via UnifiedDiff. */
  const preparedDiff = useMemo(() => {
    if (!file.diff) return null;
    if (isUnifiedDiff(file.diff) || isPartialDiff(file.diff)) {
      return synthesizeUnifiedDiff(file.diff, file.path, "update");
    }
    // If it's raw +/- lines, synthesize a unified diff
    return synthesizeUnifiedDiff(file.diff, file.path, "update");
  }, [file.diff, file.path]);

  return (
    <div className="border-t border-border/20 first:border-t-0">
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={onToggle}
        className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-accent transition"
        aria-expanded={expanded}
      >
        {expanded ? (
          <ChevronDown size={9} className="shrink-0 text-muted-foreground/60" />
        ) : (
          <ChevronRight size={9} className="shrink-0 text-muted-foreground/60" />
        )}

        {/* File name */}
        <span className="min-w-0 flex-1 flex items-baseline gap-1 truncate">
          <span className="text-[10px] text-foreground/90 font-medium truncate">
            {short}
          </span>
          {dir && (
            <span className="text-[9px] text-muted-foreground/60 truncate">
              {dir}
            </span>
          )}
        </span>

        {/* Per-file +X −Y */}
        {(file.added > 0 || file.removed > 0) && (
          <span className="flex items-center gap-1 shrink-0 text-[9px] font-mono">
            {file.added > 0 && (
              <span className="text-success">+{file.added}</span>
            )}
            {file.removed > 0 && (
              <span className="text-destructive">−{file.removed}</span>
            )}
          </span>
        )}
      </Button>

      {/* Inline diff expansion */}
      {expanded && preparedDiff && (
        <div className="border-t border-border/20 max-h-[320px] overflow-auto bg-background/30">
          <UnifiedDiff patch={preparedDiff} />
        </div>
      )}

      {expanded && !preparedDiff && (
        <div className="border-t border-border/20 px-3 py-2 text-[10px] text-muted-foreground italic">
          No diff available
        </div>
      )}
    </div>
  );
}
