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
import { useWorkspaceSnapshots } from "../../lib/use-workspace-snapshots";
import clsx from "clsx";
import { ChevronDown, Clock, RotateCcw } from "lucide-react";
import { OrchestratorTask } from "./TaskCard";
import { CheckpointDiffRange } from "./CheckpointTimeline";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  task: OrchestratorTask;
  selectedTurn: number | null;
  onRestore?: () => void;
  onSelectTurn?: (range: CheckpointDiffRange | null) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function CheckpointDropdown({ task, selectedTurn, onRestore, onSelectTurn }: Props) {
  const [open, setOpen] = useState(false);
  const { checkpoints, backups, loading, restoring, error, restore, reload } = useWorkspaceSnapshots(task);
  const dropdownRef = useRef<HTMLDivElement>(null);

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
        ref: cp.ref,
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

  const handleRestore = async (sourceRef: string) => {
    if (await restore(sourceRef)) onRestore?.();
  };

  // Don't render if no checkpoints.
  if (loading && checkpoints.length === 0) return null;
  if (!loading && turns.length === 0 && backups.length === 0 && !error) return null;

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
        aria-label="Saved file states"
        title="Saved file states"
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
                <div
                  key={turn.turnNumber}
                  className={clsx(
                    "group flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left transition",
                    isActive
                      ? "bg-gx-accent/10 text-gx-accent"
                      : "text-gx-text hover:bg-gx-hover",
                    !turn.hasSha && "opacity-40 cursor-not-allowed",
                  )}
                >
                  <button type="button" onClick={() => handleSelect(turn)} disabled={!turn.hasSha} className="flex min-w-0 flex-1 items-center gap-2 text-left">
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
                        <span className="ml-1.5 text-[9px] text-gx-success font-normal">live</span>
                      )}
                    </span>
                    <span className="text-[10px] font-mono text-gx-muted truncate">
                      {turn.fromSha.slice(0, 7)}
                      {turn.toSha ? ` → ${turn.toSha.slice(0, 7)}` : " → HEAD"}
                    </span>
                  </div>

                  </button>

                  {/* Restore the saved pre-input file state. */}
                  {turn.hasSha && (
                    <button
                      type="button"
                      disabled={restoring}
                      onClick={(e) => {
                        e.stopPropagation();
                        void handleRestore(turn.ref);
                      }}
                      className="flex shrink-0 items-center gap-0.5 rounded border border-gx-error/30 bg-gx-error/5 px-1.5 py-0.5 text-[9px] text-gx-error opacity-0 group-hover:opacity-100 hover:bg-gx-error/10 disabled:opacity-50 transition"
                      title={`Restore files before turn ${turn.turnNumber}`}
                      aria-label={`Restore files before turn ${turn.turnNumber}`}
                    >
                      <RotateCcw size={8} /> Restore files
                    </button>
                  )}
                </div>
              );
            })}
          </div>

          {error && <div className="px-3 py-2 text-xs text-gx-error"><p role="alert">{error}</p><button type="button" onClick={reload}>Reload saved file states</button></div>}
          {backups.length > 0 && <div className="max-h-48 overflow-y-auto border-t border-gx-border p-2">
            <p className="mb-1 text-xs text-gx-muted">Backups</p>
            {backups.map((backup, index) => <button key={backup.ref} type="button" disabled={restoring} onClick={() => void handleRestore(backup.ref)}
              className="block w-full rounded px-2 py-1 text-left text-xs hover:bg-gx-hover disabled:opacity-50">
              Restore files saved {new Date(backup.createdAt).toLocaleString()} · backup {backups.length - index}
            </button>)}
          </div>}
          {/* Footer */}
          <div className="border-t border-gx-border px-3 py-1.5">
            <span className="text-[10px] text-gx-muted">
              {turns.length} checkpoint{turns.length !== 1 ? "s" : ""} · restore files, keep conversation
            </span>
          </div>
        </div>
      )}
    </div>
  );
}
