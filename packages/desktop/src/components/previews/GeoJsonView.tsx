import { useEffect, useRef, useState } from "react";
import { Globe, Loader2 } from "lucide-react";
import { rpc } from "../../ipc";

interface Props { repoId: string; path: string; rev: string | null; }

/**
 * GeoJSON map viewer. Renders features on a simple SVG map projection
 * (no external map tile dependency). For a full Leaflet/Mapbox
 * integration we'd need a heavier dep; this gives a useful preview
 * without adding 200KB to the bundle.
 */
export function GeoJsonView({ repoId, path, rev }: Props) {
  const [geojson, setGeojson] = useState<unknown>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    rpc
      .readText(repoId, path, { rev: rev ?? undefined, max_bytes: 10 * 1024 * 1024 })
      .then((res) => {
        if (cancelled) return;
        if (res.truncated || !res.text) { setErr("File too large to preview"); return; }
        try { setGeojson(JSON.parse(res.text)); } catch { setErr("Invalid GeoJSON"); }
      })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>;
  if (!geojson) return <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground"><Loader2 size={14} className="animate-spin" /> Loading GeoJSON…</div>;

  return (
    <div className="flex h-full flex-col bg-background">
      <div className="flex shrink-0 items-center gap-2 border-b border-border bg-card px-3 py-1.5 text-[11px] text-muted-foreground">
        <Globe size={12} className="text-primary" /> GeoJSON · {path.split("/").pop()}
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-4">
        <GeoSvg data={geojson} />
      </div>
    </div>
  );
}

/** Minimal SVG renderer for GeoJSON geometries. Projects lat/lng to
 *  a simple equirectangular projection bounded by the data's extent. */
function GeoSvg({ data }: { data: unknown }) {
  const svgRef = useRef<SVGSVGElement>(null);
  const features = extractFeatures(data);
  if (features.length === 0) return <div className="text-xs text-muted-foreground">No renderable features found.</div>;

  // Compute bounding box.
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const coords of allCoords(features)) {
    if (coords[0] < minX) minX = coords[0];
    if (coords[0] > maxX) maxX = coords[0];
    if (coords[1] < minY) minY = coords[1];
    if (coords[1] > maxY) maxY = coords[1];
  }
  const pad = 0.05;
  const dx = (maxX - minX) || 1;
  const dy = (maxY - minY) || 1;
  minX -= dx * pad; maxX += dx * pad;
  minY -= dy * pad; maxY += dy * pad;
  const W = 800, H = 600;
  const sx = W / (maxX - minX);
  const sy = H / (maxY - minY);
  const proj = (lng: number, lat: number): [number, number] => [
    (lng - minX) * sx,
    H - (lat - minY) * sy, // flip Y
  ];

  const paths: string[] = [];
  const points: Array<[number, number]> = [];
  for (const f of features) {
    const geom = f.geometry;
    if (!geom) continue;
    if (geom.type === "Point") { const p = proj(geom.coordinates[0], geom.coordinates[1]); points.push(p); }
    else if (geom.type === "MultiPoint") { for (const c of geom.coordinates) { const p = proj(c[0], c[1]); points.push(p); } }
    else if (geom.type === "LineString") { paths.push(ringToPath(geom.coordinates, proj, false)); }
    else if (geom.type === "MultiLineString") { for (const ring of geom.coordinates) paths.push(ringToPath(ring, proj, false)); }
    else if (geom.type === "Polygon") { for (const ring of geom.coordinates) paths.push(ringToPath(ring, proj, true)); }
    else if (geom.type === "MultiPolygon") { for (const poly of geom.coordinates) for (const ring of poly) paths.push(ringToPath(ring, proj, true)); }
  }

  return (
    <svg ref={svgRef} viewBox={`0 0 ${W} ${H}`} className="w-full rounded border border-border bg-card" style={{ maxHeight: 500 }}>
      {paths.map((d, i) => <path key={i} d={d} fill="rgb(var(--primary) / 0.15)" stroke="rgb(var(--primary))" strokeWidth={1.5} />)}
      {points.map((p, i) => <circle key={`p${i}`} cx={p[0]} cy={p[1]} r={3} fill="rgb(var(--primary))" />)}
    </svg>
  );
}

function ringToPath(coords: number[][], proj: (lng: number, lat: number) => [number, number], close: boolean): string {
  const pts = coords.map(c => proj(c[0], c[1]));
  return pts.map((p, i) => `${i === 0 ? "M" : "L"}${p[0].toFixed(1)},${p[1].toFixed(1)}`).join("") + (close ? "Z" : "");
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function extractFeatures(data: any): any[] {
  if (!data) return [];
  if (data.type === "FeatureCollection") return data.features ?? [];
  if (data.type === "Feature") return [data];
  if (data.type && data.coordinates) return [{ type: "Feature", geometry: data, properties: {} }];
  return [];
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function* allCoords(features: any[]): Generator<number[]> {
  for (const f of features) {
    const geom = f.geometry;
    if (!geom) continue;
    if (geom.type === "Point") yield geom.coordinates;
    else if (geom.type === "MultiPoint") for (const c of geom.coordinates) yield c;
    else if (geom.type === "LineString") for (const c of geom.coordinates) yield c;
    else if (geom.type === "MultiLineString") for (const ring of geom.coordinates) for (const c of ring) yield c;
    else if (geom.type === "Polygon") for (const ring of geom.coordinates) for (const c of ring) yield c;
    else if (geom.type === "MultiPolygon") for (const poly of geom.coordinates) for (const ring of poly) for (const c of ring) yield c;
  }
}
