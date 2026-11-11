/**
 * Live File Tree — Real-time view of files the agent has modified.
 *
 * Only shows files with actual edits (not reads). Updates incrementally
 * as the agent emits file_edit events. Clicking a file navigates to
 * its diff in the Diff tab.
 */

import { useEffect, useMemo, useState } from "react";
import { FileCode, FilePlus, FileX } from "lucide-react";
import clsx from "clsx";

// ── Types ───────────────────────────────────────────────────────────

export interface LiveFile {
  path: string;
  status: "added" | "modified" | "deleted" | "reading";
  /** Timestamp of the last edit event for this file. */
  lastTouched: number;
  /** Number of edit events for this file. */
  editCount: number;
}

interface Props {
  /** Map of file path → LiveFile, updated by the parent as events arrive. */
  files: Map<string, LiveFile>;
  /** Whether the agent is currently running. */
  isStreaming: boolean;
  /** Callback when a file is clicked — passes the file path. */
  onSelectFile?: (path: string) => void;
  /** Currently selected file path. */
  selectedFile?: string | null;
}

// ── Component ───────────────────────────────────────────────────────

export function LiveFileTree({ files, isStreaming, onSelectFile, selectedFile }: Props) {
  const [now, setNow] = useState(Date.now());

  // Tick every 2s to update "recency" indicators.
  useEffect(() => {
    if (!isStreaming) return;
    const interval = setInterval(() => setNow(Date.now()), 2000);
    return () => clearInterval(interval);
  }, [isStreaming]);

  // Only show files that were actually edited (not just read).
  const editedFiles = useMemo(() => {
    return Array.from(files.values())
      .filter((f) => f.editCount > 0)
      .sort((a, b) => b.lastTouched - a.lastTouched);
  }, [files]);

  if (editedFiles.length === 0) {
    if (!isStreaming) {
      return (
        <div className="flex h-full items-center justify-center px-3 text-center text-[11px] text-muted-foreground">
          No files modified.
        </div>
      );
    }
    // Don't show a "waiting" placeholder — the parent hides us until files arrive.
    return null;
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Header */}
      <div className="sticky top-0 z-10 flex items-center gap-2 border-b border-border bg-card px-3 py-1.5 text-[10px] text-muted-foreground">
        <span>{editedFiles.length} file{editedFiles.length !== 1 ? "s" : ""} modified</span>
        {isStreaming && (
          <span className="ml-auto flex items-center gap-1 text-success">
            <span className="h-1.5 w-1.5 rounded-full bg-success animate-pulse" />
            Live
          </span>
        )}
      </div>

      {/* File list */}
      <div className="flex-1 overflow-auto">
        {editedFiles.map((file) => {
          const isRecent = now - file.lastTouched < 3000;
          const isSelected = selectedFile === file.path;

          return (
            <button
              key={file.path}
              type="button"
              onClick={() => onSelectFile?.(file.path)}
              className={clsx(
                "group flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] transition",
                isSelected
                  ? "bg-primary/10 text-foreground"
                  : "text-muted-foreground hover:bg-accent hover:text-foreground",
                isRecent && isStreaming && !isSelected && "bg-success/5",
              )}
              title={file.path}
            >
              {/* Status icon */}
              <FileStatusIcon status={file.status} isRecent={isRecent && isStreaming} />

              {/* File name (just the basename) */}
              <span className="min-w-0 flex-1 truncate font-mono text-[10px]">
                {file.path.split("/").pop()}
              </span>

              {/* Edit count badge */}
              {file.editCount > 1 && (
                <span className="shrink-0 rounded-full bg-primary/10 px-1.5 text-[9px] text-primary">
                  {file.editCount}
                </span>
              )}

              {/* Recency pulse */}
              {isRecent && isStreaming && (
                <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-success animate-pulse" />
              )}
            </button>
          );
        })}
      </div>
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function FileStatusIcon({ status, isRecent }: { status: LiveFile["status"]; isRecent: boolean }) {
  const baseClass = "shrink-0";

  switch (status) {
    case "added":
      return <FilePlus size={12} className={clsx(baseClass, "text-success")} />;
    case "deleted":
      return <FileX size={12} className={clsx(baseClass, "text-destructive")} />;
    case "modified":
    default:
      return <FileCode size={12} className={clsx(baseClass, "text-warning", isRecent && "animate-pulse")} />;
  }
}
