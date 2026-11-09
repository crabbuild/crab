import { useEffect, useRef, useState } from "react";
import { Loader2, RotateCcw, Trash2 } from "lucide-react";
import { rpc, type CacheCleanResult, type CacheStats } from "../ipc";
import { formatBytes } from "../lib/format";

export function CacheSettings({ repoId, label }: { repoId?: string; label?: string }) {
  const [stats, setStats] = useState<CacheStats | null>(null);
  const [result, setResult] = useState<CacheCleanResult | null>(null);
  const [pending, setPending] = useState<string | null>("Inspecting cache…");
  const [error, setError] = useState<string | null>(null);
  const active = useRef(true);

  useEffect(() => {
    let cancelled = false;
    active.current = true;
    rpc.cacheStats(repoId).then((report) => {
      if (!cancelled) setStats(report);
    }).catch((error) => {
      if (!cancelled) setError(error instanceof Error ? error.message : String(error));
    }).finally(() => { if (!cancelled) setPending(null); });
    return () => { cancelled = true; active.current = false; };
  }, [repoId]);

  async function inspectOrClean(dryRun?: boolean) {
    setPending(dryRun === undefined ? "Inspecting cache…" : dryRun ? "Previewing cleanup…" : "Cleaning cache…");
    setError(null);
    setResult(null);
    try {
      if (dryRun !== undefined) {
        const report = await rpc.cacheClean(repoId, dryRun);
        if (!active.current) return;
        setResult(report);
      }
      const report = await rpc.cacheStats(repoId);
      if (active.current) setStats(report);
    } catch (error) {
      if (active.current) {
        setStats(null);
        setError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      if (active.current) setPending(null);
    }
  }

  return (
    <section aria-label="Cache usage" className="space-y-3 text-xs">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-gx-muted">
          {repoId?.startsWith("ssh://") ? `SSH host · ${label}` : label ? `This computer · ${label}` : "This computer"}
        </p>
        <button type="button" className="btn text-xs" disabled={pending !== null} onClick={() => void inspectOrClean()}>
          {pending ? <Loader2 size={12} className="animate-spin" /> : <RotateCcw size={12} />}
          {pending ?? "Refresh usage"}
        </button>
      </div>
      {error && <p role="alert" className="break-words text-gx-error">{error}</p>}
      {stats && <>
        <p className="break-all font-mono text-[11px] text-gx-muted">{stats.root}</p>
        {stats.root_state === "unavailable" ? (
          <p className="text-gx-warning">Cache unavailable. Reported bytes are observed lower bounds.</p>
        ) : (
          <dl className="grid grid-cols-2 gap-x-4 gap-y-1 tabular-nums sm:grid-cols-4">
            <dt className="text-gx-muted">Allocated disk</dt><dd>{formatBytes(stats.observed.allocated_bytes)}{!stats.scan_complete && " or more"}</dd>
            <dt className="text-gx-muted">Logical bytes</dt><dd>{formatBytes(stats.observed.logical_bytes)}{!stats.scan_complete && " or more"}</dd>
            <dt className="text-gx-muted">Configured budget</dt><dd>{stats.budget_bytes === null ? "Unlimited" : formatBytes(stats.budget_bytes)}</dd>
            <dt className="text-gx-muted">Budget status</dt><dd>{stats.over_budget === null ? "Unknown" : stats.over_budget ? "Over budget" : "Within budget"}</dd>
          </dl>
        )}
        {stats.root_state === "missing" && <p className="text-gx-muted">Cache has not been created.</p>}
        <p className="text-[11px] text-gx-muted">Live usage includes retained state and directories. Shared filesystem blocks may be counted more than once.</p>
        <details>
          <summary className="cursor-pointer text-gx-muted">Usage by content type</summary>
          <div className="mt-2 overflow-x-auto">
            <table className="w-full text-left text-[11px] tabular-nums">
              <thead className="text-gx-muted"><tr><th className="py-1 font-medium">Type</th><th className="font-medium">Files</th><th className="font-medium">Logical</th><th className="font-medium">Allocated</th><th className="font-medium">Inspection</th></tr></thead>
              <tbody>{Object.entries(stats.families).map(([name, usage]) => (
                <tr key={name} className="border-t border-gx-border">
                  <td className="py-1 pr-3">{name}</td><td>{usage.files}{!usage.complete && "+"}</td><td>{formatBytes(usage.logical_bytes)}{!usage.complete && "+"}</td><td>{formatBytes(usage.allocated_bytes)}{!usage.complete && "+"}</td><td>{!usage.complete ? "Partial; lower bounds" : usage.issues > 0 ? "Unavailable" : "Complete"}</td>
                </tr>
              ))}</tbody>
            </table>
          </div>
        </details>
        {stats.issues.length > 0 && <div role="alert" className="space-y-1 text-gx-warning">
          {stats.issues.map((issue, index) => <p key={index} className="break-words">{issue.family ?? "Cache"}: {issue.path} — {issue.error}</p>)}
          {stats.omitted_issues > 0 && <p>{stats.omitted_issues} additional issues omitted.</p>}
        </div>}
      </>}
      <div className="flex flex-wrap items-center gap-2 border-t border-gx-border pt-3">
        <button type="button" className="btn text-xs" disabled={pending !== null} onClick={() => void inspectOrClean(true)}>Preview cleanup</button>
        <button type="button" className="btn text-xs" disabled={pending !== null} onClick={() => void inspectOrClean(false)}><Trash2 size={12} /> Clean cache</button>
        <p className="text-[11px] text-gx-muted">Keeps retained, unknown and busy data. Reusable payloads may need downloading again.</p>
      </div>
      {result && <p role="status" className="text-gx-text">
        {result.dry_run ? "Would remove" : "Removed"} {result.files_removed} payloads, {formatBytes(result.bytes_reclaimed)}. Retained {result.retained_entries} entries or subtrees; {result.busy_entries} busy; {result.unsafe_entries} unsafe.
      </p>}
    </section>
  );
}
