import { useEffect, useState } from "react";
import { Database, Folder, Loader2, Table2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

export function Hdf5View({ repoId, path, rev }: Props) {
  const [data, setData] = useState<Awaited<ReturnType<typeof rpc.previewHdf5>> | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    rpc.previewHdf5(repoId, path, { rev: rev ?? undefined })
      .then(d => { if (!cancelled) setData(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>;
  if (!data) return <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={14} className="animate-spin" /> Reading HDF5 header…</div>;

  return (
    <div className="h-full overflow-auto bg-gx-bg-sub p-6">
      <div className="mx-auto max-w-3xl space-y-4">
        <div className="flex items-center gap-3">
          <Database size={20} className="text-gx-accent" />
          <div>
            <div className="text-lg font-semibold">{path.split("/").pop()}</div>
            <div className="text-[11px] text-gx-muted">{formatBytes(data.file_size)} · Superblock v{data.superblock_version}</div>
          </div>
        </div>

        {!data.h5dump_available && (
          <div className="rounded border border-gx-warn/40 bg-gx-warn/10 p-3 text-[11px] text-gx-warn">
            Install <code className="font-mono">h5dump</code> (via <code>brew install hdf5</code>) for full dataset listing.
            Currently showing basic file metadata only.
          </div>
        )}

        {data.items.length > 0 && (
          <div className="rounded border border-gx-border bg-gx-surface p-3">
            <div className="mb-2 text-[10px] uppercase tracking-wider text-gx-muted">Contents ({data.items.length})</div>
            <ul className="space-y-1">
              {data.items.map((item, i) => (
                <li key={i} className="flex items-center gap-2 font-mono text-[12px]">
                  {item.kind === "group" ? <Folder size={12} className="text-gx-muted" /> : <Table2 size={12} className="text-gx-accent" />}
                  <span className="text-gx-text">{item.name}</span>
                  <span className="text-[10px] text-gx-muted">{item.kind}</span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </div>
  );
}
