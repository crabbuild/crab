import clsx from "clsx";
import { AlertTriangle, Trash2 } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";

/**
 * Confirmation dialog for removing a worktree.
 *
 * Two modes:
 * - Clean worktree: simple "Remove worktree?" confirmation with Cancel/Remove.
 * - Dirty worktree: lists the dirty file count and requires the user to type
 *   "REMOVE" to enable the Force Remove button, preventing accidental data loss.
 *
 * Follows the same modal pattern as `<ConfirmForceDeleteDialog />`.
 */
export interface WorktreeRemoveConfirmDialogProps {
  /** Display name or directory name of the worktree. */
  worktreeName: string;
  /** Full path to the worktree. */
  worktreePath: string;
  /** Number of uncommitted/dirty files. 0 means the worktree is clean. */
  dirtyCount: number;
  /** Called when the user confirms removal. `force` is true when dirty. */
  onConfirm: (force: boolean) => void;
  onCancel: () => void;
}

export function WorktreeRemoveConfirmDialog({
  worktreeName,
  worktreePath,
  dirtyCount,
  onConfirm,
  onCancel,
}: WorktreeRemoveConfirmDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const inputRef = useRef<HTMLInputElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);
  const [typedValue, setTypedValue] = useState("");

  const isDirty = dirtyCount > 0;
  const confirmWord = "REMOVE";
  const matches = typedValue === confirmWord;

  // Grab focus on mount, restore on unmount.
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    if (isDirty) {
      inputRef.current?.focus();
    } else {
      cancelRef.current?.focus();
    }
    return () => {
      previousActive.current?.focus?.();
    };
  }, [isDirty]);

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
        className="w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-gx-border p-4">
          <div
            className={clsx(
              "mt-0.5 shrink-0 rounded p-2",
              isDirty
                ? "bg-gx-error/15 text-gx-error"
                : "bg-gx-warn/15 text-gx-warn",
            )}
          >
            {isDirty ? (
              <AlertTriangle size={18} aria-hidden="true" />
            ) : (
              <Trash2 size={18} aria-hidden="true" />
            )}
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-gx-text"
            >
              {isDirty ? "Force remove worktree with uncommitted changes?" : "Remove worktree?"}
            </h2>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-gx-muted">
              {isDirty ? (
                <>
                  Worktree{" "}
                  <code className="rounded bg-gx-bg-sub px-1 font-mono text-gx-text">
                    {worktreeName}
                  </code>{" "}
                  has{" "}
                  <span className="font-semibold text-gx-warn">
                    {dirtyCount} uncommitted change{dirtyCount !== 1 ? "s" : ""}
                  </span>
                  . Force removing will permanently discard these changes.
                </>
              ) : (
                <>
                  Remove worktree{" "}
                  <code className="rounded bg-gx-bg-sub px-1 font-mono text-gx-text">
                    {worktreeName}
                  </code>
                  ? This cannot be undone.
                </>
              )}
            </p>
          </div>
        </div>

        {/* Path display */}
        <div className="border-b border-gx-border px-4 py-2">
          <span className="text-[11px] text-gx-muted">Path: </span>
          <code className="font-mono text-[11px] text-gx-text">{worktreePath}</code>
        </div>

        {/* Typed confirmation for dirty worktrees */}
        {isDirty && (
          <div className="border-b border-gx-border px-4 py-3">
            <label className="block text-[12px] text-gx-muted">
              Type <code className="rounded bg-gx-bg-sub px-1 font-mono font-semibold text-gx-error">REMOVE</code> to confirm force removal:
            </label>
            <input
              ref={inputRef}
              type="text"
              className={clsx(
                "mt-1.5 w-full rounded border bg-gx-bg-sub px-2 py-1.5 font-mono text-[12px] text-gx-text focus-visible:outline-none transition",
                matches
                  ? "border-gx-accent focus:ring-1 focus:ring-gx-accent"
                  : "border-gx-border focus:ring-1 focus:ring-gx-border",
              )}
              value={typedValue}
              onChange={(e) => setTypedValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && matches) onConfirm(true);
              }}
              placeholder={confirmWord}
              autoComplete="off"
              spellCheck={false}
            />
          </div>
        )}

        {/* Action buttons */}
        <div className="flex items-center justify-end gap-2 p-3">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className={clsx(
              "rounded border border-gx-border bg-gx-surface px-3 py-1.5 text-[12px] text-gx-text transition",
              "hover:bg-gx-surface-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
            )}
          >
            Cancel
          </button>
          {isDirty ? (
            <button
              type="button"
              onClick={() => onConfirm(true)}
              disabled={!matches}
              className={clsx(
                "rounded border px-3 py-1.5 text-[12px] font-medium transition",
                "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-error",
                matches
                  ? "border-gx-error/60 bg-gx-error/15 text-gx-error hover:bg-gx-error/25"
                  : "cursor-not-allowed border-gx-border bg-gx-surface text-gx-muted opacity-50",
              )}
              aria-label="Force Remove"
            >
              Force Remove
            </button>
          ) : (
            <button
              type="button"
              onClick={() => onConfirm(false)}
              className={clsx(
                "rounded border border-gx-error/60 bg-gx-error/15 px-3 py-1.5 text-[12px] font-medium text-gx-error transition",
                "hover:bg-gx-error/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-error",
              )}
              aria-label="Remove"
            >
              Remove
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
