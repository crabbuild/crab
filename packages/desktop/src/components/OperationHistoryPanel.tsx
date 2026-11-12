import { Button } from "@/components/ui/button";
import clsx from "clsx";
import {
  AlertTriangle,
  CheckCircle2,
  Clock,
  Trash2,
  Undo2,
  XCircle,
} from "lucide-react";
import { useEffect, useState } from "react";
import {
  clearHistory,
  useOperationHistory,
  type OperationRecord,
} from "../lib/operation-history";
import { executeUndo, UNDO_WINDOW_MS, useUndoStack } from "../lib/undo-stack";

// --- Relative time formatter ---

function formatRelativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  if (diff < 1000) return "just now";
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

// --- Status icon helper ---

function StatusIcon({ status }: { status: OperationRecord["status"] }) {
  switch (status) {
    case "success":
      return <CheckCircle2 size={14} className="shrink-0 text-success" />;
    case "error":
      return <XCircle size={14} className="shrink-0 text-destructive" />;
    case "conflict":
      return <AlertTriangle size={14} className="shrink-0 text-warning" />;
  }
}

// --- Main component ---

/**
 * Self-contained operation history panel. Renders a timeline of recent
 * git operations with status badges, relative timestamps, durations,
 * and optional undo buttons. Can be mounted in any view that wants to
 * show operation history (e.g., the Activity sidebar).
 */
export function OperationHistoryPanel() {
  const history = useOperationHistory();
  const undoStack = useUndoStack();
  const [now, setNow] = useState(Date.now());

  // Tick every second so relative timestamps stay fresh and undo
  // window expiry is reflected in real time. Pauses when hidden.
  useEffect(() => {
    let interval: ReturnType<typeof setInterval> | null = null;

    const start = () => {
      if (interval) return;
      interval = setInterval(() => setNow(Date.now()), 1000);
    };
    const stop = () => {
      if (interval) { clearInterval(interval); interval = null; }
    };
    const onVisibility = () => { document.hidden ? stop() : start(); };

    if (!document.hidden) start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);

  const undoableIds = new Set(
    undoStack
      .filter((entry) => !entry.undone && now - entry.timestamp < UNDO_WINDOW_MS)
      .map((entry) => entry.id),
  );

  // Show most recent operations first.
  const sorted = [...history].reverse();

  return (
    <div className="flex h-full min-h-0 flex-col bg-background">
      {/* Header */}
      <div className="flex h-8 shrink-0 items-center justify-between border-b border-border bg-card px-2">
        <div className="flex items-center gap-1.5 text-xs font-medium text-foreground">
          <Clock size={14} className="text-muted-foreground" />
          <span>Operation History</span>
        </div>
        {history.length > 0 && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => clearHistory()}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground transition-colors hover:bg-primary/20 hover:text-foreground"
            title="Clear history"
            aria-label="Clear operation history"
          >
            <Trash2 size={12} />
            Clear
          </Button>
        )}
      </div>

      {/* Content */}
      <div className="min-h-0 flex-1 overflow-y-auto">
        {sorted.length === 0 ? (
          <div className="flex h-full items-center justify-center p-4 text-center text-xs text-muted-foreground">
            No operations yet
          </div>
        ) : (
          <ul className="divide-y divide-border/50" role="list" aria-label="Operation history">
            {sorted.map((op) => (
              <OperationRow
                key={op.id}
                operation={op}
                canUndo={op.undoId != null && undoableIds.has(op.undoId)}
                now={now}
              />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

// --- Row component ---

function OperationRow({
  operation,
  canUndo,
  now,
}: {
  operation: OperationRecord;
  canUndo: boolean;
  now: number;
}) {
  const [undoing, setUndoing] = useState(false);

  async function handleUndo() {
    if (!operation.undoId) return;
    setUndoing(true);
    try {
      await executeUndo(operation.undoId);
    } finally {
      setUndoing(false);
    }
  }

  // Suppress the lint about `now` being unused — it's used to force
  // re-render for fresh relative timestamps.
  void now;

  return (
    <li className="flex items-center gap-2 px-2 py-1.5">
      <StatusIcon status={operation.status} />

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5">
          <span className="truncate text-xs text-foreground">
            {operation.label}
          </span>
          <span
            className={clsx(
              "shrink-0 rounded px-1 py-px text-[10px] font-medium",
              operation.status === "success" && "bg-success/10 text-success",
              operation.status === "error" && "bg-destructive/10 text-destructive",
              operation.status === "conflict" && "bg-warning/10 text-warning",
            )}
          >
            {operation.status}
          </span>
        </div>
        <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
          <span>{formatRelativeTime(operation.timestamp)}</span>
          <span>·</span>
          <span>{operation.durationMs}ms</span>
        </div>
      </div>

      {canUndo && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={handleUndo}
          disabled={undoing}
          className={clsx(
            "flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-primary transition-colors",
            "hover:bg-primary/20",
            undoing && "cursor-not-allowed opacity-50",
          )}
          title="Undo this operation"
          aria-label={`Undo ${operation.label}`}
        >
          <Undo2 size={12} />
          Undo
        </Button>
      )}
    </li>
  );
}
