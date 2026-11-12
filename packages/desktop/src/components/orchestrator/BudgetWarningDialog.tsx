import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import clsx from "clsx";
import { AlertTriangle } from "lucide-react";
import { useId, useRef } from "react";

// ── Types ───────────────────────────────────────────────────────────

export interface BudgetWarningDialogProps {
  /** Current accumulated cost in dollars. */
  cost: number;
  /** Budget limit in dollars. */
  limit: number;
  /** Optional workspace title for context. */
  workspaceTitle?: string;
  /** Called when the user chooses to continue the agent. */
  onContinue: () => void;
  /** Called when the user chooses to stop the agent. */
  onStop: () => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Warning dialog shown when a workspace exceeds its budget limit.
 *
 * The agent is paused until the user decides to continue or stop.
 *
 * Accessibility:
 *   - `role="alertdialog"` for urgent announcement.
 *   - `aria-labelledby` / `aria-describedby` for accessible name/desc.
 *   - Initial focus on Stop (safer default).
 *   - Esc closes via `onStop`.
 */
export function BudgetWarningDialog({
  cost,
  limit,
  workspaceTitle,
  onContinue,
  onStop,
}: BudgetWarningDialogProps) {
  const bodyId = useId();
  const stopRef = useRef<HTMLButtonElement | null>(null);

  // Esc stops the agent (safe default).

  const formattedCost = `$${cost.toFixed(2)}`;
  const formattedLimit = `$${limit.toFixed(2)}`;
  const title = workspaceTitle ? `'${workspaceTitle}'` : "This workspace";

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          if (true) onStop();
        }
      }}
    >
      <DialogContent
        role="alertdialog"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
    stopRef.current?.focus();
        }}
        showCloseButton={false}
        aria-describedby={bodyId}
        className="block gap-0 p-0 w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-warning/15 p-2 text-warning">
            <AlertTriangle size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <DialogTitle className="text-[14px] font-semibold text-foreground"
            >
              Budget Warning
            </DialogTitle>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              {title} has used {formattedCost} (limit: {formattedLimit}). The
              agent has been paused.
            </p>
          </div>
        </div>

        {/* Action buttons */}
        <div className="flex items-center justify-end gap-2 p-3">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={onContinue}
            className={clsx(
              "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            Continue
          </Button>
          <Button
            variant="ghost"
            size="compact"
            ref={stopRef}
            type="button"
            onClick={onStop}
            className={clsx(
              "rounded border border-destructive/60 bg-destructive/15 px-3 py-1.5 text-[12px] font-medium text-destructive transition",
              "hover:bg-destructive/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
            )}
          >
            Stop agent
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
