import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
/**
 * ConflictResolutionDialog — modal dialog for resolving a sync conflict.
 * Shows the conflicted file path, local and remote modification times,
 * and offers three resolution strategies: keep local, keep remote, or keep both.
 */

import clsx from "clsx";
import { AlertTriangle, Copy, Monitor, Server, X } from "lucide-react";
import { useRef } from "react";
import type { SyncConflict } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

export type ConflictResolution = "local" | "remote" | "both";

export interface ConflictResolutionDialogProps {
  conflict: SyncConflict;
  onResolve: (resolution: ConflictResolution) => void;
  onDismiss: () => void;
}

// ── Component ────────────────────────────────────────────────────

export function ConflictResolutionDialog({
  conflict,
  onResolve,
  onDismiss,
}: ConflictResolutionDialogProps) {
  const dismissRef = useRef<HTMLButtonElement | null>(null);
  const localTimeFormatted = new Date(conflict.localMtime).toLocaleString();
  const remoteTimeFormatted = new Date(conflict.remoteMtime).toLocaleString();

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          if (true) onDismiss();
        }
      }}
    >
      <DialogContent
        onOpenAutoFocus={(event) => {
          event.preventDefault();
    dismissRef.current?.focus();
        }}
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 w-[28rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div className="mt-0.5 shrink-0 rounded p-2 bg-warning/15 text-warning">
            <AlertTriangle size={20} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <DialogTitle className="text-[14px] font-semibold text-foreground"
            >
              Sync Conflict
            </DialogTitle>
            <p className="mt-0.5 text-[12px] text-muted-foreground">
              This file was modified on both sides since the last sync
            </p>
          </div>
          <Button
            variant="ghost"
            size="compact"
            ref={dismissRef}
            type="button"
            onClick={onDismiss}
            className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground transition"
            aria-label="Dismiss"
          >
            <X size={16} />
          </Button>
        </div>

        {/* Body */}
        <div className="space-y-4 p-4">
          {/* File path */}
          <div>
            <label className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
              File
            </label>
            <div className="mt-1 rounded border border-border bg-background px-3 py-2 font-mono text-[12px] text-foreground select-all break-all">
              {conflict.filePath}
            </div>
          </div>

          {/* Timestamps */}
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                <Monitor size={11} aria-hidden="true" />
                Local Modified
              </label>
              <div className="mt-1 rounded border border-border bg-background px-3 py-2 text-[12px] text-foreground">
                {localTimeFormatted}
              </div>
            </div>
            <div>
              <label className="flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                <Server size={11} aria-hidden="true" />
                Remote Modified
              </label>
              <div className="mt-1 rounded border border-border bg-background px-3 py-2 text-[12px] text-foreground">
                {remoteTimeFormatted}
              </div>
            </div>
          </div>
        </div>

        {/* Footer — resolution buttons */}
        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-border p-3">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => onResolve("local")}
            className={clsx(
              "flex items-center gap-1.5 rounded border px-3 py-1.5 text-[12px] font-medium transition",
              "border-primary/60 bg-primary/15 text-primary hover:bg-primary/25",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            <Monitor size={13} aria-hidden="true" />
            Keep Local
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => onResolve("remote")}
            className={clsx(
              "flex items-center gap-1.5 rounded border px-3 py-1.5 text-[12px] font-medium transition",
              "border-primary/60 bg-primary/15 text-primary hover:bg-primary/25",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            <Server size={13} aria-hidden="true" />
            Keep Remote
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => onResolve("both")}
            className={clsx(
              "flex items-center gap-1.5 rounded border px-3 py-1.5 text-[12px] font-medium transition",
              "border-primary/60 bg-primary/15 text-primary hover:bg-primary/25",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            <Copy size={13} aria-hidden="true" />
            Keep Both
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
