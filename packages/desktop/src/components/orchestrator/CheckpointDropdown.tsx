import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
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

import clsx from "clsx";
import { ChevronDown, Clock, RotateCcw } from "lucide-react";
import { useCallback, useMemo, useRef, useState } from "react";
import { useWorkspaceSnapshots } from "../../lib/use-workspace-snapshots";
import { CheckpointDiffRange } from "./CheckpointTimeline";
import { OrchestratorTask } from "./TaskCard";

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

  const handleSelect = useCallback(
    (turn: (typeof turns)[0]) => {
    if (!turn.hasSha) return;
    onSelectTurn?.({
      fromSha: turn.fromSha,
      toSha: turn.toSha,
      turnNumber: turn.turnNumber,
    });
  },
    [onSelectTurn],
  );

  const handleRestore = async (sourceRef: string) => {
    if (await restore(sourceRef)) onRestore?.();
  };

  // Don't render if no checkpoints.
  if (loading && checkpoints.length === 0) return null;
  if (!loading && turns.length === 0 && backups.length === 0 && !error) return null;

  return (
    <div ref={dropdownRef} className="relative">
      {/* Trigger button */}
      <DropdownMenu open={open} onOpenChange={setOpen}>
        <DropdownMenuTrigger asChild>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setOpen(!open)}
            className={clsx(
          "flex items-center gap-1 px-3 py-2 text-xs transition",
          open || selectedTurn != null
            ? "text-primary"
            : "text-muted-foreground hover:text-foreground",
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
          </Button>
        </DropdownMenuTrigger>

        {/* Dropdown panel */}
        {open && (
          <DropdownMenuContent
            side="bottom"
            align="end"
            className=" w-72 rounded-lg border border-border bg-card "
          >
            <DropdownMenuGroup>
              {/* Header */}
              <div className="flex items-center justify-between border-b border-border px-3 py-2">
                <span className="text-xs font-medium text-foreground">
                  Checkpoints
                </span>
                <DropdownMenuItem
                  onSelect={() => {
                onSelectTurn?.(null);
              }}
                  className={clsx(
                "text-[10px] transition",
                selectedTurn == null
                  ? "text-primary font-medium"
                  : "text-muted-foreground hover:text-foreground",
              )}
                >
                  Show all changes
                </DropdownMenuItem>
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
                      ? "bg-primary/10 text-primary"
                      : "text-foreground hover:bg-accent",
                    !turn.hasSha && "opacity-40 cursor-not-allowed",
                  )}
                >
                      <DropdownMenuItem
                        onSelect={() => handleSelect(turn)}
                        disabled={!turn.hasSha}
                        className="justify-start flex min-w-0 flex-1 items-center gap-2 text-left"
                      >
                        {/* Turn indicator dot */}
                        <span
                    className={clsx(
                      "h-2 w-2 shrink-0 rounded-full",
                      isActive
                        ? "bg-primary"
                        : turn.isLatest
                          ? "bg-success"
                          : "bg-muted-foreground/50",
                    )}
                  />

                        {/* Turn info */}
                        <div className="flex flex-1 flex-col min-w-0">
                          <span className="text-xs font-medium">
                            Turn {turn.turnNumber}
                            {turn.isLatest && (
                              <span className="ml-1.5 text-[9px] text-success font-normal">
                                live
                              </span>
                            )}
                          </span>
                          <span className="text-[10px] font-mono text-muted-foreground truncate">
                            {turn.fromSha.slice(0, 7)}
                            {turn.toSha ? ` → ${turn.toSha.slice(0, 7)}` : " → HEAD"}
                          </span>
                        </div>
                      </DropdownMenuItem>

                      {/* Restore the saved pre-input file state. */}
                      {turn.hasSha && (
                        <DropdownMenuItem
                          disabled={restoring}
                          onSelect={(e) => {
                        e.stopPropagation();
                        void handleRestore(turn.ref);
                      }}
                          className="flex shrink-0 items-center gap-0.5 rounded border border-destructive/30 bg-destructive/5 px-1.5 py-0.5 text-[9px] text-destructive opacity-0 group-hover:opacity-100 hover:bg-destructive/10 disabled:opacity-50 transition"
                          title={`Restore files before turn ${turn.turnNumber}`}
                          aria-label={`Restore files before turn ${turn.turnNumber}`}
                        >
                          <RotateCcw size={8} /> Restore files
                        </DropdownMenuItem>
                      )}
                    </div>
                  );
                })}
              </div>

              {error && (
                <div className="px-3 py-2 text-xs text-destructive">
                  <p role="alert">{error}</p>
                  <DropdownMenuItem onSelect={reload}>
                    Reload saved file states
                  </DropdownMenuItem>
                </div>
              )}
              {backups.length > 0 && (
                <div className="max-h-48 overflow-y-auto border-t border-border p-2">
                  <p className="mb-1 text-xs text-muted-foreground">Backups</p>
                  {backups.map((backup, index) => (
                    <DropdownMenuItem
                      key={backup.ref}
                      disabled={restoring}
                      onSelect={() => void handleRestore(backup.ref)}
                      className="justify-start block w-full rounded px-2 py-1 text-left text-xs hover:bg-accent disabled:opacity-50"
                    >
                      Restore files saved{" "}
                      {new Date(backup.createdAt).toLocaleString()} · backup{" "}
                      {backups.length - index}
                    </DropdownMenuItem>
                  ))}
                </div>
              )}
              {/* Footer */}
              <div className="border-t border-border px-3 py-1.5">
                <span className="text-[10px] text-muted-foreground">
                  {turns.length} checkpoint{turns.length !== 1 ? "s" : ""} ·
                  restore files, keep conversation
                </span>
              </div>
            </DropdownMenuGroup>
          </DropdownMenuContent>
        )}
      </DropdownMenu>
    </div>
  );
}
