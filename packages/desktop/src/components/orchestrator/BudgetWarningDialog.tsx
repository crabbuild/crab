import clsx from "clsx";
import { AlertTriangle } from "lucide-react";
import { useEffect, useId, useRef } from "react";

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
  const headingId = useId();
  const bodyId = useId();
  const stopRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  // Grab focus on mount, restore on unmount.
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    stopRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, []);

  // Esc stops the agent (safe default).
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onStop();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [onStop]);

  const formattedCost = `$${cost.toFixed(2)}`;
  const formattedLimit = `$${limit.toFixed(2)}`;
  const title = workspaceTitle ? `'${workspaceTitle}'` : "This workspace";

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onStop();
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
          <div className="mt-0.5 shrink-0 rounded bg-warning/15 p-2 text-warning">
            <AlertTriangle size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-foreground"
            >
              Budget Warning
            </h2>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              {title} has used {formattedCost} (limit: {formattedLimit}).
              The agent has been paused.
            </p>
          </div>
        </div>

        {/* Action buttons */}
        <div className="flex items-center justify-end gap-2 p-3">
          <button
            type="button"
            onClick={onContinue}
            className={clsx(
              "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            Continue
          </button>
          <button
            ref={stopRef}
            type="button"
            onClick={onStop}
            className={clsx(
              "rounded border border-destructive/60 bg-destructive/15 px-3 py-1.5 text-[12px] font-medium text-destructive transition",
              "hover:bg-destructive/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
            )}
          >
            Stop agent
          </button>
        </div>
      </div>
    </div>
  );
}
