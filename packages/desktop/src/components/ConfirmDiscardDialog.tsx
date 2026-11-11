import clsx from "clsx";
import { AlertTriangle } from "lucide-react";
import { useEffect, useId, useRef } from "react";

/**
 * Blocking confirm dialog shown before a large discard operation.
 *
 * The `<ChangesView />` wires this component up when a discard
 * request exceeds the user's configured threshold (Settings →
 * Diff → "Confirm before large discard"). Below the threshold,
 * discard happens in one click — that's the whole point of the
 * two-tier UX: fast for small reverts, deliberate for big ones.
 *
 * Accessibility notes:
 *
 *   - `role="alertdialog"` so assistive tech announces the
 *     destructive nature of the action, not just a plain dialog.
 *   - `aria-labelledby` / `aria-describedby` hook the visible
 *     heading and body into the modal's accessible name / desc.
 *   - Initial focus lands on Cancel — the non-destructive default
 *     — so pressing Enter by reflex cancels rather than discards.
 *   - Esc closes via `onCancel`, matching OS-level modal convention.
 *
 * The focus trap is intentionally minimal: we grab focus on mount,
 * restore it to whatever had focus before on unmount, and let Tab
 * cycle through the two buttons. For a two-button modal that's
 * sufficient without pulling in a full focus-trap dependency.
 */
export interface ConfirmDiscardDialogProps {
  /** Human-readable name(s) of the file(s) affected. For a single-
   *  hunk discard this is typically `["src/foo.ts"]`; for a
   *  pointer whole-file discard the same; for a future multi-file
   *  discard it would list them all. Passed to `<ul>` so long
   *  lists scroll instead of blowing the layout. */
  files: string[];
  /** Exact number of `+` / `-` lines the discard would touch. */
  lineCount: number;
  /** User-configured threshold that was crossed. Echoed in the
   *  body so the reader understands why they're being asked. */
  threshold: number;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmDiscardDialog({
  files,
  lineCount,
  threshold,
  onConfirm,
  onCancel,
}: ConfirmDiscardDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  // Grab focus on mount, restore on unmount. `document.activeElement`
  // may be null in some test environments — harmless; we just skip
  // the restore.
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    cancelRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, []);

  // Esc cancels. Scoped to the dialog's lifetime via the effect
  // cleanup so background views don't swallow Esc on unrelated
  // routes.
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

  const filesLabel = files.length === 1 ? "file" : "files";
  // Cap the inline file list so an accidental "discard everything"
  // doesn't explode the modal. Users can still scroll the `<ul>`.
  const MAX_FILES_SHOWN = 20;
  const shownFiles = files.slice(0, MAX_FILES_SHOWN);
  const hiddenCount = files.length - shownFiles.length;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      // Clicking the backdrop cancels — matches OS-modal convention.
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
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-destructive/15 p-2 text-destructive">
            <AlertTriangle size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-foreground"
            >
              Discard {lineCount.toLocaleString()} line
              {lineCount === 1 ? "" : "s"}?
            </h2>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              This change crosses your confirm threshold of {threshold.toLocaleString()}
              {" "}line{threshold === 1 ? "" : "s"}. Discarding rewrites the {filesLabel}
              {" "}on disk and cannot be undone from inside Crab Desktop.
            </p>
          </div>
        </div>
        <div className="max-h-40 overflow-auto border-b border-border bg-background px-4 py-2">
          <ul className="space-y-0.5">
            {shownFiles.map((f) => (
              <li
                key={f}
                className="truncate font-mono text-[11px] text-foreground"
                title={f}
              >
                {f}
              </li>
            ))}
            {hiddenCount > 0 && (
              <li className="text-[11px] text-muted-foreground">
                … and {hiddenCount.toLocaleString()} more
              </li>
            )}
          </ul>
        </div>
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
            Discard
          </button>
        </div>
      </div>
    </div>
  );
}
