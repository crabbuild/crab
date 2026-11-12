import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import clsx from "clsx";
import { GitMerge, Loader2 } from "lucide-react";
import { useId, useRef, useState } from "react";
import { MergeStrategy, rpc } from "../ipc";

/**
 * Modal dialog for initiating a merge operation.
 *
 * Pre-fills source (from context menu selection) and target (current
 * HEAD branch). The user picks a strategy and optionally edits the
 * commit message before confirming.
 *
 * On success the dialog closes and fires `onSuccess` so the parent
 * can refresh branches and show a toast. On conflict (state "paused")
 * it closes and fires `onConflicts` so the parent can surface a
 * "merge paused" toast. Errors are shown inline.
 */
export interface MergeDialogProps {
  /** Branch being merged (source). */
  sourceBranch: string;
  /** Current HEAD branch (target). */
  targetBranch: string;
  /** Active repo id for the RPC call. */
  repoId: string;
  /** Called after a successful merge (state "done"). */
  onSuccess: () => void;
  /** Called when the merge pauses due to conflicts. */
  onConflicts: (conflictCount: number) => void;
  /** Called to close the dialog without action. */
  onCancel: () => void;
}

export function MergeDialog({
  sourceBranch,
  targetBranch,
  repoId,
  onSuccess,
  onConflicts,
  onCancel,
}: MergeDialogProps) {
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);

  const [strategy, setStrategy] = useState<MergeStrategy>("MergeCommit");
  const [message, setMessage] = useState(
    `Merge branch '${sourceBranch}' into ${targetBranch}`,
  );
  const [merging, setMerging] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleMerge() {
    setMerging(true);
    setError(null);
    try {
      const result = await rpc.opStart(repoId, {
        kind: "Merge",
        from: sourceBranch,
        into: targetBranch,
        strategy,
        message: message.trim() || undefined,
      });

      if (result.state === "done") {
        onSuccess();
      } else if (result.state === "paused") {
        onConflicts(result.conflicts?.length ?? 0);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setMerging(false);
    }
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onCancel();
      }}}
    >
      <DialogContent
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          cancelRef.current?.focus();
        }}
        showCloseButton={false}
        aria-describedby={bodyId}
        className="block gap-0 p-0 w-[30rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-primary/15 p-2 text-primary">
            <GitMerge size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <DialogTitle className="text-[14px] font-semibold text-foreground"
            >
              Merge branch
            </DialogTitle>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-muted-foreground">
              Merge{" "}
              <code className="rounded bg-background px-1 font-mono text-foreground">
                {sourceBranch}
              </code>{" "}
              into{" "}
              <code className="rounded bg-background px-1 font-mono text-foreground">
                {targetBranch}
              </code>
            </p>
          </div>
        </div>

        {/* Body */}
        <div className="space-y-4 border-b border-border px-4 py-4">
          {/* Source / Target (read-only) */}
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="block text-[11px] font-medium text-muted-foreground">
                Source
              </label>
              <div className="mt-1 truncate rounded border border-border bg-background px-2 py-1.5 font-mono text-[12px] text-foreground">
                {sourceBranch}
              </div>
            </div>
            <div>
              <label className="block text-[11px] font-medium text-muted-foreground">
                Target
              </label>
              <div className="mt-1 truncate rounded border border-border bg-background px-2 py-1.5 font-mono text-[12px] text-foreground">
                {targetBranch}
              </div>
            </div>
          </div>

          {/* Strategy */}
          <fieldset>
            <legend className="text-[11px] font-medium text-muted-foreground">
              Strategy
            </legend>
            <ToggleGroup
              type="single"
              value={strategy}
              onValueChange={(value) => {
                if (value) setStrategy(value as MergeStrategy);
              }}
              aria-label="Merge strategy"
              className="mt-1.5 justify-start gap-2"
            >
              <ToggleGroupItem
                value="MergeCommit"
                variant="outline"
                className="h-7 text-xs"
              >
                Merge commit
              </ToggleGroupItem>
              <ToggleGroupItem
                value="Squash"
                variant="outline"
                className="h-7 text-xs"
              >
                Squash
              </ToggleGroupItem>
              <ToggleGroupItem
                value="FastForwardOnly"
                variant="outline"
                className="h-7 text-xs"
              >
                Fast-forward only
              </ToggleGroupItem>
            </ToggleGroup>
          </fieldset>

          {/* Commit message */}
          <div>
            <label
              htmlFor="merge-message"
              className="block text-[11px] font-medium text-muted-foreground"
            >
              Commit message
            </label>
            <Input
              id="merge-message"
              type="text"
              className="mt-1 w-full rounded border border-border bg-background px-2 py-1.5 font-mono text-[12px] text-foreground focus-visible:outline-none transition focus:ring-1 focus:ring-primary"
              value={message}
              onChange={(e) => setMessage(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !merging) {
                  e.preventDefault();
                  handleMerge();
                }
              }}
              disabled={merging}
              autoComplete="off"
              spellCheck={false}
            />
          </div>

          {/* Error */}
          {error && (
            <div className="rounded border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 p-3">
          <Button
            variant="ghost"
            size="compact"
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            disabled={merging}
            className={clsx(
              "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
              "disabled:opacity-50",
            )}
          >
            Cancel
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={handleMerge}
            disabled={merging}
            className={clsx(
              "flex items-center gap-1.5 rounded border border-primary/60 bg-primary/15 px-3 py-1.5 text-[12px] font-medium text-primary transition",
              "hover:bg-primary/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
              "disabled:opacity-50",
            )}
          >
            {merging && <Loader2 size={12} className="animate-spin" />}
            Merge
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
