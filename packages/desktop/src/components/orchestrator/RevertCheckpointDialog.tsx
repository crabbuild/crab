import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import clsx from "clsx";
import { AlertTriangle } from "lucide-react";
import { useId, useRef } from "react";

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
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onCancel();
        }
      }}
    >
      <DialogContent
        role="alertdialog"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
    cancelRef.current?.focus();
        }}
        showCloseButton={false}
        aria-describedby={bodyId}
        className="block gap-0 p-0 w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-destructive/15 p-2 text-destructive">
            <AlertTriangle size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <DialogTitle className="text-[14px] font-semibold text-foreground"
            >
              Revert to Turn {turnNumber}?
            </DialogTitle>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              This will revert the workspace to Turn {turnNumber}. All changes
              and chat history after this turn will be permanently lost.
            </p>
          </div>
        </div>

        {/* Action buttons */}
        <div className="flex items-center justify-end gap-2 p-3">
          <Button
            variant="ghost"
            size="compact"
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className={clsx(
              "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            Cancel
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={onConfirm}
            className={clsx(
              "rounded border border-destructive/60 bg-destructive/15 px-3 py-1.5 text-[12px] font-medium text-destructive transition",
              "hover:bg-destructive/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
            )}
          >
            Revert
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
