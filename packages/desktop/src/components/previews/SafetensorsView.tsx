import { useEffect, useMemo, useState } from "react";
import { Brain, Copy, Loader2, Search } from "lucide-react";
import { rpc, SafetensorsPreview, SafetensorsTensor } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

// Safetensors preview: HF model weights at a glance. Shows total
// parameter count (the "is this a 7B?" answer), dtype histogram, and
// the full tensor list with per-tensor dtype/shape/bytes. The whole
// thing reads only the JSON header, not the tensor data, so it
// scales to 100 GB files.
export function SafetensorsView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<SafetensorsPreview | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    setData(null);
    setErr(null);
    rpc
      .previewSafetensors(repoId, path, { rev: rev ?? undefined, signal: controller.signal })
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

  const filtered = useMemo(() => {
    if (!data) return [] as SafetensorsTensor[];
    const f = filter.trim().toLowerCase();
    if (!f) return data.tensors;
    return data.tensors.filter((t) => t.name.toLowerCase().includes(f));
  }, [data, filter]);

  if (err) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
        {err}
      </div>
    );
  }
  if (!data) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Reading safetensors header…
      </div>
    );
  }

  const totalParams = BigInt(data.total_params);
  const totalBytes = Number(data.total_bytes);

  return (
    <div className="flex h-full flex-col">
      {/* Summary hero */}
      <div className="shrink-0 border-b border-gx-border bg-gx-surface p-4">
        <div className="mb-3 flex items-center gap-2 text-sm text-gx-muted">
          <Brain size={14} className="text-gx-accent" />
          Safetensors · ML weights
        </div>
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          <Stat label="Parameters" value={humanParams(totalParams)} />
          <Stat label="On disk" value={formatBytes(totalBytes)} />
          <Stat label="Tensors" value={data.tensor_count.toLocaleString()} />
          <Stat label="Header" value={formatBytes(data.header_bytes)} />
        </div>
        {Object.keys(data.dtype_histogram).length > 0 && (
          <div className="mt-3 flex flex-wrap gap-1.5">
            {Object.entries(data.dtype_histogram).map(([dtype, n]) => (
              <span key={dtype} className="chip font-mono">
                {dtype} · {n}
              </span>
            ))}
          </div>
        )}
        {data.metadata && Object.keys(data.metadata).length > 0 && (
          <details className="mt-3 rounded border border-gx-border bg-gx-bg-sub p-2 text-[11px]">
            <summary className="cursor-pointer text-gx-muted">Embedded metadata</summary>
            <pre className="mt-2 overflow-auto">{JSON.stringify(data.metadata, null, 2)}</pre>
          </details>
        )}
      </div>

      {/* Tensor list */}
      <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <Search size={12} />
        <input
          className="flex-1 rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-[11.5px] focus-visible:outline-none focus:border-gx-accent"
          placeholder="Filter tensors (e.g. attn, layer.0, lm_head)…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <span>
          {filtered.length} / {data.tensor_count}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-auto bg-gx-bg-sub font-mono text-[12px]">
        <table className="w-full border-collapse">
          <thead className="sticky top-0 bg-gx-surface">
            <tr>
              <th className="border-b border-gx-border px-2 py-1 text-left text-gx-muted">Name</th>
              <th className="border-b border-gx-border px-2 py-1 text-left text-gx-muted">Dtype</th>
              <th className="border-b border-gx-border px-2 py-1 text-left text-gx-muted">Shape</th>
              <th className="border-b border-gx-border px-2 py-1 text-right text-gx-muted">Params</th>
              <th className="border-b border-gx-border px-2 py-1 text-right text-gx-muted">Size</th>
              <th className="border-b border-gx-border px-2 py-1"></th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((t) => {
              const numel = t.shape.length
                ? t.shape.reduce((a, b) => a * b, 1)
                : 1;
              return (
                <tr key={t.name} className="odd:bg-gx-surface">
                  <td className="border-b border-gx-border/40 px-2 py-1 align-top text-gx-text">{t.name}</td>
                  <td className="border-b border-gx-border/40 px-2 py-1 align-top text-gx-accent">{t.dtype}</td>
                  <td className="border-b border-gx-border/40 px-2 py-1 align-top text-gx-text">
                    [{t.shape.join(" × ")}]
                  </td>
                  <td className="border-b border-gx-border/40 px-2 py-1 text-right align-top text-gx-text">
                    {numel.toLocaleString()}
                  </td>
                  <td className="border-b border-gx-border/40 px-2 py-1 text-right align-top text-gx-muted">
                    {formatBytes(t.nbytes)}
                  </td>
                  <td className="border-b border-gx-border/40 px-2 py-1 align-top">
                    <button
                      type="button"
                      className="text-gx-muted hover:text-gx-accent"
                      onClick={() => void navigator.clipboard.writeText(t.name)}
                      title="Copy tensor name"
                    >
                      <Copy size={11} />
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded border border-gx-border bg-gx-bg-sub p-3">
      <div className="text-[10px] uppercase tracking-wider text-gx-muted">{label}</div>
      <div className="mt-1 font-mono text-lg text-gx-text">{value}</div>
    </div>
  );
}

// "7B", "175B", "13M" — what ML folks actually say out loud.
function humanParams(n: bigint): string {
  const abs = n < 0n ? -n : n;
  if (abs < 1_000n) return abs.toString();
  const units: Array<[bigint, string]> = [
    [1_000_000_000_000n, "T"],
    [1_000_000_000n, "B"],
    [1_000_000n, "M"],
    [1_000n, "K"],
  ];
  for (const [mag, sym] of units) {
    if (abs >= mag) {
      // Keep one decimal for numbers < 100, none otherwise — mirrors
      // how people write "7.2B" but "175B".
      const whole = Number(abs / mag);
      const rem = Number(((abs % mag) * 10n) / mag);
      return whole < 100 && rem > 0 ? `${whole}.${rem}${sym}` : `${whole}${sym}`;
    }
  }
  return abs.toString();
}
