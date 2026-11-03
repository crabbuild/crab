import { RotateCcw } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  turnNumber: number;
  timestamp: number;
  onRevert?: (turnNumber: number) => void;
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatTime(ms: number): string {
  const date = new Date(ms);
  return date.toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

// ── Component ───────────────────────────────────────────────────────

export function CheckpointMarker({ turnNumber, timestamp, onRevert }: Props) {
  return (
    <div
      className="group relative flex items-center gap-2 py-1"
      role="separator"
      aria-label={`Turn ${turnNumber} checkpoint at ${formatTime(timestamp)}`}
    >
      {/* Left line */}
      <div className="h-px flex-1 bg-gx-border/60" />

      {/* Label */}
      <span className="shrink-0 select-none text-[10px] text-gx-muted/70">
        Turn {turnNumber} · {formatTime(timestamp)}
      </span>

      {/* Revert button — visible on hover */}
      {onRevert && (
        <button
          type="button"
          onClick={() => onRevert(turnNumber)}
          className="shrink-0 flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted/70 opacity-0 transition hover:bg-gx-surface-hover hover:text-gx-text group-hover:opacity-100 focus-visible:opacity-100 focus-visible:outline focus-visible:outline-1 focus-visible:outline-gx-accent"
          aria-label={`Revert to turn ${turnNumber}`}
        >
          <RotateCcw size={10} />
          Revert
        </button>
      )}

      {/* Right line */}
      <div className="h-px flex-1 bg-gx-border/60" />
    </div>
  );
}
