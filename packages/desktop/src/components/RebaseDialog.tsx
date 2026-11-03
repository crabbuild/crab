import clsx from "clsx";
import { GitBranch, Loader2 } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { rpc } from "../ipc";

/**
 * Modal dialog for initiating a rebase operation.
 *
 * Pre-fills the branch to rebase (from context menu selection) and
 * the onto target (current HEAD branch). On start it kicks off a
 * non-interactive rebase via `rpc.opStart`. The interactive todo
 * editor (`<RebaseTodo />`) is a separate component used after the
 * rebase pauses or when the user wants to edit the todo mid-rebase.
 *
 * On success (state "done"): close + fire `onSuccess`.
 * On paused (conflicts): close + fire `onConflicts`.
 * On error: show inline.
 */
export interface RebaseDialogProps {
  /** Branch being rebased. */
  branch: string;
  /** Current HEAD branch (onto target). */
  ontoBranch: string;
  /** Active repo id for the RPC call. */
  repoId: string;
  /** Called after a successful rebase (state "done"). */
  onSuccess: () => void;
  /** Called when the rebase pauses due to conflicts. */
  onConflicts: () => void;
  /** Called to close the dialog without action. */
  onCancel: () => void;
}

export function RebaseDialog({
  branch,
  ontoBranch,
  repoId,
  onSuccess,
  onConflicts,
  onCancel,
}: RebaseDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  const [rebasing, setRebasing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Focus management: grab focus on mount, restore on unmount.
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

  async function handleRebase() {
    setRebasing(true);
    setError(null);
    try {
      const result = await rpc.opStart(repoId, {
        kind: "Rebase",
        onto: ontoBranch,
        branch,
      });

      if (result.state === "done") {
        onSuccess();
      } else if (result.state === "paused") {
        onConflicts();
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setRebasing(false);
    }
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={headingId}
        aria-describedby={bodyId}
        className="w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-gx-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-gx-accent/15 p-2 text-gx-accent">
            <GitBranch size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-gx-text"
            >
              Rebase branch
            </h2>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-gx-muted">
              Rebase{" "}
              <code className="rounded bg-gx-bg-sub px-1 font-mono text-gx-text">
                {branch}
              </code>{" "}
              onto{" "}
              <code className="rounded bg-gx-bg-sub px-1 font-mono text-gx-text">
                {ontoBranch}
              </code>
            </p>
          </div>
        </div>

        {/* Body */}
        <div className="space-y-4 border-b border-gx-border px-4 py-4">
          {/* Branch / Onto (read-only) */}
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="block text-[11px] font-medium text-gx-muted">
                Branch
              </label>
              <div className="mt-1 truncate rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[12px] text-gx-text">
                {branch}
              </div>
            </div>
            <div>
              <label className="block text-[11px] font-medium text-gx-muted">
                Onto
              </label>
              <div className="mt-1 truncate rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[12px] text-gx-text">
                {ontoBranch}
              </div>
            </div>
          </div>

          {/* Error */}
          {error && (
            <div className="rounded border border-gx-error/40 bg-gx-error/10 px-3 py-2 text-[11px] text-gx-error">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 p-3">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            disabled={rebasing}
            className={clsx(
              "rounded border border-gx-border bg-gx-surface px-3 py-1.5 text-[12px] text-gx-text transition",
              "hover:bg-gx-surface-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              "disabled:opacity-50",
            )}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={handleRebase}
            disabled={rebasing}
            className={clsx(
              "flex items-center gap-1.5 rounded border border-gx-accent/60 bg-gx-accent/15 px-3 py-1.5 text-[12px] font-medium text-gx-accent transition",
              "hover:bg-gx-accent/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              "disabled:opacity-50",
            )}
          >
            {rebasing && <Loader2 size={12} className="animate-spin" />}
            Start Rebase
          </button>
        </div>
      </div>
    </div>
  );
}
