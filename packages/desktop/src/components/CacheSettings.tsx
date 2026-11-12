import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Loader2, RotateCcw, Trash2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
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
        <p className="text-muted-foreground">
          {repoId?.startsWith("ssh://") ? `SSH host · ${label}` : label ? `This computer · ${label}` : "This computer"}
        </p>
        <Button variant="outline" type="button" className="text-xs" disabled={pending !== null} onClick={() => void inspectOrClean()}>
          {pending ? (
            <Loader2 size={12} className="animate-spin" />
          ) : (
            <RotateCcw size={12} />
          )}
          {pending ?? "Refresh usage"}
        </Button>
      </div>
      {error && (
        <p role="alert" className="break-words text-destructive">
          {error}
        </p>
      )}
      {stats && (
        <>
          <p className="break-all font-mono text-[11px] text-muted-foreground">
            {stats.root}
          </p>
          {stats.root_state === "unavailable" ? (
            <p className="text-warning">
              Cache unavailable. Reported bytes are observed lower bounds.
            </p>
          ) : (
            <dl className="grid grid-cols-2 gap-x-4 gap-y-1 tabular-nums sm:grid-cols-4">
              <dt className="text-muted-foreground">Allocated disk</dt>
              <dd>
                {formatBytes(stats.observed.allocated_bytes)}
                {!stats.scan_complete && " or more"}
              </dd>
              <dt className="text-muted-foreground">Logical bytes</dt>
              <dd>
                {formatBytes(stats.observed.logical_bytes)}
                {!stats.scan_complete && " or more"}
              </dd>
              <dt className="text-muted-foreground">Configured budget</dt>
              <dd>
                {stats.budget_bytes === null ? "Unlimited" : formatBytes(stats.budget_bytes)}
              </dd>
              <dt className="text-muted-foreground">Budget status</dt>
              <dd>
                {stats.over_budget === null ? "Unknown" : stats.over_budget ? "Over budget" : "Within budget"}
              </dd>
            </dl>
          )}
          {stats.root_state === "missing" && (
            <p className="text-muted-foreground">Cache has not been created.</p>
          )}
          <p className="text-[11px] text-muted-foreground">
            Live usage includes retained state and directories. Shared
            filesystem blocks may be counted more than once.
          </p>
          <Collapsible>
            <CollapsibleTrigger className="cursor-pointer text-muted-foreground">
              Usage by content type
            </CollapsibleTrigger>
            <CollapsibleContent>
              <div className="mt-2 overflow-x-auto">
                <table className="w-full text-left text-[11px] tabular-nums">
                  <thead className="text-muted-foreground">
                    <tr>
                      <th className="py-1 font-medium">Type</th>
                      <th className="font-medium">Files</th>
                      <th className="font-medium">Logical</th>
                      <th className="font-medium">Allocated</th>
                      <th className="font-medium">Inspection</th>
                    </tr>
                  </thead>
                  <tbody>
                    {Object.entries(stats.families).map(([name, usage]) => (
                      <tr key={name} className="border-t border-border">
                        <td className="py-1 pr-3">{name}</td>
                        <td>
                          {usage.files}
                          {!usage.complete && "+"}
                        </td>
                        <td>
                          {formatBytes(usage.logical_bytes)}
                          {!usage.complete && "+"}
                        </td>
                        <td>
                          {formatBytes(usage.allocated_bytes)}
                          {!usage.complete && "+"}
                        </td>
                        <td>
                          {!usage.complete ? "Partial; lower bounds" : usage.issues > 0 ? "Unavailable" : "Complete"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </CollapsibleContent>
          </Collapsible>
          {stats.issues.length > 0 && (
            <div role="alert" className="space-y-1 text-warning">
              {stats.issues.map((issue, index) => (
                <p key={index} className="break-words">
                  {issue.family ?? "Cache"}: {issue.path} — {issue.error}
                </p>
              ))}
              {stats.omitted_issues > 0 && (
                <p>{stats.omitted_issues} additional issues omitted.</p>
              )}
            </div>
          )}
        </>
      )}
      <div className="flex flex-wrap items-center gap-2 border-t border-border pt-3">
        <Button variant="outline" type="button" className="text-xs" disabled={pending !== null} onClick={() => void inspectOrClean(true)}>
          Preview cleanup
        </Button>
        <Button variant="outline" type="button" className="text-xs" disabled={pending !== null} onClick={() => void inspectOrClean(false)}>
          <Trash2 size={12} /> Clean cache
        </Button>
        <p className="text-[11px] text-muted-foreground">
          Keeps retained, unknown and busy data. Reusable payloads may need
          downloading again.
        </p>
      </div>
      {result && (
        <p role="status" className="text-foreground">
          {result.dry_run ? "Would remove" : "Removed"} {result.files_removed}{" "}
          payloads, {formatBytes(result.bytes_reclaimed)}. Retained{" "}
          {result.retained_entries} entries or subtrees; {result.busy_entries}{" "}
          busy; {result.unsafe_entries} unsafe.
        </p>
      )}
    </section>
  );
}
