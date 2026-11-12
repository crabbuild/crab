import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
/**
 * Mount button component for the repo context menu.
 *
 * Opens a dialog to configure mount options (mountpoint, ref, read-only)
 * and calls the mount IPC to create a new FUSE mount.
 */

import { HardDrive, Loader2, X } from "lucide-react";
import { useState } from "react";

interface MountButtonProps {
  /** Repository URL or local path to mount. */
  repo: string;
  /** Optional default ref (branch/tag). */
  defaultRef?: string;
  /** Callback after successful mount. */
  onMounted?: (mountpoint: string) => void;
}

interface MountDialogProps {
  open: boolean;
  repo: string;
  defaultRef?: string;
  onClose: () => void;
  onMounted?: (mountpoint: string) => void;
}

function MountDialog({ open, repo, defaultRef, onClose, onMounted }: MountDialogProps) {
  const [mountpoint, setMountpoint] = useState("");
  const [ref, setRef] = useState(defaultRef ?? "HEAD");
  const [readOnly, setReadOnly] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!open) return null;

  async function handleMount() {
    if (!mountpoint.trim()) {
      setError("Mountpoint is required");
      return;
    }

    setBusy(true);
    setError(null);

    try {
      const result = await window.crab.mount.mount({
        repo,
        mountpoint: mountpoint.trim(),
        ref: ref || "HEAD",
        readOnly,
      });

      if (!result.ok) {
        setError(result.error ?? "Mount failed");
        return;
      }

      onMounted?.(mountpoint.trim());
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 w-96 rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <DialogTitle className="text-sm font-semibold text-foreground">
            Mount Repository
          </DialogTitle>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground transition"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={14} />
          </Button>
        </div>

        {/* Body */}
        <div className="space-y-3 px-4 py-4">
          {/* Source */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-muted-foreground uppercase tracking-wider">
              Source
            </label>
            <div className="rounded border border-border bg-background px-3 py-1.5 text-xs text-foreground truncate">
              {repo}
            </div>
          </div>

          {/* Mountpoint */}
          <div>
            <label
              htmlFor="mount-mountpoint"
              className="mb-1 block text-[11px] font-medium text-muted-foreground uppercase tracking-wider"
            >
              Mountpoint
            </label>
            <Input
              id="mount-mountpoint"
              type="text"
              className="w-full rounded border border-border bg-background px-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
              placeholder="/mnt/my-repo"
              value={mountpoint}
              onChange={(e) => setMountpoint(e.target.value)}
              disabled={busy}
              autoFocus
            />
          </div>

          {/* Ref */}
          <div>
            <label
              htmlFor="mount-ref"
              className="mb-1 block text-[11px] font-medium text-muted-foreground uppercase tracking-wider"
            >
              Branch / Ref
            </label>
            <Input
              id="mount-ref"
              type="text"
              className="w-full rounded border border-border bg-background px-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
              placeholder="HEAD"
              value={ref}
              onChange={(e) => setRef(e.target.value)}
              disabled={busy}
            />
          </div>

          {/* Read-only toggle */}
          <label className="flex items-center gap-2 cursor-pointer">
            <Checkbox
              className="rounded border-border"
              checked={readOnly}
              onCheckedChange={(checked) => setReadOnly(checked === true)}
              disabled={busy}
            />
            <span className="text-xs text-foreground">
              Read-only (no overlay writes)
            </span>
          </label>

          {/* Error */}
          {error && (
            <div className="rounded border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-2 border-t border-border px-4 py-3">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-3 py-1.5 text-xs text-muted-foreground hover:text-foreground hover:bg-accent border border-border transition"
            onClick={onClose}
            disabled={busy}
          >
            Cancel
          </Button>
          <Button
            variant="default"
            size="compact"
            type="button"
            className="rounded bg-primary px-3 py-1.5 text-xs text-primary-foreground hover:bg-primary/90 transition disabled:opacity-50"
            onClick={handleMount}
            disabled={busy || !mountpoint.trim()}
          >
            {busy ? <Loader2 size={12} className="animate-spin" /> : "Mount"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/**
 * Button that opens the mount dialog for a repository.
 * Intended for use in repo context menus or action bars.
 */
export function MountButton({ repo, defaultRef, onMounted }: MountButtonProps) {
  const [dialogOpen, setDialogOpen] = useState(false);

  return (
    <>
      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-primary/10 transition"
        onClick={() => setDialogOpen(true)}
        title="Mount this repository as a virtual filesystem"
      >
        <HardDrive size={12} />
        <span>Mount</span>
      </Button>

      <MountDialog
        open={dialogOpen}
        repo={repo}
        defaultRef={defaultRef}
        onClose={() => setDialogOpen(false)}
        onMounted={onMounted}
      />
    </>
  );
}
