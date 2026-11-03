import { useMemo } from "react";
import { ArrowRight, Minus, Package, Plus } from "lucide-react";
import { formatBytes } from "../lib/format";
import { DiffInlineCommit } from "./DiffInlineCommit";
import type { CommandContext } from "../lib/command-registry";

interface CrabDiffFile {
  path: string;
  status: string;
  old_size?: number;
  new_size?: number;
  delta_bytes?: number;
  chunks_added?: number;
  chunks_removed?: number;
  chunks_unchanged?: number;
  chunks_modified?: number;
  byte_ranges?: Array<{ offset: number; length: number; kind: string }>;
  dedup_ratio?: number;
  old_file_hash?: string;
  new_file_hash?: string;
}

interface CrabDiffSummary {
  files_changed?: number;
  total_delta_bytes?: number;
  total_chunks_added?: number;
  total_chunks_removed?: number;
}

interface CrabDiffData {
  files?: CrabDiffFile[];
  summary?: CrabDiffSummary;
}

/**
 * Normalize a file entry from the raw `crab diff --json` output into
 * the shape expected by the rendering code.
 *
 * The CLI output wraps each file in `{ report: { ... } }` and uses
 * field names like `added_segments` / `removed_segments` / `unchanged_segments`.
 * Byte ranges come as `[offset, length]` tuples in `changed_byte_ranges`.
 * The synthetic fallback from the agent handler already uses the flat shape.
 */
function normalizeFile(raw: Record<string, unknown>): CrabDiffFile {
  // Unwrap the `report` wrapper if present (CLI envelope format).
  const entry: Record<string, unknown> = (raw.report && typeof raw.report === "object")
    ? (raw.report as Record<string, unknown>)
    : raw;

  // Map segment-based field names to chunk-based names expected by the UI.
  const added = (entry.chunks_added as number | undefined)
    ?? (entry.added_segments as number | undefined);
  const removed = (entry.chunks_removed as number | undefined)
    ?? (entry.removed_segments as number | undefined);
  const unchanged = (entry.chunks_unchanged as number | undefined)
    ?? (entry.unchanged_segments as number | undefined);

  // Byte ranges: CLI produces `changed_byte_ranges: [[offset, len], ...]`
  // while synthetic produces `byte_ranges: [{offset, length, kind}, ...]`.
  let byteRanges: CrabDiffFile["byte_ranges"] | undefined;
  if (Array.isArray(entry.byte_ranges)) {
    byteRanges = entry.byte_ranges as CrabDiffFile["byte_ranges"];
  } else if (Array.isArray(entry.changed_byte_ranges)) {
    byteRanges = (entry.changed_byte_ranges as Array<[number, number]>)
      .filter((r) => Array.isArray(r) && r.length >= 2)
      .map(([offset, length]) => ({ offset, length, kind: "modified" }));
  }

  return {
    path: entry.path as string,
    status: entry.status as string,
    old_size: entry.old_size as number | undefined,
    new_size: entry.new_size as number | undefined,
    delta_bytes: entry.delta_bytes as number | undefined,
    chunks_added: added,
    chunks_removed: removed,
    chunks_unchanged: unchanged,
    chunks_modified: entry.chunks_modified as number | undefined,
    byte_ranges: byteRanges,
    dedup_ratio: entry.dedup_ratio as number | undefined,
    old_file_hash: entry.old_file_hash as string | undefined,
    new_file_hash: entry.new_file_hash as string | undefined,
  };
}

interface Props {
  /** Raw JSON string from `crab diff --json` output. */
  data: string;
  /** File path for single-file context. */
  path: string;
  /** Repository ID — when provided, enables the inline commit toolbar. */
  repoId?: string;
  /** Called after a successful inline commit to refresh the parent view. */
  onRefresh?: () => void;
}

/**
 * Renders a large file diff view for crab-tracked files.
 *
 * Unlike text-based unified diffs, crab operates at the segment level
 * (content-defined chunking via gearhash). This component shows which
 * segments were added, removed, or modified between two versions of a
 * file, along with size deltas, file reconstruction info, and
 * byte-range annotations.
 */
