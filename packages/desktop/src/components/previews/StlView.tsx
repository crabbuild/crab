import { useEffect, useMemo, useState } from "react";
import { Box, Loader2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

/**
 * STL 3D model viewer. Renders a wireframe orthographic projection
 * of the model's triangles as SVG. No Three.js dependency.
 */
export function StlView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<Awaited<ReturnType<typeof rpc.previewStl>> | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    rpc.previewStl(repoId, path, { rev: rev ?? undefined })
      .then(d => { if (!cancelled) setData(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>;
  if (!data) return <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground"><Loader2 size={14} className="animate-spin" /> Parsing STL model…</div>;

  return (
    <div className="flex h-full flex-col bg-background">
      <div className="flex shrink-0 items-center gap-3 border-b border-border bg-card px-3 py-1.5 text-[11px] text-muted-foreground">
        <Box size={12} className="text-primary" />
        <span>{path.split("/").pop()}</span>
        <span>{formatBytes(data.file_size)}</span>
        <span>{data.triangle_count.toLocaleString()} triangles</span>
        {data.is_ascii && <span className="chip text-[9px]">ASCII</span>}
      </div>
      <div className="flex min-h-0 flex-1 items-center justify-center p-4">
        {data.preview_vertices.length > 0 ? (
          <WireframeSvg vertices={data.preview_vertices} bounds={data.bounds} />
        ) : (
          <div className="text-sm text-muted-foreground">No vertex data available for preview.</div>
        )}
      </div>
    </div>
  );
}

function WireframeSvg({ vertices, bounds }: {
  vertices: Array<[number, number, number]>;
  bounds: { min: [number, number, number]; max: [number, number, number] };
}) {
  const W = 600, H = 500;

  const lines = useMemo(() => {
    const [minX, minY] = bounds.min;
    const [maxX, maxY] = bounds.max;
    const dx = maxX - minX || 1;
    const dy = maxY - minY || 1;
    const scale = Math.min(W / dx, H / dy) * 0.85;
    const offX = (W - dx * scale) / 2;
    const offY = (H - dy * scale) / 2;

    const proj = (v: [number, number, number]): [number, number] => [
      (v[0] - minX) * scale + offX,
      H - ((v[1] - minY) * scale + offY), // flip Y
    ];

    // Draw triangle edges (every 3 vertices form a triangle).
    const paths: string[] = [];
    for (let i = 0; i + 2 < vertices.length; i += 3) {
      const p0 = proj(vertices[i]);
      const p1 = proj(vertices[i + 1]);
      const p2 = proj(vertices[i + 2]);
      paths.push(`M${p0[0].toFixed(1)},${p0[1].toFixed(1)}L${p1[0].toFixed(1)},${p1[1].toFixed(1)}L${p2[0].toFixed(1)},${p2[1].toFixed(1)}Z`);
    }
    return paths;
  }, [vertices, bounds]);

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="max-h-full max-w-full rounded border border-border bg-card" style={{ maxHeight: 450 }}>
      {lines.map((d, i) => (
        <path key={i} d={d} fill="none" stroke="rgb(var(--primary))" strokeWidth={0.5} opacity={0.6} />
      ))}
    </svg>
  );
}
