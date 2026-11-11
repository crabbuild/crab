import clsx from "clsx";
import { AlertTriangle } from "lucide-react";
import { useEffect, useId, useRef } from "react";

// ── Types ───────────────────────────────────────────────────────────

export interface RevertCheckpointDialogProps {
  /** The turn number the user wants to revert to. */
  turnNumber: number;
  /** Called when the user confirms the revert. */
  onConfirm: () => void;
  /** Called when the user cancels. */
  onCancel: () => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Confirmation dialog shown before reverting to a checkpoint.
 *
 * Warns the user that all changes and chat history after the
 * selected turn will be permanently lost.
 *
 * Accessibility:
 *   - `role="alertdialog"` for destructive action announcement.
 *   - `aria-labelledby` / `aria-describedby` for accessible name/desc.
 *   - Initial focus on Cancel (non-destructive default).
 *   - Esc closes via `onCancel`.
 */
export function RevertCheckpointDialog({
  turnNumber,
  onConfirm,
  onCancel,
}: RevertCheckpointDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  // Grab focus on mount, restore on unmount.
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    cancelRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, []);

  // Esc cancels.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onCancel();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [onCancel]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={headingId}
        aria-describedby={bodyId}
        className="w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-destructive/15 p-2 text-destructive">
            <AlertTriangle size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-foreground"
            >
              Revert to Turn {turnNumber}?
            </h2>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              This will revert the workspace to Turn {turnNumber}. All changes
              and chat history after this turn will be permanently lost.
            </p>
          </div>
        </div>

        {/* Action buttons */}
        <div className="flex items-center justify-end gap-2 p-3">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className={clsx(
              "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onConfirm}
            className={clsx(
              "rounded border border-destructive/60 bg-destructive/15 px-3 py-1.5 text-[12px] font-medium text-destructive transition",
              "hover:bg-destructive/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
            )}
          >
            Revert
          </button>
        </div>
      </div>
    </div>
  );
}
