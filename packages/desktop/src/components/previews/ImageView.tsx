import { Button } from "@/components/ui/button";
import { useEffect, useState } from "react";
import { Maximize2, Minimize2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
  entryKind: "file" | "pointer" | "lfs" | "dir" | "submodule";
}

// Chromium handles all the image decoding for us; we just feed it a
// data URL. For SVGs we pass the utf-8 text through directly so the
// user can see crisp vector output.
export function ImageView({ repoId, path, rev }: Props) {
  const [src, setSrc] = useState<string | null>(null);
  const [size, setSize] = useState<number | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [fit, setFit] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setSrc(null);
    setSize(null);
    setErr(null);
    rpc
      .readBytes(repoId, path, { rev: rev ?? undefined, max_bytes: 32 * 1024 * 1024 })
      .then((r) => {
        if (cancelled) return;
        if (r.truncated || !r.bytes_b64) {
          setErr(r.reason ?? "image too large to preview");
          return;
        }
        const mime = mimeFor(path);
        setSrc(`data:${mime};base64,${r.bytes_b64}`);
        setSize(r.size);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [repoId, path, rev]);

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center justify-between border-b border-border bg-card px-3 py-1.5 text-[11px] text-muted-foreground">
        <span>Image · {size != null ? formatBytes(size) : "…"}</span>
        <Button variant="outline"
          type="button"
          className="text-xs"
          onClick={() => setFit((f) => !f)}
          title={fit ? "Show at actual size" : "Fit to window"}
        >
          {fit ? <Maximize2 size={12} /> : <Minimize2 size={12} />}
          {fit ? "Actual size" : "Fit"}
        </Button>
      </div>
      <div className="relative flex min-h-0 flex-1 items-center justify-center overflow-auto bg-background p-4">
        {err && (
          <div className="rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {err}
          </div>
        )}
        {src && (
          <img
            src={src}
            alt={path}
            className={fit ? "max-h-full max-w-full object-contain" : "max-w-none"}
            style={{ imageRendering: "auto" }}
          />
        )}
      </div>
    </div>
  );
}

function mimeFor(path: string): string {
  const ext = path.slice(path.lastIndexOf(".") + 1).toLowerCase();
  const map: Record<string, string> = {
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    gif: "image/gif",
    webp: "image/webp",
    avif: "image/avif",
    bmp: "image/bmp",
    ico: "image/x-icon",
    svg: "image/svg+xml",
  };
  return map[ext] ?? "application/octet-stream";
}
