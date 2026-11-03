import { useEffect, useState } from "react";
import { Loader2, Music } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props { repoId: string; path: string; rev: string | null; }

/** HTML5 audio player for .mp3, .wav, .flac, .ogg files. */
export function AudioView({ repoId, path, rev }: Props) {
  const [url, setUrl] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [size, setSize] = useState<number>(0);

  useEffect(() => {
    let cancelled = false;
    rpc
      .readBytes(repoId, path, { rev: rev ?? undefined, max_bytes: 100 * 1024 * 1024 })
      .then((res) => {
        if (cancelled) return;
        if (res.truncated) { setErr(`Audio too large (${formatBytes(res.size)})`); return; }
        setSize(res.size);
        const binary = atob(res.bytes_b64!);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
        const ext = path.split(".").pop()?.toLowerCase() ?? "mp3";
        const mimeMap: Record<string, string> = { mp3: "audio/mpeg", wav: "audio/wav", flac: "audio/flac", ogg: "audio/ogg", aac: "audio/aac", m4a: "audio/mp4", wma: "audio/x-ms-wma" };
        const blob = new Blob([bytes], { type: mimeMap[ext] ?? "audio/mpeg" });
        setUrl(URL.createObjectURL(blob));
      })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev]);

  if (err) return <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>;
  if (!url) return <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={14} className="animate-spin" /> Loading audio…</div>;

  return (
    <div className="flex h-full flex-col items-center justify-center bg-gx-bg-sub p-8">
      <Music size={48} className="mb-4 text-gx-accent/50" />
      <div className="mb-4 text-sm font-medium">{path.split("/").pop()}</div>
      <div className="mb-2 text-[11px] text-gx-muted">{formatBytes(size)}</div>
      <audio controls className="w-full max-w-md" src={url} />
    </div>
  );
}
