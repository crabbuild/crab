import { useEffect, useRef, useState } from "react";
import { Film, Loader2, Music } from "lucide-react";
import { onAgentEvent, rpc, type MediaSource } from "../../ipc";
import { formatBytes } from "../../lib/format";
import { publishToast } from "../../lib/toasts";
import { useStore } from "../../state";

export function MediaView({ repoId, path, rev, kind }: { repoId: string; path: string; rev: string | null; kind: "audio" | "video" }) {
  const [source, setSource] = useState<MediaSource | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const [retry, setRetry] = useState(0);
  const element = useRef<HTMLMediaElement | null>(null);
  const { state } = useStore();
  const nativeSessionId = state.repos.find(repo => repo.repo_id === repoId)?.native_session_id;

  useEffect(() => {
    setSource(null); setError(null); setReady(false);
    if (!nativeSessionId) return;
    const abort = new AbortController();
    let captured: MediaSource | undefined;
    const close = (source: MediaSource) => rpc.closeMedia(source.media_id).catch((error: unknown) => {
      publishToast({ kind: "error", title: "Could not close media reader", body: error instanceof Error ? error.message : String(error) });
    });
    const unsubscribe = onAgentEvent(event => {
      if (event.method !== "media/error" || !event.params || typeof event.params !== "object") return;
      const params = event.params as { media_id?: unknown; error?: { message?: unknown } };
      if (params.media_id === captured?.media_id && typeof params.error?.message === "string") setError(params.error.message);
    });
    void rpc.openMedia(repoId, path, { native_session_id: nativeSessionId, rev: rev ?? undefined, signal: abort.signal }).then(async source => {
      if (abort.signal.aborted) await close(source);
      else { captured = source; setSource(source); }
    }).catch((error: unknown) => {
      if (!abort.signal.aborted) setError(error instanceof Error ? error.message : String(error));
    });
    return () => {
      abort.abort(); unsubscribe();
      const media = element.current;
      if (media) { media.pause(); media.removeAttribute("src"); media.load(); }
      if (captured) void close(captured);
    };
  }, [repoId, path, rev, retry, nativeSessionId]);

  const failed = () => setError(current => current ?? "This media container or codec could not be played. Reopen the file to retry.");
  const icon = kind === "video" ? <Film size={14} /> : <Music size={14} />;
  return <section aria-label={`${kind === "video" ? "Video" : "Audio"} preview`} className="flex h-full min-h-0 flex-col items-center justify-center gap-4 bg-gx-bg-sub p-4">
    <p className="flex max-w-full items-center gap-2 text-xs text-gx-muted">{icon}<span className="truncate">{path.split("/").pop()}</span>{source && <span className="shrink-0">· {formatBytes(source.size)}</span>}</p>
    {!nativeSessionId ? <p role="status" className="text-xs text-gx-muted">Waiting for repository recovery…</p> : error ? <div className="max-w-xl space-y-3 text-xs"><p role="alert" className="break-words rounded border border-gx-error/40 bg-gx-error/10 p-3 text-gx-error">{error}</p><button type="button" className="btn" onClick={() => setRetry(retry + 1)}>Reopen media</button></div> : <>
      {source && (kind === "video" ? <video ref={node => { element.current = node; }} controls preload="metadata" crossOrigin="anonymous" className="min-h-0 max-w-full flex-1 rounded" src={source.url} onLoadedMetadata={() => setReady(true)} onError={failed} /> : <audio ref={node => { element.current = node; }} controls preload="metadata" crossOrigin="anonymous" className="w-full max-w-lg" src={source.url} onLoadedMetadata={() => setReady(true)} onError={failed} />)}
      {!ready && <p role="status" className="flex items-center gap-2 text-xs text-gx-muted"><Loader2 size={14} className="animate-spin" />{source ? "Preparing media…" : "Opening media…"}</p>}
      {source?.kind === "lfs" && !ready && <p className="max-w-lg text-center text-xs text-gx-muted">A first read may verify the complete LFS object before playback can begin.</p>}
    </>}
  </section>;
}
