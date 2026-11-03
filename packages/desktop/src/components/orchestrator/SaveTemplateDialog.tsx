import clsx from "clsx";
import { useCallback, useEffect, useId, useRef, useState } from "react";

// ── Types ───────────────────────────────────────────────────────────

export interface SaveTemplateDialogProps {
  onSave: (name: string, description: string) => void;
  onCancel: () => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Dialog for saving the current workspace configuration as a reusable template.
 *
 * Follows the same modal pattern as RevertCheckpointDialog:
 *   - Backdrop click dismisses.
 *   - Esc cancels.
 *   - Focus trapped: initial focus on the name input.
 *   - Focus restored on unmount.
 *   - Proper ARIA labelling.
 */
export function SaveTemplateDialog({ onSave, onCancel }: SaveTemplateDialogProps) {
  const headingId = useId();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const nameRef = useRef<HTMLInputElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  // Focus management: grab focus on mount, restore on unmount.
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    nameRef.current?.focus();
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

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      const trimmedName = name.trim();
      if (!trimmedName) return;
      onSave(trimmedName, description.trim());
    },
    [name, description, onSave],
  );

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
        className="w-[26rem] max-w-[calc(100vw-2rem)] rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="border-b border-gx-border p-4">
          <h2 id={headingId} className="text-[14px] font-semibold text-gx-text">
            Save as Template
          </h2>
          <p className="mt-1 text-[12px] text-gx-muted">
            Save the current workspace configuration for reuse.
          </p>
        </div>

        {/* Form */}
        <form onSubmit={handleSubmit} className="space-y-4 p-4">
          <div>
            <label
              htmlFor="template-name"
              className="mb-1 block text-[11px] font-medium text-gx-muted"
            >
              Name
            </label>
            <input
              ref={nameRef}
              id="template-name"
              type="text"
              className="w-full rounded-md border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-xs text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
              placeholder="e.g., Bug Fix (fast)"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>

          <div>
            <label
              htmlFor="template-description"
              className="mb-1 block text-[11px] font-medium text-gx-muted"
            >
              Description
            </label>
            <textarea
              id="template-description"
              className="w-full resize-none rounded-md border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-xs text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
              placeholder="Short description of when to use this template…"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={3}
            />
          </div>

          {/* Actions */}
          <div className="flex items-center justify-end gap-2 pt-1">
            <button
              type="button"
              onClick={onCancel}
              className={clsx(
                "rounded border border-gx-border bg-gx-surface px-3 py-1.5 text-[12px] text-gx-text transition",
                "hover:bg-gx-surface-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              )}
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={!name.trim()}
              className={clsx(
                "rounded bg-gx-accent px-3 py-1.5 text-[12px] font-medium text-white transition",
                "hover:bg-gx-accent/80 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
                "disabled:cursor-not-allowed disabled:opacity-50",
              )}
            >
              Save Template
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
