import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { rpc, NpyPreview, NpzPreview } from "../../ipc";
import { formatBytes } from "../../lib/format";

type Props =
  | { kind: "npy"; repoId: string; path: string; rev: string | null }
  | { kind: "npz"; repoId: string; path: string; rev: string | null };

export function NumpyView(props: Props) {
  if (props.kind === "npy") return <NpyView {...props} />;
  return <NpzView {...props} />;
}

function NpyView({ repoId, path, rev }: { repoId: string; path: string; rev: string | null }) {
  const [data, setData] = useState<NpyPreview | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    setData(null);
    setErr(null);
    rpc
      .previewNpy(repoId, path, { rev: rev ?? undefined, signal: controller.signal })
      .then((d) => {
        if (!cancelled) setData(d);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [repoId, path, rev]);

  if (err) {
    return <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>;
  }
  if (!data) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Reading numpy header…
      </div>
    );
  }

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mb-4 text-sm font-semibold">NumPy array</div>
      <div className="grid max-w-xl grid-cols-2 gap-3">
        <KV label="Shape" value={`[${data.shape.join(" × ")}]`} />
        <KV label="Dtype" value={data.dtype} />
        <KV label="Order" value={data.order} />
        <KV label="Elements" value={Number(data.num_elements).toLocaleString()} />
      </div>
    </div>
  );
}

function NpzView({ repoId, path, rev }: { repoId: string; path: string; rev: string | null }) {
  const [data, setData] = useState<NpzPreview | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    setData(null);
    setErr(null);
    rpc
      .previewNpz(repoId, path, { rev: rev ?? undefined, signal: controller.signal })
      .then((d) => {
        if (!cancelled) setData(d);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [repoId, path, rev]);

  if (err) {
    return <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>;
  }
  if (!data) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Reading npz archive…
      </div>
    );
  }

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mb-4 text-sm font-semibold">
        NumPy archive · {data.members.length} array{data.members.length === 1 ? "" : "s"}
      </div>
      <table className="w-full max-w-3xl border-collapse font-mono text-[12px]">
        <thead>
          <tr>
            <th className="border-b border-border px-2 py-1 text-left text-muted-foreground">Name</th>
            <th className="border-b border-border px-2 py-1 text-left text-muted-foreground">Dtype</th>
            <th className="border-b border-border px-2 py-1 text-left text-muted-foreground">Shape</th>
            <th className="border-b border-border px-2 py-1 text-right text-muted-foreground">Size</th>
          </tr>
        </thead>
        <tbody>
          {data.members.map((m) => (
            <tr key={m.name}>
              <td className="border-b border-border/40 px-2 py-1">{m.name}</td>
              <td className="border-b border-border/40 px-2 py-1 text-primary">{m.dtype ?? "–"}</td>
              <td className="border-b border-border/40 px-2 py-1">
                {m.shape ? `[${m.shape.join(" × ")}]` : m.error ?? "–"}
              </td>
              <td className="border-b border-border/40 px-2 py-1 text-right text-muted-foreground">
                {m.nbytes != null ? formatBytes(m.nbytes) : "–"}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function KV({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded border border-border bg-card p-3">
      <div className="text-[10px] uppercase tracking-wider text-muted-foreground">{label}</div>
      <div className="mt-1 font-mono text-sm">{value}</div>
    </div>
  );
}
