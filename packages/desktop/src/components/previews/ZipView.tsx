import { useEffect, useMemo, useState } from "react";
import { Loader2, Search } from "lucide-react";
import { rpc, ZipPreview } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

export function ZipView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<ZipPreview | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    setData(null);
    setErr(null);
    rpc
      .previewZip(repoId, path, { rev: rev ?? undefined, signal: controller.signal })
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
    if (!data) return [];
    const f = filter.trim().toLowerCase();
    if (!f) return data.members;
    return data.members.filter((m) => m.name.toLowerCase().includes(f));
  }, [data, filter]);

  if (err) {
    return <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>;
  }
  if (!data) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Reading archive…
      </div>
    );
  }

  const totalComp = Number(data.total_compressed);
  const totalUncomp = Number(data.total_uncompressed);
  const ratio = totalUncomp > 0 ? totalComp / totalUncomp : 1;

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center gap-4 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <span>
          <strong className="text-gx-text">{data.member_count}</strong> entries
        </span>
        <span>
          {formatBytes(totalComp)} → {formatBytes(totalUncomp)} ({(ratio * 100).toFixed(1)}%)
        </span>
        <div className="ml-auto flex items-center gap-1">
          <Search size={11} />
          <input
            className="w-48 rounded border border-gx-border bg-gx-bg-sub px-2 py-0.5 text-[11px] focus-visible:outline-none focus:border-gx-accent"
            placeholder="Filter"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto bg-gx-bg-sub">
        <table className="w-full border-collapse font-mono text-[12px]">
          <thead className="sticky top-0 bg-gx-surface">
            <tr>
              <th className="border-b border-gx-border px-2 py-1 text-left text-gx-muted">Path</th>
              <th className="border-b border-gx-border px-2 py-1 text-right text-gx-muted">Size</th>
              <th className="border-b border-gx-border px-2 py-1 text-right text-gx-muted">Compressed</th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((m) => (
              <tr key={m.name} className="odd:bg-gx-surface">
                <td className="border-b border-gx-border/40 px-2 py-1 text-gx-text">{m.name}</td>
                <td className="border-b border-gx-border/40 px-2 py-1 text-right text-gx-muted">
                  {m.is_dir ? "–" : formatBytes(m.size)}
                </td>
                <td className="border-b border-gx-border/40 px-2 py-1 text-right text-gx-muted">
                  {m.is_dir ? "–" : formatBytes(m.compressed)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
