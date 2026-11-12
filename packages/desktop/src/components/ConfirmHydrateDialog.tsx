import { useRef } from "react";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
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
  const cancelRef = useRef<HTMLButtonElement>(null);
  const triggerRef = useRef<HTMLElement | null>(null);

  const isHydrate = verb === "hydrate";
  const Icon = isHydrate ? Download : FileX;
  const title = isHydrate ? "Hydrate files?" : "Dehydrate files?";
  const body = isHydrate
    ? "Fetches and materializes content from object storage. Uses disk space equal to the logical size."
    : "Checks the working files and their saved content before replacing them with pointers to free disk space.";
  const wouldUseMoreThanFree =
    isHydrate && freeBytes != null && freeBytes < totalBytes;

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next) onCancel(); }}>
      <DialogContent className="w-[min(520px,92vw)] gap-0 bg-card p-5" showCloseButton={false}
        onOpenAutoFocus={(event) => {
          // The trigger belongs to the calling view, outside this controlled
          // Dialog root. Radix can only restore its own DialogTrigger ref.
          triggerRef.current = document.activeElement as HTMLElement | null;
          event.preventDefault();
          cancelRef.current?.focus();
        }}
        onCloseAutoFocus={(event) => { event.preventDefault(); triggerRef.current?.focus(); }}>
        <DialogTitle className="mb-3 flex items-center gap-2 text-base font-semibold">
          <Icon size={16} className="text-primary" />
          {title}
        </DialogTitle>
        <DialogDescription className="mb-4 text-sm text-muted-foreground">{body}</DialogDescription>

        <div className="mb-4 space-y-2 rounded border border-border bg-background p-3 text-sm">
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
          <div className="mb-3 rounded border border-warning/40 bg-warning/10 p-3 text-xs text-warning">
            <HardDrive size={12} className="mr-1 inline" />
            The files to hydrate are larger than the free space on this disk.
            Proceeding may cause writes to fail mid-operation.
          </div>
        )}

        {error && (
          <div className="mb-3 rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
            {error}
          </div>
        )}

        <div className="flex justify-end gap-2">
          <Button ref={cancelRef} variant="outline" type="button" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
          <Button variant="default"
            type="button"

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
          </Button>
        </div>
      </DialogContent>
    </Dialog>
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
      <span className="text-muted-foreground">{label}</span>
      <span className={warn ? "font-mono text-warning" : "font-mono text-foreground"}>
        {value}
      </span>
    </div>
  );
}
