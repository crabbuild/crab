import { useEffect, useState } from "react";
import { Loader2, Shield } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

export function ProtobufView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<Awaited<ReturnType<typeof rpc.previewProtobuf>> | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    rpc.previewProtobuf(repoId, path, { rev: rev ?? undefined })
      .then(d => { if (!cancelled) setData(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>;
  if (!data) return <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground"><Loader2 size={14} className="animate-spin" /> Parsing protobuf descriptor…</div>;

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mx-auto max-w-4xl space-y-4">
        <div className="flex items-center gap-3">
          <Shield size={20} className="text-primary" />
          <div>
            <div className="text-lg font-semibold">{path.split("/").pop()}</div>
            <div className="text-[11px] text-muted-foreground">{formatBytes(data.file_size)} · {data.file_count} file{data.file_count === 1 ? "" : "s"}</div>
          </div>
        </div>

        {data.files.map((f, fi) => (
          <div key={fi} className="rounded border border-border bg-card p-4">
            <div className="mb-2 flex items-center gap-2">
              <span className="font-mono text-[12px] font-semibold text-foreground">{f.name}</span>
              {f.package && <span className="chip text-[9px]">{f.package}</span>}
              {f.syntax && <span className="chip text-[9px]">{f.syntax}</span>}
            </div>

            {f.messages.length > 0 && (
              <div className="mb-3">
                <div className="mb-1 text-[10px] uppercase tracking-wider text-muted-foreground">Messages ({f.messages.length})</div>
                {f.messages.map((m, mi) => (
                  <div key={mi} className="mb-2 rounded border border-border/60 bg-background p-2">
                    <div className="mb-1 font-mono text-[12px] font-semibold text-primary">{m.name}</div>
                    <table className="w-full text-[11px]">
                      <tbody>
                        {m.fields.map((field, ffi) => (
                          <tr key={ffi} className="border-b border-border/30 last:border-b-0">
                            <td className="py-0.5 pr-2 font-mono text-muted-foreground">{field.number}</td>
                            <td className="py-0.5 pr-2 font-mono text-foreground">{field.name}</td>
                            <td className="py-0.5 pr-2 font-mono text-muted-foreground">{field.type.replace("Type", "")}</td>
                            <td className="py-0.5 font-mono text-[10px] text-muted-foreground">{field.label.replace("Label", "")}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                ))}
              </div>
            )}

            {f.enums.length > 0 && (
              <div className="mb-3">
                <div className="mb-1 text-[10px] uppercase tracking-wider text-muted-foreground">Enums ({f.enums.length})</div>
                {f.enums.map((e, ei) => (
                  <div key={ei} className="mb-1 flex items-center gap-2 text-[11px]">
                    <span className="font-mono font-semibold text-warning">{e.name}</span>
                    <span className="text-muted-foreground">= {e.values.join(" | ")}</span>
                  </div>
                ))}
              </div>
            )}

            {f.services.length > 0 && (
              <div>
                <div className="mb-1 text-[10px] uppercase tracking-wider text-muted-foreground">Services ({f.services.length})</div>
                {f.services.map((s, si) => (
                  <div key={si} className="mb-2 rounded border border-border/60 bg-background p-2">
                    <div className="mb-1 font-mono text-[12px] font-semibold text-success">{s.name}</div>
                    {s.methods.map((m, mi) => (
                      <div key={mi} className="flex gap-2 font-mono text-[11px]">
                        <span className="text-foreground">{m.name}</span>
                        <span className="text-muted-foreground">({m.input_type.split(".").pop()}) → {m.output_type.split(".").pop()}</span>
                      </div>
                    ))}
                  </div>
                ))}
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
