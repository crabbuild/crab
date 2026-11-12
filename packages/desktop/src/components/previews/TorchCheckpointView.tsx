import { Button } from "@/components/ui/button";
import { ChevronDown, ChevronRight, Cpu, Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

export function TorchCheckpointView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<Awaited<ReturnType<typeof rpc.previewTorchCheckpoint>> | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [entriesOpen, setEntriesOpen] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    setData(null);
    setErr(null);
    rpc.previewTorchCheckpoint(repoId, path, { rev: rev ?? undefined, signal: controller.signal })
      .then(d => { if (!cancelled) setData(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; controller.abort(); };
  }, [repoId, path, rev]);

  if (err)
    return (
      <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
        {err}
      </div>
    );
  if (!data)
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Parsing checkpoint…
      </div>
    );

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mx-auto max-w-3xl space-y-4">
        <div className="flex items-center gap-3">
          <Cpu size={20} className="text-primary" />
          <div>
            <div className="text-lg font-semibold">{path.split("/").pop()}</div>
            <div className="text-[11px] text-muted-foreground">
              {formatBytes(data.file_size)} · {data.format}
            </div>
          </div>
        </div>

        <div className="grid grid-cols-2 gap-3 text-[12px]">
          <KV label="Format" value={data.format} />
          <KV label="File size" value={formatBytes(data.file_size)} />
          <KV label="Tensors" value={data.tensor_count.toLocaleString()} />
          <KV label="Optimizer state" value={data.has_optimizer_state ? "Yes" : "No"} />
        </div>

        {data.zip_entries.length > 0 && (
          <div className="rounded border border-border bg-card p-3">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="justify-start flex w-full items-center gap-2 text-left text-[10px] uppercase tracking-wider text-muted-foreground"
              onClick={() => setEntriesOpen(!entriesOpen)}
            >
              {entriesOpen ? (
                <ChevronDown size={12} />
              ) : (
                <ChevronRight size={12} />
              )}
              ZIP entries ({data.zip_entries.length})
            </Button>
            {entriesOpen && (
              <ul className="mt-2 max-h-64 space-y-1 overflow-auto">
                {data.zip_entries.map((entry, i) => (
                  <li key={i} className="flex items-center justify-between font-mono text-[11px]">
                    <span className="truncate text-foreground">
                      {entry.name}
                    </span>
                    <span className="ml-2 shrink-0 text-muted-foreground">
                      {formatBytes(entry.size)}
                    </span>
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
      <div className="mb-0.5 text-[10px] uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="text-sm">{value}</div>
    </div>
  );
}
