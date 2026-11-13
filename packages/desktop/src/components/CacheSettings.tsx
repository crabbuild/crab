import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Loader2, RotateCcw, Trash2 } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { rpc, type CacheCleanResult, type CacheStats } from "../ipc";
import { formatBytes } from "../lib/format";

export function CacheSettings({ repoId, label }: { repoId?: string; label?: string }) {
  const [stats, setStats] = useState<CacheStats | null>(null);
  const [result, setResult] = useState<CacheCleanResult | null>(null);
  const [pending, setPending] = useState<string | null>("Inspecting cache…");
  const [error, setError] = useState<string | null>(null);
  const [unlimited, setUnlimited] = useState(true);
  const [limitMiB, setLimitMiB] = useState("1024");
  const [budgetResult, setBudgetResult] = useState<string | null>(null);
  const generation = useRef(0);
  const modeId = useId();

  function receiveStats(report: CacheStats) {
    setStats(report);
    if (report.catalog.state === "unavailable") return;
    const limit = report.catalog.state === "readable" ? report.catalog.stats.retention_budget_bytes : null;
    setUnlimited(limit === null);
    setLimitMiB(limit === null ? "1024" : String(limit / 1_048_576));
  }

  useEffect(() => {
    const scope = ++generation.current;
    setStats(null);
    setResult(null);
    setBudgetResult(null);
    setError(null);
    setPending("Inspecting cache…");
    rpc.cacheStats(repoId).then((report) => {
      if (scope === generation.current) receiveStats(report);
    }).catch((error) => {
      if (scope === generation.current) setError(error instanceof Error ? error.message : String(error));
    }).finally(() => { if (scope === generation.current) setPending(null); });
    return () => { generation.current++; };
  }, [repoId]);

  async function inspectOrClean(dryRun?: boolean) {
    const scope = generation.current;
    setPending(dryRun === undefined ? "Inspecting cache…" : dryRun ? "Previewing cleanup…" : "Cleaning cache…");
    setError(null);
    setResult(null);
    setBudgetResult(null);
    try {
      if (dryRun !== undefined) {
        const report = await rpc.cacheClean(repoId, dryRun);
        if (scope !== generation.current) return;
        setResult(report);
      }
      const report = await rpc.cacheStats(repoId);
      if (scope === generation.current) receiveStats(report);
    } catch (error) {
      if (scope === generation.current) {
        setStats(null);
        setError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      if (scope === generation.current) setPending(null);
    }
  }

  async function saveBudget(event: React.FormEvent) {
    event.preventDefault();
    const maxBytes = unlimited ? null : Number(limitMiB) * 1_048_576;
    if (maxBytes !== null && (!limitMiB.trim() || !Number.isSafeInteger(maxBytes) || maxBytes < 0)) {
      setError("Enter a nonnegative limit in MiB that resolves to whole bytes.");
      return;
    }
    const scope = generation.current;
    setPending("Saving budget…");
    setError(null);
    setResult(null);
    setBudgetResult(null);
    try {
      const report = await rpc.cacheSetBudget(repoId, maxBytes);
      if (scope !== generation.current) return;
      receiveStats(report.usage);
      setBudgetResult(report.maintenance_pending
        ? "Budget saved. Cache maintenance is already running; refresh usage when it finishes."
        : `Budget saved. Reclaimed ${formatBytes(report.reclaimed_bytes)} of reusable payloads.`);
    } catch (error) {
      if (scope !== generation.current) return;
      setError(`Budget update did not finish: ${error instanceof Error ? error.message : String(error)}`);
      // Saving can succeed before maintenance fails. Refresh the actual policy
      // rather than presenting the previous limit as if the action rolled back.
      try {
        const report = await rpc.cacheStats(repoId);
        if (scope === generation.current) receiveStats(report);
      } catch {
        if (scope === generation.current) setStats(null);
      }
    } finally {
      if (scope === generation.current) setPending(null);
    }
  }

  const rootBudget = stats?.catalog.state === "readable" ? stats.catalog.stats.retention_budget_bytes : null;
  const rootOverBudget = !stats?.budget_available ? null : rootBudget === null ? false
    : stats.observed.allocated_bytes > rootBudget ? true : stats.scan_complete ? false : null;

  return (
    <section aria-label="Cache usage" className="flex flex-col gap-3 text-xs">
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
              <dt className="text-muted-foreground">Shared retention budget</dt>
              <dd>
                {!stats.budget_available ? "Unknown" : rootBudget === null ? "Unlimited" : formatBytes(rootBudget)}
              </dd>
              <dt className="text-muted-foreground">Budget status</dt>
              <dd>
                {rootOverBudget === null ? "Unknown" : rootOverBudget ? "Over budget" : "Within budget"}
              </dd>
            </dl>
          )}
          {stats.root_state === "missing" && (
            <p className="text-muted-foreground">Cache has not been created.</p>
          )}
          {stats.budget_available && stats.budget_bytes !== null && stats.budget_bytes !== rootBudget && <p className="text-[11px] text-muted-foreground">CLI operations also apply their configured {formatBytes(stats.budget_bytes)} ceiling.</p>}
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
      <form onSubmit={event => void saveBudget(event)} className="flex flex-col gap-2 border-t border-border pt-3">
        <div className="flex flex-wrap items-center gap-2">
          <label className="font-medium" htmlFor={modeId}>Cache retention</label>
          <Select value={!stats?.budget_available ? "unknown" : unlimited ? "unlimited" : "limited"} disabled={pending !== null || !stats?.budget_available} onValueChange={value => setUnlimited(value === "unlimited")}>
            <SelectTrigger id={modeId} className="w-44"><SelectValue /></SelectTrigger>
            <SelectContent><SelectGroup>
              {!stats?.budget_available && <SelectItem value="unknown" disabled>Unavailable</SelectItem>}
              <SelectItem value="unlimited">Unlimited</SelectItem>
              <SelectItem value="limited">Limit disk retention</SelectItem>
            </SelectGroup></SelectContent>
          </Select>
          {!unlimited && <div className="flex items-center gap-2">
            <Input aria-label="Cache limit in MiB" type="number" min="0" step="any" className="w-28" value={limitMiB} disabled={pending !== null} onChange={event => setLimitMiB(event.target.value)} />
            <span className="text-muted-foreground">MiB</span>
          </div>}
          <Button type="submit" disabled={pending !== null || !stats?.budget_available}>Save budget</Button>
        </div>
        <p className="text-[11px] text-muted-foreground">Shared by repositories and storage snapshots on this host. Zero stops new retention. Active reads and retained data can keep usage above the limit; repository limits may be lower.</p>
        {budgetResult && <p role="status">{budgetResult}</p>}
      </form>
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
