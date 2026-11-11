import { NativeSelect } from "@/components/ui/native-select";
import { Button } from "@/components/ui/button";
import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  Cloud,
  Database,
  HardDrive,
  Loader2,
  Trash2,
  Upload,
  Layers,
  XCircle,
} from "lucide-react";
import { OpenRepo } from "../state";
import { rpc, StagingStats, StagedFileEntry } from "../ipc";
import { formatBytes } from "../lib/format";

interface Props {
  repo: OpenRepo;
  /** Trigger to refresh (increment after add/push completes). */
  refreshKey?: number;
  /** Called when user clicks "Push to Remote". */
  onPush?: (remote?: string) => void;
  /** Called when user clicks "Track Files" to open the add panel. */
  onAdd?: () => void;
}

/** Infer a file type icon/label from the file path or size. */
function inferFileType(entry: StagedFileEntry): { label: string; color: string } {
  // When a file path is available, use the extension for a richer label.
  if (entry.file_path) {
    const ext = entry.file_path.split(".").pop()?.toLowerCase() ?? "";
    const mediaExts = new Set(["mp4", "mov", "avi", "mkv", "webm"]);
    const modelExts = new Set(["safetensors", "onnx", "pt", "pth", "bin", "gguf"]);
    const archiveExts = new Set(["zip", "tar", "gz", "tgz", "zst", "xz", "bz2"]);
    const imageExts = new Set(["png", "jpg", "jpeg", "tiff", "bmp", "psd"]);
    const dataExts = new Set(["parquet", "arrow", "csv", "db", "sqlite"]);

    if (modelExts.has(ext)) return { label: "ML", color: "text-thinking" };
    if (mediaExts.has(ext)) return { label: "VID", color: "text-info" };
    if (imageExts.has(ext)) return { label: "IMG", color: "text-info" };
    if (archiveExts.has(ext)) return { label: "ARC", color: "text-warning" };
    if (dataExts.has(ext)) return { label: "DAT", color: "text-info" };
  }

  // Fallback: categorize by size.
  const bytes = entry.total_bytes;
  if (bytes > 1024 * 1024 * 1024) return { label: "XL", color: "text-destructive" };
  if (bytes > 100 * 1024 * 1024) return { label: "LG", color: "text-warning" };
  if (bytes > 10 * 1024 * 1024) return { label: "MD", color: "text-warning" };
  return { label: "SM", color: "text-success" };
}

/**
 * Collapsible "Staging Area" section showing the actual crab staging
 * area state — files with pending/committed chunks awaiting push.
 */
