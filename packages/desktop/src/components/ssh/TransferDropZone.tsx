/**
 * TransferDropZone — drag-and-drop overlay for uploading files to a remote directory.
 *
 * Shows within 200ms of drag enter, dismisses on drag leave.
 * Queues uploads for dropped files (up to 500 per drop).
 * Supports recursive folder upload (max depth 50, skips symlinks).
 * Handles permission-denied errors with user notification.
 */

import { useState, useCallback, useRef, useEffect } from "react";
import { Upload, FolderUp, AlertCircle } from "lucide-react";
import clsx from "clsx";

// ── Types ────────────────────────────────────────────────────────

interface TransferDropZoneProps {
  /** The target remote directory path for uploads */
  remoteDir: string;
  /** SSH profile ID for the active connection */
  profileId: string;
  /** Whether the drop zone is enabled (connection active) */
  enabled?: boolean;
  /** Callback fired after uploads are queued (for refreshing file trees) */
  onUploadComplete?: () => void;
  /** Children to render beneath the overlay */
  children: React.ReactNode;
}

interface DropError {
  message: string;
  directory?: string;
}

// ── Constants ────────────────────────────────────────────────────

const MAX_FILES_PER_DROP = 500;
const MAX_FOLDER_DEPTH = 50;
const SHOW_DELAY_MS = 200;

// ── Helpers ──────────────────────────────────────────────────────

/**
 * Recursively collect file entries from a DataTransferItem,
 * respecting max depth and skipping symlinks.
 */
async function collectFilesFromEntry(
  entry: FileSystemEntry,
  basePath: string,
  depth: number,
  collected: File[],
): Promise<void> {
  if (depth > MAX_FOLDER_DEPTH) return;
  if (collected.length >= MAX_FILES_PER_DROP) return;

  if (entry.isFile) {
    const fileEntry = entry as FileSystemFileEntry;
    const file = await new Promise<File>((resolve, reject) => {
      fileEntry.file(resolve, reject);
    });
    // Attach relative path for directory structure preservation
    Object.defineProperty(file, "webkitRelativePath", {
      value: basePath ? `${basePath}/${entry.name}` : entry.name,
      writable: false,
    });
    collected.push(file);
  } else if (entry.isDirectory) {
    const dirEntry = entry as FileSystemDirectoryEntry;
    const reader = dirEntry.createReader();
    const entries = await new Promise<FileSystemEntry[]>((resolve, reject) => {
      reader.readEntries(resolve, reject);
    });
    for (const child of entries) {
      if (collected.length >= MAX_FILES_PER_DROP) break;
      const childPath = basePath ? `${basePath}/${entry.name}` : entry.name;
      await collectFilesFromEntry(child, childPath, depth + 1, collected);
    }
  }
  // Symlinks are skipped (they don't appear as isFile or isDirectory)
}

// ── Component ────────────────────────────────────────────────────

