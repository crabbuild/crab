import { useMemo, useState } from "react";
import { ArrowRight, Package } from "lucide-react";
import { formatBytes } from "../lib/format";
import { DiffInlineCommit } from "./DiffInlineCommit";
import type { CommandContext } from "../lib/command-registry";
import type { LargeFileDiff } from "../ipc";

interface Props {
  data: LargeFileDiff;
  repoId?: string;
  onRefresh?: () => void;
}

export function LargeFileDiffView({ data, repoId, onRefresh }: Props) {
  const metrics = data.chunk_metrics;
  const context = useMemo<CommandContext | null>(() => repoId ? {
    repoId, currentBranch: "", upstream: null, ahead: 0, behind: 0,
    dirtyCount: data.status === "unchanged" ? 0 : 1, hasStagedChanges: false,
    activeOperation: null, forgeProvider: null,
  } : null, [repoId, data.status]);
  const oldSize = data.before_exists ? data.old_size : 0;
  const newSize = data.after_exists ? data.new_size : 0;
  const delta = oldSize == null || newSize == null ? null : newSize - oldSize;
  return (
    <div className="space-y-4 p-4" data-testid="large-file-diff">
      {context && repoId && <DiffInlineCommit repoId={repoId} filePath={data.path} context={context} onCommitted={onRefresh} />}
      <div className="rounded-lg border border-gx-border bg-gx-surface p-4">
        <div className="flex items-center gap-2 text-sm font-medium text-gx-text">
          <Package size={16} className="text-gx-accent" /> Large file diff
          <span className="chip text-[10px]">{data.status === "unchanged" ? "Content unchanged" : data.status}</span>
        </div>
        <div className="mt-3 grid grid-cols-3 gap-3 text-xs">
          <Metric label="Before" value={!data.before_exists ? "Absent" : oldSize == null ? "Unavailable" : formatBytes(oldSize)} />
          <Metric label="After" value={!data.after_exists ? "Absent" : newSize == null ? "Unavailable" : formatBytes(newSize)} />
          <Metric label="Size delta" value={delta == null ? "Unavailable" : `${delta > 0 ? "+" : delta < 0 ? "−" : ""}${formatBytes(Math.abs(delta))}`} />
        </div>
        <div className="mt-4 space-y-2 text-[11px] text-gx-muted">
          {(["before", "after"] as const).map((side) => {
            const identity = data[side];
            return identity && <div key={side} className="flex flex-wrap gap-x-2">
              <span className="capitalize">{side} · {identity.kind === "crab" ? "BLAKE3" : "LFS SHA-256"}</span>
              <code className="break-all select-text text-gx-text">{identity.hash}</code>
            </div>;
          })}
        </div>
        <p className="mt-3 text-xs text-gx-muted">{data.metrics_note}</p>
      </div>
      {metrics && <>
        <div className="rounded-lg border border-gx-border bg-gx-surface p-4">
          <div className="mb-3 flex items-center gap-2 text-xs text-gx-muted">
            {metrics.old_source} <ArrowRight size={12} /> {metrics.new_source}
          </div>
          <table className="w-full text-left text-xs">
            <thead className="text-gx-muted"><tr><th className="pb-2 font-normal">Ordered chunk content</th><th className="pb-2 font-normal">Chunks</th><th className="pb-2 font-normal">Bytes</th></tr></thead>
            <tbody className="text-gx-text">
              <tr><td className="py-1">Unchanged</td><td>{metrics.unchanged_chunks.toLocaleString()}</td><td>{formatBytes(metrics.unchanged_bytes)}</td></tr>
              <tr><td className="py-1">Removed</td><td>{metrics.removed_chunks.toLocaleString()}</td><td>{formatBytes(metrics.removed_bytes)}</td></tr>
              <tr><td className="py-1">Added</td><td>{metrics.added_chunks.toLocaleString()}</td><td>{formatBytes(metrics.added_bytes)}</td></tr>
            </tbody>
          </table>
          <p className="mt-3 text-xs text-gx-muted">{(metrics.reuse_ratio * 100).toFixed(1)}% ordered byte reuse. Before: {metrics.old_chunks.toLocaleString()} chunks; after: {metrics.new_chunks.toLocaleString()} chunks.</p>
        </div>
        <Ranges key={`old:${data.before?.hash}`} label="Changed ranges before" ranges={metrics.changed_byte_ranges_old} />
        <Ranges key={`new:${data.after?.hash}`} label="Changed ranges after" ranges={metrics.changed_byte_ranges_new} />
      </>}
    </div>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return <div><div className="text-gx-muted">{label}</div><div className="mt-1 font-medium text-gx-text">{value}</div></div>;
}

function Ranges({ label, ranges }: { label: string; ranges: Array<[number, number]> }) {
  const [page, setPage] = useState(0);
  const pageSize = 50;
  const start = page * pageSize;
  return <section aria-label={label} className="rounded-lg border border-gx-border bg-gx-surface p-4 text-xs">
    <h3 className="font-medium text-gx-text">{label} · {ranges.length.toLocaleString()}</h3>
    {ranges.length === 0 ? <p className="mt-2 text-gx-muted">No changed ranges.</p> : <>
      <table className="mt-3 w-full text-left font-mono text-[11px] text-gx-muted">
        <thead><tr><th className="pb-2 font-normal">Byte offset</th><th className="pb-2 font-normal">Byte length</th></tr></thead>
        <tbody>{ranges.slice(start, start + pageSize).map(([offset, length]) => <tr key={offset}><td className="py-0.5">{offset.toLocaleString()}</td><td>{length.toLocaleString()}</td></tr>)}</tbody>
      </table>
      {ranges.length > pageSize && <div className="mt-3 flex items-center gap-3">
        <button className="btn" disabled={page === 0} onClick={() => setPage(page - 1)}>Previous ranges</button>
        <span>{start + 1}–{Math.min(start + pageSize, ranges.length)} of {ranges.length.toLocaleString()}</span>
        <button className="btn" disabled={start + pageSize >= ranges.length} onClick={() => setPage(page + 1)}>Next ranges</button>
      </div>}
    </>}
  </section>;
}