export function CrabStagedSection({ repo, refreshKey, onPush, onAdd }: Props) {
  const [expanded, setExpanded] = useState(true);
  const [stats, setStats] = useState<StagingStats | null>(null);
  const [loading, setLoading] = useState(false);
  const [cleaning, setCleaning] = useState(false);
  const [removingFiles, setRemovingFiles] = useState<Set<string>>(new Set());
  const [remotes, setRemotes] = useState<Array<{ name: string; url: string }>>([]);
  const [selectedRemote, setSelectedRemote] = useState<string | undefined>(undefined);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const result = await rpc.stagingStats(repo.repo_id);
      setStats(result);
    } catch {
      // Staging stats unavailable — not a crab repo or no staging area.
      setStats(null);
    } finally {
      setLoading(false);
    }
  }, [repo.repo_id]);

  useEffect(() => {
    refresh();
  }, [refresh, refreshKey]);

  // Fetch crab remotes for the remote selector.
  useEffect(() => {
    rpc.gitRepoInfo(repo.repo_id).then((info) => {
      const crabRemotes = info.remotes
        .filter((r) => r.url.startsWith("crab://"))
        .map((r) => ({ name: r.name, url: r.url }));
      // Deduplicate by name (git remote -v shows fetch + push).
      const unique = Array.from(new Map(crabRemotes.map((r) => [r.name, r])).values());
      setRemotes(unique);
    }).catch(() => { /* ignore */ });
  }, [repo.repo_id]);

  const handleClean = useCallback(async () => {
    setCleaning(true);
    try {
      await rpc.stagingClean(repo.repo_id, true);
      await refresh();
    } catch {
      // Ignore errors — clean may not be available
    } finally {
      setCleaning(false);
    }
  }, [repo.repo_id, refresh]);

  const handleRemoveFile = useCallback(async (fileHash: string) => {
    setRemovingFiles((prev) => new Set(prev).add(fileHash));
    try {
      // crab reset accepts file paths or hashes — pass the hash
      // prefixed with "hash:" so the CLI can resolve it.
      await rpc.crabResetFiles(repo.repo_id, [`hash:${fileHash}`]);
      await refresh();
    } catch {
      // Silently fail — the file may already be removed
    } finally {
      setRemovingFiles((prev) => {
        const next = new Set(prev);
        next.delete(fileHash);
        return next;
      });
    }
  }, [repo.repo_id, refresh]);

  // Don't render if no staging data or not a crab repo.
  if (!stats) {
    return null;
  }

  const fileCount = stats.file_count ?? 0;
  const chunkCount = stats.chunk_count ?? 0;
  const totalBytes = stats.total_staged_bytes ?? 0;
  const hasLiveData = chunkCount > 0 || totalBytes > 0;
  const files = stats.files ?? [];
  const filesWithChunks = files.filter((f) => f.total_chunks > 0);

  // Show the section if there are registered files (even without active chunks)
  if (fileCount === 0 && files.length === 0) {
    return null;
  }

  return (
    <div className="border-b border-border">
      {/* Header */}
      <button
        type="button"
        className="flex w-full items-center gap-2 px-3 py-2 text-left text-[11px] hover:bg-accent"
        onClick={() => setExpanded(!expanded)}
      >
        {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
        <Database size={11} className="text-primary" />
        <span className="font-semibold text-foreground">Staging Area</span>
        <span className="text-muted-foreground">
          {fileCount} file{fileCount !== 1 ? "s" : ""}
          {chunkCount > 0 && ` · ${chunkCount.toLocaleString()} chunks`}
          {totalBytes > 0 && ` · ${formatBytes(totalBytes)}`}
        </span>
        {loading && <Loader2 size={10} className="animate-spin text-muted-foreground" />}
      </button>

      {expanded && (
        <div className="px-3 pb-2">
          {/* Summary stats row */}
          {hasLiveData && (
            <div className="mb-2 flex items-center gap-3 rounded bg-card px-2 py-1 text-[10px] text-muted-foreground">
              <span className="flex items-center gap-1">
                <HardDrive size={9} />
                {formatBytes(stats.live_bytes ?? 0)} live
              </span>
              {(stats.dead_bytes ?? 0) > 0 && (
                <span className="flex items-center gap-1 text-warning">
                  <Trash2 size={9} />
                  {formatBytes(stats.dead_bytes ?? 0)} dead
                  ({((stats.dead_ratio ?? 0) * 100).toFixed(0)}%)
                </span>
              )}
              <span className="flex items-center gap-1">
                <Layers size={9} />
                {stats.segments_sealed ?? 0} sealed seg
                {(stats.segments_sealed ?? 0) !== 1 ? "s" : ""}
              </span>
            </div>
          )}

          {/* Per-file list */}
          {files.length > 0 && (
            <ul className="mb-2 max-h-48 space-y-0.5 overflow-auto">
              {files.map((f) => {
                const fileType = inferFileType(f);
                const hasChunks = f.total_chunks > 0;
                return (
                  <li
                    key={f.file_hash}
                    className="group flex items-center gap-2 rounded px-2 py-1 text-xs hover:bg-accent"
                  >
                    {/* File type badge */}
                    <span
                      className={clsx(
                        "shrink-0 w-5 text-center text-[9px] font-bold",
                        fileType.color,
                      )}
                    >
                      {fileType.label}
                    </span>

                    {/* File name or hash (truncated) */}
                    <span
                      className="min-w-0 flex-1 truncate font-mono text-foreground"
                      title={f.file_path ?? f.file_hash}
                    >
                      {f.file_path ?? `${f.file_hash.slice(0, 16)}…`}
                    </span>

                    {/* Size */}
                    <span className="shrink-0 text-muted-foreground">
                      {formatBytes(f.total_bytes)}
                    </span>

                    {/* Chunk info */}
                    {hasChunks && (
                      <span className="shrink-0 rounded bg-card px-1 text-[10px] text-muted-foreground">
                        {f.total_chunks} chunks
                      </span>
                    )}

                    {/* Pending indicator */}
                    {f.pending_chunks > 0 && (
                      <span className="shrink-0 rounded bg-warning/10 px-1 text-[10px] text-warning">
                        {f.pending_chunks} pending
                      </span>
                    )}

                    {/* Segment count */}
                    {f.segments > 0 && (
                      <span className="shrink-0 rounded bg-card px-1 text-[10px] text-muted-foreground">
                        {f.segments} seg{f.segments !== 1 ? "s" : ""}
                      </span>
                    )}

                    {/* Remove from staging button */}
                    <button
                      type="button"
                      className="shrink-0 rounded p-0.5 text-muted-foreground transition hover:bg-destructive/10 hover:text-destructive"
                      onClick={() => handleRemoveFile(f.file_hash)}
                      disabled={removingFiles.has(f.file_hash)}
                      title="Remove from staging area"
                    >
                      {removingFiles.has(f.file_hash) ? (
                        <Loader2 size={11} className="animate-spin" />
                      ) : (
                        <XCircle size={11} />
                      )}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}

          {/* Empty state */}
          {files.length === 0 && fileCount === 0 && (
            <p className="mb-2 text-xs text-muted-foreground">
              No files staged. Use <code className="text-foreground">crab add</code> to stage files.
            </p>
          )}

          {/* Action buttons */}
          <div className="flex flex-col gap-2">
            {/* Remote selector — shown when multiple crab remotes exist */}
            {remotes.length > 1 && filesWithChunks.length > 0 && (
              <div className="flex items-center gap-2 text-xs">
                <label htmlFor="staging-push-remote" className="shrink-0 text-muted-foreground">Remote:</label>
                <NativeSelect
                  id="staging-push-remote"
                  className="flex-1 rounded border border-border bg-card px-2 py-1 text-xs text-foreground"
                  value={selectedRemote ?? ""}
                  onChange={(e) => setSelectedRemote(e.target.value || undefined)}
                >
                  <option value="">default</option>
                  {remotes.map((r) => (
                    <option key={r.name} value={r.name}>
                      {r.name} — {r.url}
                    </option>
                  ))}
                </NativeSelect>
              </div>
            )}
            <div className="flex items-center gap-2">
              {filesWithChunks.length > 0 && (
                <Button variant="default"
                  type="button"
                  className="text-xs"
                  onClick={() => onPush?.(selectedRemote)}
                >
                  <Upload size={11} /> Push{selectedRemote ? ` to ${selectedRemote}` : " to Remote"}
                </Button>
              )}
              <Button variant="outline"
                type="button"
                className="text-xs"
                onClick={onAdd}
              >
                <Cloud size={11} /> Track Files
              </Button>
              {(hasLiveData || (stats.dead_bytes ?? 0) > 0) && (
                <Button variant="outline"
                  type="button"
                  className="text-xs"
                  onClick={handleClean}
                  disabled={cleaning}
                  title="Clean staging area: remove stale data and reclaim disk space"
                >
                  {cleaning ? (
                    <Loader2 size={11} className="animate-spin" />
                  ) : (
                    <Trash2 size={11} />
                  )}
                  Clean
                </Button>
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
