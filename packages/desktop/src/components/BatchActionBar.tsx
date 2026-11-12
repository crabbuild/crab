import { Button } from "@/components/ui/button";
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
      className="flex items-center gap-2 border-b border-primary/30 bg-primary/10 px-3 py-1.5"
      role="toolbar"
      aria-label="Batch file actions"
    >
      <span className="text-[11px] font-medium text-foreground">
        {count} files selected
      </span>

      <div className="ml-auto flex items-center gap-1">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onStage}
          disabled={busy}
          className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-success hover:bg-success/10 disabled:opacity-50 transition"
          title="Stage selected files (S)"
        >
          <Plus size={12} />
          Stage
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onUnstage}
          disabled={busy}
          className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-warning hover:bg-warning/10 disabled:opacity-50 transition"
          title="Unstage selected files (U)"
        >
          <Minus size={12} />
          Unstage
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onDiscard}
          disabled={busy || !onDiscard}
          className="flex items-center gap-1 rounded px-2 py-0.5 text-[11px] text-destructive hover:bg-destructive/10 disabled:opacity-50 transition"
          title="Discard selected files (Delete)"
          aria-label="Discard selected files"
        >
          <Trash2 size={12} />
          Discard
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onClear}
          className="ml-1 flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground transition"
          title="Clear selection (Escape)"
          aria-label="Clear selection"
        >
          <X size={12} />
        </Button>
      </div>
    </div>
  );
}
