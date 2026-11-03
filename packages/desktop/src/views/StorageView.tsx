import { useCallback, useEffect, useState } from "react";
import { Database, RefreshCw } from "lucide-react";
import { rpc, StorageSummary } from "../ipc";
import { formatBytes } from "../lib/format";
import { OpenRepo } from "../state";

export function StorageView({ repo }: { repo: OpenRepo }) {
  const [summary, setSummary] = useState<StorageSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    setSummary(null);
    try { setSummary(await rpc.storageSummary(repo.repo_id)); }
    catch (error) { setError(error instanceof Error ? error.message : String(error)); }
    finally { setLoading(false); }
  }, [repo.repo_id]);
  useEffect(() => { void refresh(); }, [refresh]);

  const capsule = summary?.metadata.capsule;
  return (
    <section className="h-full overflow-auto p-6" aria-label="Repository storage">
      <div className="mb-5 flex items-center justify-between">
        <h2 className="flex items-center gap-2 text-sm font-semibold"><Database size={16} /> Repository storage</h2>
        <button type="button" className="btn text-xs" onClick={() => void refresh()} disabled={loading}>
          <RefreshCw size={12} className={loading ? "animate-spin" : ""} /> Refresh
        </button>
      </div>
      {loading && <p className="text-sm text-gx-muted" role="status">Reading storage metadata…</p>}
      {error && <p className="rounded border border-gx-error/40 bg-gx-error/10 p-3 text-sm text-gx-error" role="alert">{error}</p>}
      {summary && capsule && <div className="space-y-5">
        <div className="rounded border border-gx-border bg-gx-surface p-4">
          <h3 className="mb-3 text-xs font-semibold">Native v2 repository</h3>
          <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-sm">
            <dt className="text-gx-muted">Protocol</dt><dd>{summary.metadata.protocol}</dd>
            <dt className="text-gx-muted">Generation</dt><dd>{capsule.generation}</dd>
            <dt className="text-gx-muted">Visible refs</dt><dd>{capsule.visible_refs}</dd>
            <dt className="text-gx-muted">Visible capsules</dt><dd>{capsule.visible_capsules}</dd>
            <dt className="text-gx-muted">Checkpoint</dt><dd>{capsule.checkpoint_present ? "Present" : "Not present"}</dd>
            <dt className="text-gx-muted">Root digest</dt><dd className="break-all font-mono text-xs">{capsule.root_digest}</dd>
          </dl>
        </div>
        <div className="rounded border border-gx-border bg-gx-surface p-4">
          <h3 className="mb-3 text-xs font-semibold">Local disk usage</h3>
          <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-sm">
            {([
              ["Total", summary.usage.local_total_bytes], ["Git objects", summary.usage.git_dir_bytes],
              ["Content cache", summary.usage.cache_bytes], ["Staged data", summary.usage.staging_bytes],
              ["Hydrated files", summary.usage.hydrated_bytes], ["Pointers", summary.usage.pointer_bytes],
            ] as const).map(([label, bytes]) => <div key={label} className="contents"><dt className="text-gx-muted">{label}</dt><dd>{formatBytes(bytes)}</dd></div>)}
          </dl>
        </div>
      </div>}
    </section>
  );
}
