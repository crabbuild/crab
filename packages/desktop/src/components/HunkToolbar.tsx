import { Button } from "@/components/ui/button";
import clsx from "clsx";
import { Minus, Plus, Trash2 } from "lucide-react";

/**
 * Per-hunk floating toolbar. Rendered by `<InlinePatchDiff />` at
 * the top of every hunk. Three actions:
 *
 *   - **Stage** — add the hunk to the index. Always available; the
 *     caller drives this off a worktree-vs-index diff so every
 *     hunk shown here is by definition not-yet-staged.
 *   - **Unstage** — reverse-apply `--cached`. Disabled unless the
 *     parent has a staged-side diff for this file (wiring lives in
 *     the view; the toolbar just displays state).
 *   - **Discard** — reverse-apply to the worktree. The consumer
 *     (`<ChangesView />`) routes the intent through a threshold-
 *     aware modal: small discards go through in one click, large
 *     discards pop a `<ConfirmDiscardDialog />`. The toolbar
 *     itself no longer does a local inline confirm — a two-tier
 *     "chip then modal" UX was confusing and slowed small edits.
 *
 * The toolbar is dumb: it emits intents and lets the view deal
 * with threshold gating, agent calls, error recovery, and refresh.
 */
export interface HunkToolbarProps {
  /** 1-based display hint ("Hunk 2 of 5"). Purely cosmetic. */
  hunkNumber?: number;
  hunkCount?: number;
  /** Header text to echo (the `@@ -x,y +a,b @@` string). Optional
   *  — when undefined the toolbar renders a generic title. */
  header?: string;
  /** Show / enable the Unstage button. */
  canUnstage?: boolean;
  onStage: () => void;
  onUnstage?: () => void;
  onDiscard: () => void;
  /** Compact mode: icon-only buttons. Used in the line-level
   *  variant. Defaults to the full-text hunk-level toolbar. */
  compact?: boolean;
  className?: string;
}

export function HunkToolbar({
  hunkNumber,
  hunkCount,
  header,
  canUnstage = false,
  onStage,
  onUnstage,
  onDiscard,
  compact = false,
  className,
}: HunkToolbarProps) {
  const title =
    hunkNumber != null && hunkCount != null
      ? `Hunk ${hunkNumber} of ${hunkCount}`
      : "Hunk";

  return (
    <div
      className={clsx(
        "flex items-center gap-2 border-b border-border bg-card px-2 py-1 text-[11px]",
        className,
      )}
      role="toolbar"
      aria-label={title}
    >
      {!compact && (
        <div className="flex min-w-0 items-center gap-2 text-muted-foreground">
          <span className="font-medium text-foreground">{title}</span>
          {header && (
            <span
              className="truncate font-mono text-[10px] text-muted-foreground"
              title={header}
            >
              {header}
            </span>
          )}
        </div>
      )}
      <div className={clsx("flex items-center gap-1", !compact && "ml-auto")}>
        <ToolbarButton
          icon={<Plus size={11} />}
          label="Stage hunk"
          onClick={onStage}
          tone="success"
          compact={compact}
        />
        <ToolbarButton
          icon={<Minus size={11} />}
          label="Unstage hunk"
          onClick={() => onUnstage?.()}
          disabled={!canUnstage || !onUnstage}
          tone="warn"
          compact={compact}
        />
        <ToolbarButton
          icon={<Trash2 size={11} />}
          label="Discard hunk"
          onClick={onDiscard}
          tone="error"
          compact={compact}
        />
      </div>
    </div>
  );
}

function ToolbarButton({
  icon,
  label,
  onClick,
  disabled,
  tone,
  compact,
}: {
  icon: React.ReactNode;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  tone: "success" | "warn" | "error";
  compact?: boolean;
}) {
  const toneCls =
    tone === "success"
      ? "hover:bg-success/15 hover:text-success"
      : tone === "warn"
        ? "hover:bg-warning/15 hover:text-warning"
        : "hover:bg-destructive/15 hover:text-destructive";
  return (
    <Button
      variant="ghost"
      size="compact"
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      title={label}
      className={clsx(
        "flex items-center gap-1 rounded border border-transparent px-1.5 py-0.5 text-[10px] text-muted-foreground transition",
        toneCls,
        disabled && "cursor-not-allowed opacity-40 hover:bg-transparent hover:text-muted-foreground",
      )}
    >
      {icon}
      {!compact && <span>{label.replace(" hunk", "")}</span>}
    </Button>
  );
}
