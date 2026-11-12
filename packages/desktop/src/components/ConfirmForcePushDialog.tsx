import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import clsx from "clsx";
import { AlertTriangle } from "lucide-react";
import { useId, useRef, useState } from "react";

/**
 * Typed confirmation dialog for force-pushing a branch.
 *
 * Shown when the user selects plain `--force` push. The user must
 * type the branch name exactly to enable the "Force push" button,
 * preventing accidental history rewrite. `--force-with-lease` does
 * NOT require this confirmation.
 */
export interface ConfirmForcePushDialogProps {
  branchName: string;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmForcePushDialog({
  branchName,
  onConfirm,
  onCancel,
}: ConfirmForcePushDialogProps) {
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const [typedValue, setTypedValue] = useState("");

  const matches = typedValue === branchName;

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
          inputRef.current?.focus();
        }}
        showCloseButton={false}
        aria-describedby={bodyId}
        className="block gap-0 p-0 w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-destructive/15 p-2 text-destructive">
            <AlertTriangle size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <DialogTitle className="text-[14px] font-semibold text-foreground">
              Force push branch?
            </DialogTitle>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              Force pushing{" "}
              <code className="rounded bg-background px-1 font-mono text-foreground">
                {branchName}
              </code>{" "}
              will overwrite the remote history. This cannot be undone and may
              cause others to lose work.
            </p>
          </div>
        </div>

        <div className="border-b border-border px-4 py-3">
          <label className="block text-[12px] text-muted-foreground">
            Type the branch name to confirm force push:
          </label>
          <Input
            ref={inputRef}
            type="text"
            className={clsx(
              "mt-1.5 w-full rounded border bg-background px-2 py-1.5 font-mono text-[12px] text-foreground focus-visible:outline-none transition",
              matches
                ? "border-primary focus:ring-1 focus:ring-primary"
                : "border-border focus:ring-1 focus:ring-border",
            )}
            value={typedValue}
            onChange={(e) => setTypedValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && matches) onConfirm();
            }}
            placeholder={branchName}
            autoComplete="off"
            spellCheck={false}
          />
        </div>

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
            disabled={!matches}
            className={clsx(
              "rounded border px-3 py-1.5 text-[12px] font-medium transition",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
              matches
                ? "border-destructive/60 bg-destructive/15 text-destructive hover:bg-destructive/25"
                : "cursor-not-allowed border-border bg-card text-muted-foreground opacity-50",
            )}
            aria-label="Force push"
          >
            Force push
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
