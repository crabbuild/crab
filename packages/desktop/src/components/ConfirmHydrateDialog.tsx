import { Download, FileX, HardDrive, Loader2 } from "lucide-react";
import { formatBytes } from "../lib/format";

interface Props {
  open: boolean;
  verb: "hydrate" | "dehydrate";
  /** Number of files that will be affected. */
  fileCount: number;
  /** Total logical bytes across the affected files. */
  totalBytes: number;
  /** Free disk space on the target drive, if known. */
  freeBytes?: number | null;
  busy: boolean;
  error?: string | null;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * Confirmation sheet shown before a bulk hydrate/dehydrate. Surfaces
 * file count, disk impact, and (when we know it) available disk space.
 *
 * Non-technical users benefit from a clear "this will use X GB"
 * statement before a multi-hour hydrate kicks off.
 */
export function ConfirmHydrateDialog({
  open,
  verb,
  fileCount,
  totalBytes,
  freeBytes,
  busy,
  error,
  onConfirm,
  onCancel,
}: Props) {
  if (!open) return null;

  const isHydrate = verb === "hydrate";
  const Icon = isHydrate ? Download : FileX;
  const title = isHydrate ? "Hydrate files?" : "Dehydrate files?";
  const body = isHydrate
    ? "Fetches and materializes content from object storage. Uses disk space equal to the logical size."
    : "Replaces working-tree files with pointer blobs. Frees disk space; dehydrated files can be re-hydrated at any time.";
  const wouldUseMoreThanFree =
    isHydrate && freeBytes != null && freeBytes < totalBytes;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      onClick={onCancel}
    >
      <div
        className="w-[min(520px,92vw)] rounded-lg border border-gx-border bg-gx-surface p-5 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center gap-2 text-base font-semibold">
          <Icon size={16} className="text-gx-accent" />
          {title}
        </div>
        <p className="mb-4 text-sm text-gx-muted">{body}</p>

        <div className="mb-4 space-y-2 rounded border border-gx-border bg-gx-bg-sub p-3 text-sm">
          <Row label="Files affected" value={fileCount.toLocaleString()} />
          <Row label="Logical size" value={formatBytes(totalBytes)} />
          {freeBytes != null && (
            <Row
              label="Free disk space"
              value={formatBytes(freeBytes)}
              warn={wouldUseMoreThanFree}
            />
          )}
        </div>

        {wouldUseMoreThanFree && (
          <div className="mb-3 rounded border border-gx-warn/40 bg-gx-warn/10 p-3 text-xs text-gx-warn">
            <HardDrive size={12} className="mr-1 inline" />
            The files to hydrate are larger than the free space on this disk.
            Proceeding may cause writes to fail mid-operation.
          </div>
        )}

        {error && (
          <div className="mb-3 rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
            {error}
          </div>
        )}

        <div className="flex justify-end gap-2">
          <button type="button" className="btn" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={onConfirm}
            disabled={busy}
          >
            {busy ? (
              <>
                <Loader2 size={12} className="animate-spin" /> {isHydrate ? "Hydrating…" : "Dehydrating…"}
              </>
            ) : (
              <>{isHydrate ? "Hydrate" : "Dehydrate"}</>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}

function Row({
  label,
  value,
  warn,
}: {
  label: string;
  value: string;
  warn?: boolean;
}) {
  return (
    <div className="flex items-center justify-between text-[13px]">
      <span className="text-gx-muted">{label}</span>
      <span className={warn ? "font-mono text-gx-warn" : "font-mono text-gx-text"}>
        {value}
      </span>
    </div>
  );
}
