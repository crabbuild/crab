import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
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

import clsx from "clsx";
import { RotateCcw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useWorkspaceSnapshots } from "../../lib/use-workspace-snapshots";
import { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

export interface CheckpointDiffRange {
  fromSha: string;
  toSha: string | null;
  turnNumber: number;
}

interface Props {
  task: OrchestratorTask;
  onRestore?: () => void;
  onSelectTurn?: (range: CheckpointDiffRange | null) => void;
  selectedTurn?: number | null;
}

// ── Component ───────────────────────────────────────────────────────

export function CheckpointTimeline({ task, onRestore, onSelectTurn, selectedTurn }: Props) {
  const { checkpoints, backups, loading, restoring, error, restore, reload } = useWorkspaceSnapshots(task);
  const [hoveredTurn, setHoveredTurn] = useState<number | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

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

  if (loading || (!error && turns.length === 0 && backups.length === 0)) return null;

  return (
    <div className="flex shrink-0 items-center gap-3 px-4 py-2">
      {/* "All" reset link on the left */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => onSelectTurn?.(null)}
        className={clsx(
          "shrink-0 text-[10px] font-medium transition",
          selectedTurn == null
            ? "text-primary"
            : "text-muted-foreground hover:text-foreground",
        )}
      >
        All changes
      </Button>

      {/* Separator */}
      <div className="h-3 w-px bg-border/50" />

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
            >
              {/* Node button — transparent wrapper, only the inner dot is visible */}
              <Popover
                open={isHovered}
                onOpenChange={(open) =>
                  setHoveredTurn(open ? turn.turnNumber : null)
                }
              >
                <PopoverTrigger asChild>
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    onClick={() => handleSelect(turn)}
                    disabled={!turn.hasSha}
                    className={clsx(
                    "group relative flex h-4 w-4 items-center justify-center bg-transparent border-0 p-0 focus:outline-none focus-visible:ring-2 focus-visible:ring-primary/40 rounded-full",
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
                        ? "h-3 w-3 bg-primary"
                        : turn.isLatest
                          ? "h-2 w-2 bg-success"
                          : "h-2 w-2 bg-muted-foreground/50 group-hover:bg-muted-foreground",
                    )}
                  />
                  </Button>
                </PopoverTrigger>

                {/* Label below */}
                <span
                  className={clsx(
                    "absolute top-5 text-[9px] font-mono transition whitespace-nowrap",
                    isActive
                      ? "text-primary font-semibold"
                      : isHovered
                        ? "text-foreground"
                        : "text-muted-foreground/50",
                  )}
                >
                  {turn.turnNumber}
                </span>

                {/* Hover tooltip with revert */}
                {isHovered && turn.hasSha && (
                  <PopoverContent
                    side="top"
                    className="flex w-auto items-center gap-2 px-2.5 py-1.5 whitespace-nowrap"
                  >
                    <div className="flex flex-col items-start">
                      <span className="text-[10px] font-medium text-foreground">
                        Turn {turn.turnNumber}
                      </span>
                      <span className="text-[9px] text-muted-foreground font-mono">
                        {turn.fromSha.slice(0, 7)}
                      </span>
                    </div>
                    {turn.hasSha && (
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        disabled={restoring}
                        onClick={(e) => { e.stopPropagation(); void handleRestore(turn.ref); }}
                        className="flex items-center gap-1 rounded border border-destructive/30 bg-destructive/5 px-1.5 py-0.5 text-[9px] text-destructive hover:bg-destructive/10 disabled:opacity-50 transition"
                      >
                        <RotateCcw size={8} /> Restore files
                      </Button>
                    )}

                    {/* Tooltip arrow */}
                  </PopoverContent>
                )}
              </Popover>
            </div>
          );
        })}
      </div>

      {error && (
        <div className="text-xs text-destructive">
          <p role="alert">{error}</p>
          <Button variant="ghost" size="compact" type="button" onClick={reload}>
            Reload saved file states
          </Button>
        </div>
      )}
      {backups.length > 0 && (
        <Select
          disabled={restoring}
          value={":" + String("")}
          onValueChange={(selectedValue) => {
            const value = selectedValue.slice(1);
            return void handleRestore(value);
          }}
        >
          <SelectTrigger aria-label="Restore saved file backup">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              <SelectItem value={":" + String("")}>Backups</SelectItem>
              {backups.map((backup) => (
                <SelectItem key={backup.ref} value={":" + String(backup.ref)}>
                  {new Date(backup.createdAt).toLocaleString()}
                </SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>
      )}
      {/* Right: turn counter */}
      <div className="ml-auto shrink-0 text-[10px] tabular-nums text-muted-foreground/60">
        {selectedTurn != null ? `Turn ${selectedTurn}` : `${turns.length} turn${turns.length !== 1 ? "s" : ""}`}
      </div>
    </div>
  );
}
