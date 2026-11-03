import { useEffect, useState } from "react";
import { Film, Loader2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

/**
 * HTML5 video player for .mp4, .webm, .mov files.
 * Reads the file as base64 and creates an object URL.
 */
export function VideoView({ repoId, path, rev }: Props) {
  const [url, setUrl] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [size, setSize] = useState<number>(0);

  useEffect(() => {
    let cancelled = false;
    rpc
      .readBytes(repoId, path, { rev: rev ?? undefined, max_bytes: 200 * 1024 * 1024 })
      .then((res) => {
        if (cancelled) return;
        if (res.truncated) {
          setErr(`Video too large to preview (${formatBytes(res.size)})`);
          return;
        }
        setSize(res.size);
        const binary = atob(res.bytes_b64!);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
        const ext = path.split(".").pop()?.toLowerCase() ?? "mp4";
        const mime = ext === "webm" ? "video/webm" : ext === "mov" ? "video/quicktime" : "video/mp4";
        const blob = new Blob([bytes], { type: mime });
        setUrl(URL.createObjectURL(blob));
      })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>;
  if (!url) return <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={14} className="animate-spin" /> Loading video…</div>;

  return (
    <div className="flex h-full flex-col items-center justify-center bg-gx-bg-sub p-4">
      <div className="mb-2 flex items-center gap-2 text-[11px] text-gx-muted"><Film size={12} /> {path.split("/").pop()} · {formatBytes(size)}</div>
      <video controls className="max-h-full max-w-full rounded shadow-lg" src={url} />
    </div>
  );
}
