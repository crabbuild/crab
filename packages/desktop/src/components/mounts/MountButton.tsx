/**
 * Mount button component for the repo context menu.
 *
 * Opens a dialog to configure mount options (mountpoint, ref, read-only)
 * and calls the mount IPC to create a new FUSE mount.
 */

import { useState } from "react";
import { HardDrive, Loader2, X } from "lucide-react";

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
    <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50">
      <div className="w-96 rounded-lg border border-gx-border bg-gx-surface shadow-2xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-gx-border px-4 py-3">
          <h3 className="text-sm font-semibold text-gx-text">Mount Repository</h3>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={14} />
          </button>
        </div>

        {/* Body */}
        <div className="space-y-3 px-4 py-4">
          {/* Source */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-gx-muted uppercase tracking-wider">
              Source
            </label>
            <div className="rounded border border-gx-border bg-gx-bg px-3 py-1.5 text-xs text-gx-text truncate">
              {repo}
            </div>
          </div>

          {/* Mountpoint */}
          <div>
            <label
              htmlFor="mount-mountpoint"
              className="mb-1 block text-[11px] font-medium text-gx-muted uppercase tracking-wider"
            >
              Mountpoint
            </label>
            <input
              id="mount-mountpoint"
              type="text"
              className="w-full rounded border border-gx-border bg-gx-bg px-3 py-1.5 text-xs text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus:outline-none"
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
              className="mb-1 block text-[11px] font-medium text-gx-muted uppercase tracking-wider"
            >
              Branch / Ref
            </label>
            <input
              id="mount-ref"
              type="text"
              className="w-full rounded border border-gx-border bg-gx-bg px-3 py-1.5 text-xs text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus:outline-none"
              placeholder="HEAD"
              value={ref}
              onChange={(e) => setRef(e.target.value)}
              disabled={busy}
            />
          </div>

          {/* Read-only toggle */}
          <label className="flex items-center gap-2 cursor-pointer">
            <input
              type="checkbox"
              className="rounded border-gx-border"
              checked={readOnly}
              onChange={(e) => setReadOnly(e.target.checked)}
              disabled={busy}
            />
            <span className="text-xs text-gx-text">Read-only (no overlay writes)</span>
          </label>

          {/* Error */}
          {error && (
            <div className="rounded border border-gx-error/40 bg-gx-error/10 px-3 py-2 text-xs text-gx-error">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-2 border-t border-gx-border px-4 py-3">
          <button
            type="button"
            className="rounded px-3 py-1.5 text-xs text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover border border-gx-border transition"
            onClick={onClose}
            disabled={busy}
          >
            Cancel
          </button>
          <button
            type="button"
            className="rounded bg-gx-accent px-3 py-1.5 text-xs text-white hover:bg-gx-accent/90 transition disabled:opacity-50"
            onClick={handleMount}
            disabled={busy || !mountpoint.trim()}
          >
            {busy ? <Loader2 size={12} className="animate-spin" /> : "Mount"}
          </button>
        </div>
      </div>
    </div>
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
      <button
        type="button"
        className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-gx-text hover:bg-gx-accent/10 transition"
        onClick={() => setDialogOpen(true)}
        title="Mount this repository as a virtual filesystem"
      >
        <HardDrive size={12} />
        <span>Mount</span>
      </button>

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
