/**
 * Floating batch action bar — appears when 2+ files are multi-selected
 * (Cmd/Ctrl+click or Shift+click) in the Changes view. Provides
 * Stage / Unstage / Discard actions that apply to the entire selection.
 */
import { Minus, Plus, Trash2, X } from "lucide-react";

interface BatchActionBarProps {
  count: number;
  onStage: () => void;
  onUnstage: () => void;
  onDiscard?: () => void;
  onClear: () => void;
  /** True while a batch operation is in flight. */
  busy?: boolean;
}

export function BatchActionBar({
  count,
  onStage,
  onUnstage,
  onDiscard,
  onClear,
  busy,
}: BatchActionBarProps) {
  if (count < 2) return null;

  return (
    <div
      className="flex items-center gap-2 border-b border-gx-accent/30 bg-gx-accent/10 px-3 py-1.5"
      role="toolbar"
      aria-label="Batch file actions"
    >
      <span className="text-[11px] font-medium text-gx-text">
        {count} files selected
      </span>

      <div className="ml-auto flex items-center gap-1">
        <button
          type="button"
          onClick={onStage}
          disabled={busy}
          className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-gx-success hover:bg-gx-success/10 disabled:opacity-50 transition"
          title="Stage selected files (S)"
        >
          <Plus size={12} />
          Stage
        </button>
        <button
          type="button"
          onClick={onUnstage}
          disabled={busy}
          className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-gx-warn hover:bg-gx-warn/10 disabled:opacity-50 transition"
          title="Unstage selected files (U)"
        >
          <Minus size={12} />
          Unstage
        </button>
        <button
          type="button"
          onClick={onDiscard}
          disabled={busy || !onDiscard}
          className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-gx-error hover:bg-gx-error/10 disabled:opacity-50 transition"
          title="Discard selected files (Delete)"
        >
          <Trash2 size={12} />
          Discard
        </button>
        <button
          type="button"
          onClick={onClear}
          className="ml-1 flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
          title="Clear selection (Escape)"
          aria-label="Clear selection"
        >
          <X size={12} />
        </button>
      </div>
    </div>
  );
}
