import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Cpu, Loader2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

export function TorchCheckpointView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<Awaited<ReturnType<typeof rpc.previewTorchCheckpoint>> | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [entriesOpen, setEntriesOpen] = useState(false);

  useEffect(() => {
    let cancelled = false;
    rpc.previewTorchCheckpoint(repoId, path, { rev: rev ?? undefined })
      .then(d => { if (!cancelled) setData(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>;
  if (!data) return <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={14} className="animate-spin" /> Parsing checkpoint…</div>;

  return (
    <div className="h-full overflow-auto bg-gx-bg-sub p-6">
      <div className="mx-auto max-w-3xl space-y-4">
        <div className="flex items-center gap-3">
          <Cpu size={20} className="text-gx-accent" />
          <div>
            <div className="text-lg font-semibold">{path.split("/").pop()}</div>
            <div className="text-[11px] text-gx-muted">{formatBytes(data.file_size)} · {data.format}</div>
          </div>
        </div>

        <div className="grid grid-cols-2 gap-3 text-[12px]">
          <KV label="Format" value={data.format} />
          <KV label="File size" value={formatBytes(data.file_size)} />
          <KV label="Tensors" value={data.tensor_count.toLocaleString()} />
          <KV label="Optimizer state" value={data.has_optimizer_state ? "Yes" : "No"} />
        </div>

        {data.zip_entries.length > 0 && (
          <div className="rounded border border-gx-border bg-gx-surface p-3">
            <button
              type="button"
              className="flex w-full items-center gap-2 text-left text-[10px] uppercase tracking-wider text-gx-muted"
              onClick={() => setEntriesOpen(!entriesOpen)}
            >
              {entriesOpen ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
              ZIP entries ({data.zip_entries.length})
            </button>
            {entriesOpen && (
              <ul className="mt-2 max-h-64 space-y-1 overflow-auto">
                {data.zip_entries.map((entry, i) => (
                  <li key={i} className="flex items-center justify-between font-mono text-[11px]">
                    <span className="truncate text-gx-text">{entry.name}</span>
                    <span className="ml-2 shrink-0 text-gx-muted">{formatBytes(entry.size)}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

function KV({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="mb-0.5 text-[10px] uppercase tracking-wider text-gx-muted">{label}</div>
      <div className="text-sm">{value}</div>
    </div>
  );
}