export function TransferDropZone({
  remoteDir,
  profileId,
  enabled = true,
  onUploadComplete,
  children,
}: TransferDropZoneProps) {
  const [showOverlay, setShowOverlay] = useState(false);
  const [isProcessing, setIsProcessing] = useState(false);
  const [error, setError] = useState<DropError | null>(null);
  const dragCounterRef = useRef(0);
  const showTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Clean up timer on unmount
  useEffect(() => {
    return () => {
      if (showTimerRef.current) clearTimeout(showTimerRef.current);
    };
  }, []);

  const handleDragEnter = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (!enabled) return;

      dragCounterRef.current += 1;
      if (dragCounterRef.current === 1) {
        // Show overlay after 200ms delay
        showTimerRef.current = setTimeout(() => {
          setShowOverlay(true);
          setError(null);
        }, SHOW_DELAY_MS);
      }
    },
    [enabled],
  );

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();

    dragCounterRef.current -= 1;
    if (dragCounterRef.current <= 0) {
      dragCounterRef.current = 0;
      if (showTimerRef.current) {
        clearTimeout(showTimerRef.current);
        showTimerRef.current = null;
      }
      setShowOverlay(false);
    }
  }, []);

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = "copy";
  }, []);

  const handleDrop = useCallback(
    async (e: React.DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCounterRef.current = 0;
      if (showTimerRef.current) {
        clearTimeout(showTimerRef.current);
        showTimerRef.current = null;
      }

      if (!enabled) {
        setShowOverlay(false);
        return;
      }

      setIsProcessing(true);
      setError(null);

      try {
        const items = e.dataTransfer.items;
        const collected: File[] = [];

        // Use webkitGetAsEntry for folder support
        const entries: FileSystemEntry[] = [];
        for (let i = 0; i < items.length; i++) {
          const entry = items[i].webkitGetAsEntry?.();
          if (entry) entries.push(entry);
        }

        if (entries.length > 0) {
          for (const entry of entries) {
            if (collected.length >= MAX_FILES_PER_DROP) break;
            await collectFilesFromEntry(entry, "", 0, collected);
          }
        } else {
          // Fallback: use files directly (no folder support)
          const files = e.dataTransfer.files;
          for (let i = 0; i < Math.min(files.length, MAX_FILES_PER_DROP); i++) {
            collected.push(files[i]);
          }
        }

        if (collected.length === 0) {
          setShowOverlay(false);
          setIsProcessing(false);
          return;
        }

        // Queue uploads via IPC
        const filePaths = collected.map((f) => ({
          localPath: (f as any).path || f.name,
          relativePath: f.webkitRelativePath || f.name,
        }));

        const result = await window.crab.ssh.queueDropUpload({
          profileId,
          remoteDir,
          files: filePaths,
        });

        if (result?.error === "PERMISSION_DENIED") {
          setError({
            message: "Permission denied: cannot write to this directory",
            directory: remoteDir,
          });
        } else {
          // Notify parent that uploads were queued so it can refresh.
          onUploadComplete?.();
        }
      } catch (err: any) {
        if (err?.code === "PERMISSION_DENIED" || err?.message?.includes("permission")) {
          setError({
            message: "Permission denied: cannot write to this directory",
            directory: remoteDir,
          });
        } else {
          setError({ message: err?.message || "Failed to queue uploads" });
        }
      } finally {
        setIsProcessing(false);
        setShowOverlay(false);
      }
    },
    [enabled, profileId, remoteDir, onUploadComplete],
  );

  const dismissError = useCallback(() => setError(null), []);

  return (
    <div
      className="relative flex min-h-0 h-full w-full flex-col"
      onDragEnter={handleDragEnter}
      onDragLeave={handleDragLeave}
      onDragOver={handleDragOver}
      onDrop={handleDrop}
    >
      {children}

      {/* Drop zone overlay */}
      {showOverlay && (
        <div
          className={clsx(
            "absolute inset-0 z-50 flex flex-col items-center justify-center",
            "bg-info/10 bg-info/10",
            "border-2 border-dashed border-info border-info",
            "rounded-lg backdrop-blur-sm",
            "transition-opacity duration-150",
          )}
          role="region"
          aria-label="Drop files to upload"
        >
          {isProcessing ? (
            <div className="flex flex-col items-center gap-2">
              <FolderUp className="w-10 h-10 text-info text-info animate-pulse" />
              <p className="text-sm font-medium text-info text-info">
                Processing files…
              </p>
            </div>
          ) : (
            <div className="flex flex-col items-center gap-2">
              <Upload className="w-10 h-10 text-info text-info" />
              <p className="text-sm font-medium text-info text-info">
                Drop to upload to:
              </p>
              <p className="text-xs text-info text-info font-mono px-3 py-1 bg-info/10 bg-info/40 rounded">
                {remoteDir}
              </p>
              <p className="text-xs text-muted-foreground text-muted-foreground mt-1">
                Up to {MAX_FILES_PER_DROP} files · Folders supported (max depth {MAX_FOLDER_DEPTH})
              </p>
            </div>
          )}
        </div>
      )}

      {/* Permission denied / error notification */}
      {error && (
        <div
          className={clsx(
            "absolute bottom-4 left-4 right-4 z-50",
            "flex items-start gap-2 px-3 py-2.5 rounded-lg",
            "bg-destructive/10 bg-destructive/30 border border-destructive border-destructive",
            "shadow-sm",
          )}
          role="alert"
        >
          <AlertCircle className="w-4 h-4 text-destructive text-destructive shrink-0 mt-0.5" />
          <div className="flex-1 min-w-0">
            <p className="text-sm font-medium text-destructive text-destructive">
              {error.message}
            </p>
            {error.directory && (
              <p className="text-xs text-destructive text-destructive truncate mt-0.5">
                {error.directory}
              </p>
            )}
          </div>
          <button
            type="button"
            onClick={dismissError}
            className="shrink-0 text-destructive hover:text-destructive hover:text-destructive"
            aria-label="Dismiss error"
          >
            ×
          </button>
        </div>
      )}
    </div>
  );
}
