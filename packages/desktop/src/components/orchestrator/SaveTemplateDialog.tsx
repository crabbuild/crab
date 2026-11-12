import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import clsx from "clsx";
import { useCallback, useRef, useState } from "react";

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
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const nameRef = useRef<HTMLInputElement | null>(null);
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
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onCancel();
        }
      }}
    >
      <DialogContent
        onOpenAutoFocus={(event) => {
          event.preventDefault();
    nameRef.current?.focus();
        }}
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 w-[26rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="border-b border-border p-4">
          <DialogTitle className="text-[14px] font-semibold text-foreground">
            Save as Template
          </DialogTitle>
          <p className="mt-1 text-[12px] text-muted-foreground">
            Save the current workspace configuration for reuse.
          </p>
        </div>

        {/* Form */}
        <form onSubmit={handleSubmit} className="space-y-4 p-4">
          <div>
            <label
              htmlFor="template-name"
              className="mb-1 block text-[11px] font-medium text-muted-foreground"
            >
              Name
            </label>
            <Input
              ref={nameRef}
              id="template-name"
              type="text"
              className="w-full rounded-md border border-border bg-background px-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
              placeholder="e.g., Bug Fix (fast)"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>

          <div>
            <label
              htmlFor="template-description"
              className="mb-1 block text-[11px] font-medium text-muted-foreground"
            >
              Description
            </label>
            <Textarea
              id="template-description"
              className="w-full resize-none rounded-md border border-border bg-background px-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
              placeholder="Short description of when to use this template…"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={3}
            />
          </div>

          {/* Actions */}
          <div className="flex items-center justify-end gap-2 pt-1">
            <Button
              variant="ghost"
              size="compact"
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
              variant="default"
              size="compact"
              type="submit"
              disabled={!name.trim()}
              className={clsx(
                "rounded bg-primary px-3 py-1.5 text-[12px] font-medium text-primary-foreground transition",
                "hover:bg-primary/80 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
                "disabled:cursor-not-allowed disabled:opacity-50",
              )}
            >
              Save Template
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
