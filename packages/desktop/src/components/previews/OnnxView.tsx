import { useEffect, useState } from "react";
import { Brain, Loader2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

export function OnnxView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<Awaited<ReturnType<typeof rpc.previewOnnx>> | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    rpc.previewOnnx(repoId, path, { rev: rev ?? undefined })
      .then(d => { if (!cancelled) setData(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>;
  if (!data) return <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={14} className="animate-spin" /> Parsing ONNX model…</div>;

  return (
    <div className="h-full overflow-auto bg-gx-bg-sub p-6">
      <div className="mx-auto max-w-3xl space-y-4">
        <div className="flex items-center gap-3">
          <Brain size={20} className="text-gx-accent" />
          <div>
            <div className="text-lg font-semibold">{data.graph_name || path.split("/").pop()}</div>
            <div className="text-[11px] text-gx-muted">{formatBytes(data.file_size)} · IR v{data.ir_version}</div>
          </div>
        </div>

        <div className="grid grid-cols-2 gap-3 text-[12px]">
          <KV label="Producer" value={`${data.producer_name} ${data.producer_version}`.trim() || "—"} />
          <KV label="Domain" value={data.domain || "—"} />
          <KV label="Model version" value={String(data.model_version)} />
          <KV label="Nodes" value={data.node_count.toLocaleString()} />
        </div>

        {data.doc_string && (
          <div className="rounded border border-gx-border bg-gx-surface p-3 text-[12px] text-gx-muted">{data.doc_string}</div>
        )}

        <div className="rounded border border-gx-border bg-gx-surface p-3">
          <div className="mb-2 text-[10px] uppercase tracking-wider text-gx-muted">Opsets</div>
          <div className="flex flex-wrap gap-2">
            {data.opsets.map((o, i) => (
              <span key={i} className="chip font-mono text-[10px]">{o.domain || "ai.onnx"} v{o.version}</span>
            ))}
          </div>
        </div>

        <div className="grid grid-cols-2 gap-4">
          <div className="rounded border border-gx-border bg-gx-surface p-3">
            <div className="mb-2 text-[10px] uppercase tracking-wider text-gx-success">Inputs ({data.inputs.length})</div>
            <ul className="space-y-1 font-mono text-[11px]">
              {data.inputs.map(n => <li key={n} className="truncate">{n}</li>)}
            </ul>
          </div>
          <div className="rounded border border-gx-border bg-gx-surface p-3">
            <div className="mb-2 text-[10px] uppercase tracking-wider text-gx-accent">Outputs ({data.outputs.length})</div>
            <ul className="space-y-1 font-mono text-[11px]">
              {data.outputs.map(n => <li key={n} className="truncate">{n}</li>)}
            </ul>
          </div>
        </div>
      </div>
    </div>
  );
}

function KV({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded border border-gx-border bg-gx-surface p-2">
      <div className="text-[9px] uppercase tracking-wider text-gx-muted">{label}</div>
      <div className="mt-0.5 font-mono text-[12px] text-gx-text">{value}</div>
    </div>
  );
}