export function CrabDiffView({ data, path, repoId, onRefresh }: Props) {
  const parsed = useMemo((): CrabDiffData | null => {
    try {
      const envelope = JSON.parse(data);
      // crab --json wraps in { "schema": "...", "version": "...", "data": {...} }
      const payload = envelope?.data ?? envelope;
      // Normalize file entries from CLI format to the UI's expected shape.
      if (payload?.files && Array.isArray(payload.files)) {
        payload.files = payload.files.map((f: Record<string, unknown>) => normalizeFile(f));
      }
      return payload;
    } catch {
      return null;
    }
  }, [data]);

  // Build a minimal CommandContext for the inline commit toolbar.
  // Placed before early returns to satisfy React's rules of hooks.
  const commandContext = useMemo<CommandContext | null>(
    () =>
      repoId
        ? {
            repoId,
            currentBranch: "",
            upstream: null,
            ahead: 0,
            behind: 0,
            dirtyCount: 1,
            hasStagedChanges: false,
            activeOperation: null,
            forgeProvider: null,
          }
        : null,
    [repoId],
  );

  if (!parsed) {
    return (
      <div className="flex items-center justify-center gap-2 py-6 text-sm text-gx-muted">
        <Package size={14} /> Unable to parse crab diff output
      </div>
    );
  }

  // Find the file entry matching our path (or use the first one).
  const file = parsed.files?.find((f) => f.path === path) ?? parsed.files?.[0];
  const summary = parsed.summary;

  if (!file && !summary) {
    return (
      <div className="flex items-center justify-center gap-2 py-6 text-sm text-gx-muted">
        <Package size={14} /> No changes detected by crab
      </div>
    );
  }

  return (
    <div className="p-4">
      {/* Inline commit toolbar (shown when repoId is available) */}
      {commandContext && (
        <DiffInlineCommit
          repoId={repoId!}
          filePath={path}
          context={commandContext}
          onCommitted={onRefresh}
        />
      )}
      {/* File header */}
      {file && (
        <div className="rounded-lg border border-gx-border bg-gx-surface p-4">
          <div className="flex items-start gap-3">
            <div className="shrink-0 rounded-lg bg-gx-accent/10 p-2">
              <Package size={18} className="text-gx-accent" />
            </div>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium text-gx-text">Large file diff</span>
                <StatusBadge status={file.status} />
              </div>
              <div className="mt-1 truncate font-mono text-[11px] text-gx-muted">{file.path}</div>
            </div>
          </div>

          {/* Size comparison */}
          <div className="mt-4 grid grid-cols-3 gap-3 text-center text-[11px]">
            {file.old_size != null && (
              <div className="rounded border border-gx-border bg-gx-bg-sub px-3 py-2">
                <div className="text-sm font-semibold text-gx-text">{formatBytes(file.old_size)}</div>
                <div className="text-gx-muted">Previous</div>
              </div>
            )}
            {file.new_size != null && (
              <div className="rounded border border-gx-border bg-gx-bg-sub px-3 py-2">
                <div className="text-sm font-semibold text-gx-text">{formatBytes(file.new_size)}</div>
                <div className="text-gx-muted">Current</div>
              </div>
            )}
            {file.old_size != null && file.new_size != null && (
              <div className="rounded border border-gx-border bg-gx-bg-sub px-3 py-2">
                <div className={`text-sm font-semibold ${(file.new_size - file.old_size) > 0 ? "text-gx-success" : (file.new_size - file.old_size) < 0 ? "text-gx-error" : "text-gx-muted"}`}>
                  {(file.new_size - file.old_size) > 0 ? "+" : ""}{formatBytes(Math.abs(file.new_size - file.old_size))}
                </div>
                <div className="text-gx-muted">Delta</div>
              </div>
            )}
            {file.old_size == null && file.new_size != null && file.delta_bytes != null && (
              <div className="rounded border border-gx-border bg-gx-bg-sub px-3 py-2">
                <div className={`text-sm font-semibold ${file.delta_bytes > 0 ? "text-gx-success" : file.delta_bytes < 0 ? "text-gx-error" : "text-gx-muted"}`}>
                  +{formatBytes(file.delta_bytes)}
                </div>
                <div className="text-gx-muted">Delta</div>
              </div>
            )}
          </div>

          {/* File reconstruction info */}
          {(file.old_file_hash || file.new_file_hash || file.dedup_ratio != null) && (
            <div className="mt-4">
              <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-gx-muted">
                File Reconstruction
              </div>
              <div className="rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-[11px]">
                {file.new_file_hash && (
                  <div className="flex items-center gap-2">
                    <span className="text-gx-muted">Hash:</span>
                    <code className="font-mono text-gx-text">{file.new_file_hash.slice(0, 16)}…</code>
                  </div>
                )}
                {file.old_file_hash && file.status !== "added" && (
                  <div className="mt-1 flex items-center gap-2">
                    <span className="text-gx-muted">Previous:</span>
                    <code className="font-mono text-gx-text">{file.old_file_hash.slice(0, 16)}…</code>
                  </div>
                )}
                {file.dedup_ratio != null && (
                  <div className="mt-1 flex items-center gap-2">
                    <span className="text-gx-muted">Dedup ratio:</span>
                    <span className="font-mono text-gx-text">{(file.dedup_ratio * 100).toFixed(1)}%</span>
                    <span className="text-gx-muted">shared between versions</span>
                  </div>
                )}
              </div>
            </div>
          )}

          {/* Segment stats */}
          {(file.chunks_added != null || file.chunks_removed != null || file.chunks_unchanged != null || file.chunks_modified != null) && (
            <div className="mt-4">
              <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-gx-muted">
                Segment Changes
              </div>
              <div className="flex flex-wrap gap-2">
                {file.chunks_added != null && file.chunks_added > 0 && (
                  <ChunkBadge count={file.chunks_added} label="added" tone="success" />
                )}
                {file.chunks_removed != null && file.chunks_removed > 0 && (
                  <ChunkBadge count={file.chunks_removed} label="removed" tone="error" />
                )}
                {file.chunks_modified != null && file.chunks_modified > 0 && (
                  <ChunkBadge count={file.chunks_modified} label="modified" tone="warn" />
                )}
                {file.chunks_unchanged != null && file.chunks_unchanged > 0 && (
                  <ChunkBadge count={file.chunks_unchanged} label="unchanged" tone="muted" />
                )}
              </div>

              {/* Visual segment bar */}
              <ChunkBar
                added={file.chunks_added ?? 0}
                removed={file.chunks_removed ?? 0}
                modified={file.chunks_modified ?? 0}
                unchanged={file.chunks_unchanged ?? 0}
              />
            </div>
          )}

          {/* Byte ranges (if available) */}
          {file.byte_ranges && file.byte_ranges.length > 0 && (
            <div className="mt-4">
              <div className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-gx-muted">
                Changed Regions
              </div>
              <div className="max-h-40 overflow-auto rounded border border-gx-border bg-gx-bg-sub">
                {file.byte_ranges.slice(0, 50).map((range, i) => (
                  <div key={i} className="flex items-center gap-3 border-b border-gx-border/30 px-3 py-1 text-[11px] last:border-b-0">
                    <span className={`shrink-0 ${range.kind === "added" ? "text-gx-success" : range.kind === "removed" ? "text-gx-error" : "text-gx-warn"}`}>
                      {range.kind === "added" ? <Plus size={10} /> : range.kind === "removed" ? <Minus size={10} /> : <ArrowRight size={10} />}
                    </span>
                    <span className="font-mono text-gx-muted">
                      offset {range.offset.toLocaleString()}
                    </span>
                    <span className="font-mono text-gx-text">
                      {formatBytes(range.length)}
                    </span>
                    <span className={`text-[10px] ${range.kind === "added" ? "text-gx-success" : range.kind === "removed" ? "text-gx-error" : "text-gx-warn"}`}>
                      {range.kind}
                    </span>
                  </div>
                ))}
                {file.byte_ranges.length > 50 && (
                  <div className="px-3 py-1.5 text-[10px] text-gx-muted">
                    … {file.byte_ranges.length - 50} more regions
                  </div>
                )}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function StatusBadge({ status }: { status: string }) {
  const tone = status === "added" ? "bg-gx-success/10 text-gx-success"
    : status === "deleted" ? "bg-gx-error/10 text-gx-error"
    : "bg-gx-accent/10 text-gx-accent";
  return (
    <span className={`rounded px-1.5 py-0.5 text-[10px] font-medium ${tone}`}>
      {status}
    </span>
  );
}

function ChunkBadge({ count, label, tone }: { count: number; label: string; tone: "success" | "error" | "warn" | "muted" }) {
  const cls = tone === "success" ? "border-gx-success/30 text-gx-success"
    : tone === "error" ? "border-gx-error/30 text-gx-error"
    : tone === "warn" ? "border-gx-warn/30 text-gx-warn"
    : "border-gx-border text-gx-muted";
  return (
    <span className={`inline-flex items-center gap-1 rounded border px-2 py-0.5 text-[11px] ${cls}`}>
      <span className="font-semibold">{count}</span> {label}
    </span>
  );
}

function ChunkBar({ added, removed, modified, unchanged }: { added: number; removed: number; modified: number; unchanged: number }) {
  const total = added + removed + modified + unchanged;
  if (total === 0) return null;

  const pctAdded = (added / total) * 100;
  const pctRemoved = (removed / total) * 100;
  const pctModified = (modified / total) * 100;
  const pctUnchanged = (unchanged / total) * 100;

  return (
    <div className="mt-2 flex h-2 w-full overflow-hidden rounded-full bg-gx-bg-sub" title={`${total} total chunks`}>
      {pctAdded > 0 && <div className="bg-gx-success" style={{ width: `${pctAdded}%` }} />}
      {pctModified > 0 && <div className="bg-gx-warn" style={{ width: `${pctModified}%` }} />}
      {pctRemoved > 0 && <div className="bg-gx-error" style={{ width: `${pctRemoved}%` }} />}
      {pctUnchanged > 0 && <div className="bg-gx-muted/20" style={{ width: `${pctUnchanged}%` }} />}
    </div>
  );
}
