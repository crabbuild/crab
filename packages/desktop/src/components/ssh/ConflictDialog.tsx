import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
/**
 * ConflictDialog — modal dialog for handling duplicate file conflicts during
 * drag-and-drop uploads. Prompts the user to overwrite, skip, or rename
 * with an "apply to all" option for batch resolution.
 */

import clsx from "clsx";
import { AlertTriangle, FileWarning, X } from "lucide-react";
import { useCallback, useState } from "react";

// ── Types ────────────────────────────────────────────────────────

export type ConflictResolution = "overwrite" | "skip" | "rename";

export interface FileConflict {
  /** Local file name being uploaded */
  fileName: string;
  /** Full remote path of the existing file */
  remotePath: string;
  /** Size of the existing remote file in bytes */
  existingSize?: number;
  /** Size of the new local file in bytes */
  newSize?: number;
}

export interface ConflictDialogProps {
  /** Whether the dialog is visible */
  open: boolean;
  /** The current conflict to resolve */
  conflict: FileConflict | null;
  /** Total number of remaining conflicts in this batch */
  remainingCount: number;
  /** Called when the user makes a resolution choice */
  onResolve: (resolution: ConflictResolution, applyToAll: boolean) => void;
  /** Called when the dialog is dismissed without resolution */
  onCancel: () => void;
}

// ── Component ────────────────────────────────────────────────────

export function ConflictDialog({
  open,
  conflict,
  remainingCount,
  onResolve,
  onCancel,
}: ConflictDialogProps) {
  const [applyToAll, setApplyToAll] = useState(false);

  const handleResolve = useCallback(
    (resolution: ConflictResolution) => {
      onResolve(resolution, applyToAll);
      setApplyToAll(false);
    },
    [applyToAll, onResolve],
  );

  const handleCancel = useCallback(() => {
    setApplyToAll(false);
    onCancel();
  }, [onCancel]);

  if (!open || !conflict) return null;

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) handleCancel();
      }}
    >
      {/* Backdrop */}

      {/* Dialog panel */}
      <DialogContent
        aria-describedby={undefined}
        showCloseButton={false}
        className={
          clsx(
          "w-full max-w-md mx-4",
          "bg-background bg-card rounded-xl shadow-xl",
          "border border-border border-border",
        ) + " p-0 gap-0"
        }
      >
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-border border-border">
          <div className="flex items-center gap-2">
            <FileWarning className="w-5 h-5 text-warning" />
            <DialogTitle className="text-base font-semibold text-foreground text-foreground">
              File Already Exists
            </DialogTitle>
          </div>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={handleCancel}
            className="p-1 rounded hover:bg-card hover:bg-card text-muted-foreground hover:text-muted-foreground hover:text-muted-foreground"
            aria-label="Close dialog"
          >
            <X size={16} />
          </Button>
        </div>

        {/* Body */}
        <div className="px-5 py-4">
          <div className="flex items-start gap-3 mb-4">
            <AlertTriangle className="w-5 h-5 text-warning shrink-0 mt-0.5" />
            <div className="min-w-0">
              <p className="text-sm text-foreground text-muted-foreground">
                A file named{" "}
                <span className="font-medium text-foreground text-foreground">
                  {conflict.fileName}
                </span>{" "}
                already exists in the target directory.
              </p>
              <p className="text-xs text-muted-foreground text-muted-foreground mt-1 truncate" title={conflict.remotePath}>
                {conflict.remotePath}
              </p>
              {(conflict.existingSize !== undefined || conflict.newSize !== undefined) && (
                <div className="flex gap-4 mt-2 text-xs text-muted-foreground text-muted-foreground">
                  {conflict.existingSize !== undefined && (
                    <span>
                      Existing: {formatFileSize(conflict.existingSize)}
                    </span>
                  )}
                  {conflict.newSize !== undefined && (
                    <span>New: {formatFileSize(conflict.newSize)}</span>
                  )}
                </div>
              )}
            </div>
          </div>

          {/* Apply to all checkbox */}
          {remainingCount > 1 && (
            <label className="flex items-center gap-2 mb-4 cursor-pointer">
              <Checkbox
                checked={applyToAll}
                onCheckedChange={(checked) => setApplyToAll(checked === true)}
                className="w-4 h-4 rounded border-border border-border text-info focus:ring-info"
              />
              <span className="text-sm text-muted-foreground text-muted-foreground">
                Apply to all ({remainingCount} remaining conflicts)
              </span>
            </label>
          )}
        </div>

        {/* Actions */}
        <div className="flex items-center justify-end gap-2 px-5 py-3 border-t border-border border-border bg-background bg-card/50 rounded-b-xl">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => handleResolve("skip")}
            className={clsx(
              "px-3 py-1.5 rounded-md text-sm font-medium transition-colors",
              "text-foreground text-muted-foreground",
              "hover:bg-card hover:bg-card",
              "border border-border border-border",
            )}
          >
            Skip
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => handleResolve("rename")}
            className={clsx(
              "px-3 py-1.5 rounded-md text-sm font-medium transition-colors",
              "text-info text-info",
              "hover:bg-info/10 hover:bg-info/30",
              "border border-info border-info",
            )}
          >
            Rename
          </Button>
          <Button
            variant="destructive"
            size="compact"
            type="button"
            onClick={() => handleResolve("overwrite")}
            className={clsx(
              "px-3 py-1.5 rounded-md text-sm font-medium transition-colors",
              "text-destructive-foreground bg-destructive hover:bg-destructive",
              "bg-destructive hover:bg-destructive",
            )}
          >
            Overwrite
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── Helpers ──────────────────────────────────────────────────────

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex++;
  }
  return `${value.toFixed(1)} ${units[unitIndex]}`;
}
