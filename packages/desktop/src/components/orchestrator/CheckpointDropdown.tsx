/**
 * Checkpoint Dropdown — Top-right dropdown panel for navigating agent
 * iteration history (time travel).
 *
 * Replaces the bottom-bar CheckpointTimeline with a compact button in
 * the tab toolbar that opens a resizable dropdown panel showing the
 * full checkpoint timeline with turn details, diffs, and revert actions.
 *
 * Pattern: similar to ContextPinPanel but rendered as a dropdown below
 * the trigger button rather than a full-height side panel.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { ChevronDown, Clock, RotateCcw } from "lucide-react";
import { OrchestratorTask } from "./TaskCard";
import { CheckpointDiffRange } from "./CheckpointTimeline";

// ── Types ───────────────────────────────────────────────────────────

interface Checkpoint {
  ref: string;
  turnNumber: number;
  sha: string;
}

interface Props {
  task: OrchestratorTask;
  selectedTurn: number | null;
  onRevert?: (turnNumber: number) => void;
  onSelectTurn?: (range: CheckpointDiffRange | null) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function CheckpointDropdown({ task, selectedTurn, onRevert, onSelectTurn }: Props) {
  const [open, setOpen] = useState(false);
  const [checkpoints, setCheckpoints] = useState<Checkpoint[]>([]);
  const [loading, setLoading] = useState(true);
  const [reverting, setReverting] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  // Fetch checkpoints on mount and when task status changes.
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

  // Listen for new checkpoints.
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

  // Close dropdown on outside click.
  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

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

  // Don't render if no checkpoints.
  if (loading && checkpoints.length === 0) return null;
  if (!loading && turns.length === 0) return null;

  return (
    <div ref={dropdownRef} className="relative">
      {/* Trigger button */}
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className={clsx(
          "flex items-center gap-1 px-3 py-2 text-xs transition",
          open || selectedTurn != null
            ? "text-gx-accent"
            : "text-gx-muted hover:text-gx-text",
        )}
        title="Checkpoint timeline — time travel between agent turns"
      >
        <Clock size={12} />
        {selectedTurn != null && (
          <span className="text-[10px] tabular-nums">T{selectedTurn}</span>
        )}
        {selectedTurn == null && turns.length > 0 && (
          <span className="text-[10px] tabular-nums">{turns.length}</span>
        )}
        <ChevronDown size={10} className={clsx("transition-transform", open && "rotate-180")} />
      </button>

      {/* Dropdown panel */}
      {open && (
        <div className="absolute right-0 top-full z-50 mt-1 w-72 rounded-lg border border-gx-border bg-gx-surface shadow-xl">
          {/* Header */}
          <div className="flex items-center justify-between border-b border-gx-border px-3 py-2">
            <span className="text-xs font-medium text-gx-text">Checkpoints</span>
            <button
              type="button"
              onClick={() => {
                onSelectTurn?.(null);
              }}
              className={clsx(
                "text-[10px] transition",
                selectedTurn == null
                  ? "text-gx-accent font-medium"
                  : "text-gx-muted hover:text-gx-text",
              )}
            >
              Show all changes
            </button>
          </div>

          {/* Turn list */}
          <div className="max-h-64 overflow-y-auto p-1">
            {turns.map((turn) => {
              const isActive = selectedTurn === turn.turnNumber;

              return (
                <button
                  key={turn.turnNumber}
                  type="button"
                  onClick={() => handleSelect(turn)}
                  disabled={!turn.hasSha}
                  className={clsx(
                    "group flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left transition",
                    isActive
                      ? "bg-gx-accent/10 text-gx-accent"
                      : "text-gx-text hover:bg-gx-hover",
                    !turn.hasSha && "opacity-40 cursor-not-allowed",
                  )}
                >
                  {/* Turn indicator dot */}
                  <span
                    className={clsx(
                      "h-2 w-2 shrink-0 rounded-full",
                      isActive
                        ? "bg-gx-accent"
                        : turn.isLatest
                          ? "bg-gx-success"
                          : "bg-gx-muted/50",
                    )}
                  />

                  {/* Turn info */}
                  <div className="flex flex-1 flex-col min-w-0">
                    <span className="text-xs font-medium">
                      Turn {turn.turnNumber}
                      {turn.isLatest && (
                        <span className="ml-1.5 text-[9px] text-gx-success font-normal">latest</span>
                      )}
                    </span>
                    <span className="text-[10px] font-mono text-gx-muted truncate">
                      {turn.fromSha.slice(0, 7)}
                      {turn.toSha ? ` → ${turn.toSha.slice(0, 7)}` : " → HEAD"}
                    </span>
                  </div>

                  {/* Revert button (not on latest) */}
                  {!turn.isLatest && turn.hasSha && (
                    <button
                      type="button"
                      disabled={reverting}
                      onClick={(e) => {
                        e.stopPropagation();
                        handleRevert(turn.turnNumber);
                      }}
                      className="flex shrink-0 items-center gap-0.5 rounded border border-gx-error/30 bg-gx-error/5 px-1.5 py-0.5 text-[9px] text-gx-error opacity-0 group-hover:opacity-100 hover:bg-gx-error/10 disabled:opacity-50 transition"
                      title={`Revert to turn ${turn.turnNumber}`}
                    >
                      <RotateCcw size={8} /> Revert
                    </button>
                  )}
                </button>
              );
            })}
          </div>

          {/* Footer */}
          <div className="border-t border-gx-border px-3 py-1.5">
            <span className="text-[10px] text-gx-muted">
              {turns.length} checkpoint{turns.length !== 1 ? "s" : ""} · click to view turn diff
            </span>
          </div>
        </div>
      )}
    </div>
  );
}
