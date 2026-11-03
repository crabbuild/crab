/**
 * Checkpoint Timeline — Navigate agent iteration history.
 *
 * Semantics:
 *   A "turn" is one agent run. The timeline shows what each turn produced.
 *   Checkpoints are created BEFORE each turn runs. Turn N's diff =
 *   checkpoint[N] → checkpoint[N+1] (or HEAD for the latest turn).
 *
 * UI: a minimal horizontal track with a connecting line and one node per
 * turn. Active node glows. Hover reveals turn info and revert option.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { RotateCcw } from "lucide-react";
import clsx from "clsx";
import { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

interface Checkpoint {
  ref: string;
  turnNumber: number;
  sha: string;
}

export interface CheckpointDiffRange {
  fromSha: string;
  toSha: string | null;
  turnNumber: number;
}

interface Props {
  task: OrchestratorTask;
  onRevert?: (turnNumber: number) => void;
  onSelectTurn?: (range: CheckpointDiffRange | null) => void;
  selectedTurn?: number | null;
}

// ── Component ───────────────────────────────────────────────────────

export function CheckpointTimeline({ task, onRevert, onSelectTurn, selectedTurn }: Props) {
  const [checkpoints, setCheckpoints] = useState<Checkpoint[]>([]);
  const [loading, setLoading] = useState(true);
  const [hoveredTurn, setHoveredTurn] = useState<number | null>(null);
  const [reverting, setReverting] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setLoading(true);
    (window as any).crab.orchestrator
      ?.listCheckpoints?.(task.id)
      .then((result: { success: boolean; checkpoints?: Checkpoint[] }) => {
        setCheckpoints(result?.success && result.checkpoints ? result.checkpoints : []);
      })
      .catch(() => setCheckpoints([]))
      .finally(() => setLoading(false));
  }, [task.id, task.status]);

  useEffect(() => {
    const handler = (data: { taskId: string; turnNumber: number; ref: string; sha?: string }) => {
      if (data.taskId !== task.id) return;
      setCheckpoints((prev) => {
        const existing = prev.find((c) => c.turnNumber === data.turnNumber);
        if (existing) {
          if (!existing.sha && data.sha) {
            return prev.map((c) => c.turnNumber === data.turnNumber ? { ...c, sha: data.sha! } : c);
          }
          return prev;
        }
        return [...prev, { ref: data.ref, turnNumber: data.turnNumber, sha: data.sha ?? "" }];
      });
    };
    const unsubscribe = (window as any).crab?.orchestrator?.onCheckpointCreated?.(handler);
    return () => { if (typeof unsubscribe === "function") unsubscribe(); };
  }, [task.id]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollLeft = scrollRef.current.scrollWidth;
    }
  }, [checkpoints.length]);

  const turns = useMemo(() => {
    const sorted = [...checkpoints].sort((a, b) => a.turnNumber - b.turnNumber);
    return sorted.map((cp, idx) => {
      const next = sorted[idx + 1];
      return {
        turnNumber: cp.turnNumber,
        fromSha: cp.sha,
        toSha: next ? next.sha : null,
        isLatest: idx === sorted.length - 1,
        hasSha: !!cp.sha,
      };
    });
  }, [checkpoints]);

  const handleSelect = useCallback((turn: typeof turns[0]) => {
    if (!turn.hasSha) return;
    onSelectTurn?.({
      fromSha: turn.fromSha,
      toSha: turn.toSha,
      turnNumber: turn.turnNumber,
    });
  }, [onSelectTurn]);

  const handleRevert = useCallback(async (turnNumber: number) => {
    setReverting(true);
    try {
      const result = await (window as any).crab.orchestrator?.revertCheckpoint(task.id, turnNumber);
      if (result?.success) {
        onRevert?.(turnNumber);
      }
    } catch {
      // Handled by parent.
    } finally {
      setReverting(false);
    }
  }, [task.id, onRevert]);

  if (loading || turns.length === 0) return null;

  return (
    <div className="flex shrink-0 items-center gap-3 px-4 py-2">
      {/* "All" reset link on the left */}
      <button
        type="button"
        onClick={() => onSelectTurn?.(null)}
        className={clsx(
          "shrink-0 text-[10px] font-medium transition",
          selectedTurn == null
            ? "text-gx-accent"
            : "text-gx-muted hover:text-gx-text",
        )}
      >
        All changes
      </button>

      {/* Separator */}
      <div className="h-3 w-px bg-gx-border/50" />

      {/* Turn nodes — simple inline row, no scroll container */}
      <div
        ref={scrollRef}
        className="flex items-center gap-6 py-1 max-w-full overflow-x-auto scrollbar-none"
        style={{ scrollbarWidth: "none" }}
      >
        {turns.map((turn) => {
            const isActive = selectedTurn === turn.turnNumber;
            const isHovered = hoveredTurn === turn.turnNumber;

            return (
              <div
                key={turn.turnNumber}
                className="relative flex shrink-0 flex-col items-center"
                onMouseEnter={() => setHoveredTurn(turn.turnNumber)}
                onMouseLeave={() => setHoveredTurn(null)}
              >
                {/* Node button — transparent wrapper, only the inner dot is visible */}
                <button
                  type="button"
                  onClick={() => handleSelect(turn)}
                  disabled={!turn.hasSha}
                  className={clsx(
                    "group relative flex h-4 w-4 items-center justify-center bg-transparent border-0 p-0 focus:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent/40 rounded-full",
                    !turn.hasSha && "opacity-40 cursor-not-allowed",
                    turn.hasSha && "cursor-pointer",
                  )}
                  aria-label={`Turn ${turn.turnNumber}`}
                >
                  {/* Main dot — only this is visually rendered */}
                  <span
                    className={clsx(
                      "relative rounded-full transition-colors",
                      isActive
                        ? "h-3 w-3 bg-gx-accent"
                        : turn.isLatest
                          ? "h-2 w-2 bg-gx-success"
                          : "h-2 w-2 bg-gx-muted/50 group-hover:bg-gx-muted",
                    )}
                  />
                </button>

                {/* Label below */}
                <span
                  className={clsx(
                    "absolute top-5 text-[9px] font-mono transition whitespace-nowrap",
                    isActive
                      ? "text-gx-accent font-semibold"
                      : isHovered
                        ? "text-gx-text"
                        : "text-gx-muted/50",
                  )}
                >
                  {turn.turnNumber}
                </span>

                {/* Hover tooltip with revert */}
                {isHovered && turn.hasSha && (
                  <div className="absolute bottom-full left-1/2 z-50 mb-3 flex -translate-x-1/2 items-center gap-2 rounded-md border border-gx-border bg-gx-surface px-2.5 py-1.5 whitespace-nowrap">
                    <div className="flex flex-col items-start">
                      <span className="text-[10px] font-medium text-gx-text">
                        Turn {turn.turnNumber}
                      </span>
                      <span className="text-[9px] text-gx-muted font-mono">
                        {turn.fromSha.slice(0, 7)}
                      </span>
                    </div>
                    {!turn.isLatest && (
                      <button
                        type="button"
                        disabled={reverting}
                        onClick={(e) => { e.stopPropagation(); handleRevert(turn.turnNumber); }}
                        className="flex items-center gap-1 rounded border border-gx-error/30 bg-gx-error/5 px-1.5 py-0.5 text-[9px] text-gx-error hover:bg-gx-error/10 disabled:opacity-50 transition"
                      >
                        <RotateCcw size={8} /> Revert
                      </button>
                    )}

                    {/* Tooltip arrow */}
                    <div className="absolute left-1/2 top-full h-2 w-2 -translate-x-1/2 -translate-y-1 rotate-45 border-b border-r border-gx-border bg-gx-surface" />
                  </div>
                )}
              </div>
            );
          })}
      </div>

      {/* Right: turn counter */}
      <div className="ml-auto shrink-0 text-[10px] tabular-nums text-gx-muted/60">
        {selectedTurn != null ? `Turn ${selectedTurn}` : `${turns.length} turn${turns.length !== 1 ? "s" : ""}`}
      </div>
    </div>
  );
}
