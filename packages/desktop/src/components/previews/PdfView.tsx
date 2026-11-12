import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

// PDF preview via Chromium's built-in renderer. We fetch bytes
// through the agent (so the file resolves the same way as any other
// preview), turn them into a blob URL, and feed that into an
// `<iframe>`. Electron's Chromium ships with PDF.js, so this just
// works under `file://`.
export function PdfView({ repoId, path, rev }: Props) {
  const [src, setSrc] = useState<string | null>(null);
  const [size, setSize] = useState<number | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let url: string | null = null;
    setSrc(null);
    setSize(null);
    setErr(null);
    rpc
      .readBytes(repoId, path, { rev: rev ?? undefined, max_bytes: 50 * 1024 * 1024 })
      .then((r) => {
        if (cancelled) return;
        if (r.truncated || !r.bytes_b64) {
          setErr(r.reason ?? "PDF too large to preview");
          return;
        }
        // Convert base64 → Uint8Array → Blob → object URL. Doing this
        // in small chunks avoids the 1 MB limit of atob under some
        // Chromium builds.
        const bin = atob(r.bytes_b64);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        const blob = new Blob([bytes], { type: "application/pdf" });
        url = URL.createObjectURL(blob);
        setSrc(url);
        setSize(r.size);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [repoId, path, rev]);

  if (err) {
    return <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>;
  }
  if (!src) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Loading PDF…
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center gap-2 border-b border-border bg-card px-3 py-1.5 text-[11px] text-muted-foreground">
        <span>PDF · {size != null ? formatBytes(size) : ""}</span>
      </div>
      <iframe title={path} src={src} className="min-h-0 flex-1 bg-background" />
    </div>
  );
}
