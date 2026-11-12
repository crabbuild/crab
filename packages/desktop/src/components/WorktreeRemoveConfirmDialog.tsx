import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import clsx from "clsx";
import { AlertTriangle, Trash2 } from "lucide-react";
import { useId, useRef, useState } from "react";

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
  const bodyId = useId();
  const inputRef = useRef<HTMLInputElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);

  const [typedValue, setTypedValue] = useState("");

  const isDirty = dirtyCount > 0;
  const confirmWord = "REMOVE";
  const matches = typedValue === confirmWord;

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
          if (isDirty) {
      inputRef.current?.focus();
    } else {
      cancelRef.current?.focus();
    }
        }}
        showCloseButton={false}
        aria-describedby={bodyId}
        className="block gap-0 p-0 w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div
            className={clsx(
              "mt-0.5 shrink-0 rounded p-2",
              isDirty
                ? "bg-destructive/15 text-destructive"
                : "bg-warning/15 text-warning",
            )}
          >
            {isDirty ? (
              <AlertTriangle size={18} aria-hidden="true" />
            ) : (
              <Trash2 size={18} aria-hidden="true" />
            )}
          </div>
          <div className="min-w-0 flex-1">
            <DialogTitle className="text-[14px] font-semibold text-foreground">
              {isDirty ? "Force remove worktree with uncommitted changes?" : "Remove worktree?"}
            </DialogTitle>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              {isDirty ? (
                <>
                  Worktree{" "}
                  <code className="rounded bg-background px-1 font-mono text-foreground">
                    {worktreeName}
                  </code>{" "}
                  has{" "}
                  <span className="font-semibold text-warning">
                    {dirtyCount} uncommitted change{dirtyCount !== 1 ? "s" : ""}
                  </span>
                  . Force removing will permanently discard these changes.
                </>
              ) : (
                <>
                  Remove worktree{" "}
                  <code className="rounded bg-background px-1 font-mono text-foreground">
                    {worktreeName}
                  </code>
                  ? This cannot be undone.
                </>
              )}
            </p>
          </div>
        </div>

        {/* Path display */}
        <div className="border-b border-border px-4 py-2">
          <span className="text-[11px] text-muted-foreground">Path: </span>
          <code className="font-mono text-[11px] text-foreground">
            {worktreePath}
          </code>
        </div>

        {/* Typed confirmation for dirty worktrees */}
        {isDirty && (
          <div className="border-b border-border px-4 py-3">
            <label className="block text-[12px] text-muted-foreground">
              Type{" "}
              <code className="rounded bg-background px-1 font-mono font-semibold text-destructive">
                REMOVE
              </code>{" "}
              to confirm force removal:
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
          {isDirty ? (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => onConfirm(true)}
              disabled={!matches}
              className={clsx(
                "rounded border px-3 py-1.5 text-[12px] font-medium transition",
                "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
                matches
                  ? "border-destructive/60 bg-destructive/15 text-destructive hover:bg-destructive/25"
                  : "cursor-not-allowed border-border bg-card text-muted-foreground opacity-50",
              )}
              aria-label="Force Remove"
            >
              Force Remove
            </Button>
          ) : (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => onConfirm(false)}
              className={clsx(
                "rounded border border-destructive/60 bg-destructive/15 px-3 py-1.5 text-[12px] font-medium text-destructive transition",
                "hover:bg-destructive/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-destructive",
              )}
              aria-label="Remove"
            >
              Remove
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
