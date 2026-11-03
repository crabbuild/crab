/**
 * ConflictDialog — modal dialog for handling duplicate file conflicts during
 * drag-and-drop uploads. Prompts the user to overwrite, skip, or rename
 * with an "apply to all" option for batch resolution.
 */

import { useState, useCallback } from "react";
import { AlertTriangle, FileWarning, X } from "lucide-react";
import clsx from "clsx";

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
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center"
      role="dialog"
      aria-modal="true"
      aria-labelledby="conflict-dialog-title"
    >
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/40 dark:bg-black/60"
        onClick={handleCancel}
        aria-hidden="true"
      />

      {/* Dialog panel */}
      <div
        className={clsx(
          "relative z-10 w-full max-w-md mx-4",
          "bg-white dark:bg-gray-800 rounded-xl shadow-xl",
          "border border-gray-200 dark:border-gray-700",
        )}
      >
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-gray-100 dark:border-gray-700">
          <div className="flex items-center gap-2">
            <FileWarning className="w-5 h-5 text-yellow-500" />
            <h2
              id="conflict-dialog-title"
              className="text-base font-semibold text-gray-800 dark:text-gray-100"
            >
              File Already Exists
            </h2>
          </div>
          <button
            type="button"
            onClick={handleCancel}
            className="p-1 rounded hover:bg-gray-100 dark:hover:bg-gray-700 text-gray-400 hover:text-gray-600 dark:hover:text-gray-300"
            aria-label="Close dialog"
          >
            <X size={16} />
          </button>
        </div>

        {/* Body */}
        <div className="px-5 py-4">
          <div className="flex items-start gap-3 mb-4">
            <AlertTriangle className="w-5 h-5 text-yellow-500 shrink-0 mt-0.5" />
            <div className="min-w-0">
              <p className="text-sm text-gray-700 dark:text-gray-300">
                A file named{" "}
                <span className="font-medium text-gray-900 dark:text-gray-100">
                  {conflict.fileName}
                </span>{" "}
                already exists in the target directory.
              </p>
              <p className="text-xs text-gray-500 dark:text-gray-400 mt-1 truncate" title={conflict.remotePath}>
                {conflict.remotePath}
              </p>
              {(conflict.existingSize !== undefined || conflict.newSize !== undefined) && (
                <div className="flex gap-4 mt-2 text-xs text-gray-500 dark:text-gray-400">
                  {conflict.existingSize !== undefined && (
                    <span>Existing: {formatFileSize(conflict.existingSize)}</span>
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
              <input
                type="checkbox"
                checked={applyToAll}
                onChange={(e) => setApplyToAll(e.target.checked)}
                className="w-4 h-4 rounded border-gray-300 dark:border-gray-600 text-blue-600 focus:ring-blue-500"
              />
              <span className="text-sm text-gray-600 dark:text-gray-400">
                Apply to all ({remainingCount} remaining conflicts)
              </span>
            </label>
          )}
        </div>

        {/* Actions */}
        <div className="flex items-center justify-end gap-2 px-5 py-3 border-t border-gray-100 dark:border-gray-700 bg-gray-50 dark:bg-gray-800/50 rounded-b-xl">
          <button
            type="button"
            onClick={() => handleResolve("skip")}
            className={clsx(
              "px-3 py-1.5 rounded-md text-sm font-medium transition-colors",
              "text-gray-700 dark:text-gray-300",
              "hover:bg-gray-100 dark:hover:bg-gray-700",
              "border border-gray-200 dark:border-gray-600",
            )}
          >
            Skip
          </button>
          <button
            type="button"
            onClick={() => handleResolve("rename")}
            className={clsx(
              "px-3 py-1.5 rounded-md text-sm font-medium transition-colors",
              "text-blue-700 dark:text-blue-300",
              "hover:bg-blue-50 dark:hover:bg-blue-900/30",
              "border border-blue-200 dark:border-blue-700",
            )}
          >
            Rename
          </button>
          <button
            type="button"
            onClick={() => handleResolve("overwrite")}
            className={clsx(
              "px-3 py-1.5 rounded-md text-sm font-medium transition-colors",
              "text-white bg-red-600 hover:bg-red-700",
              "dark:bg-red-700 dark:hover:bg-red-600",
            )}
          >
            Overwrite
          </button>
        </div>
      </div>
    </div>
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
