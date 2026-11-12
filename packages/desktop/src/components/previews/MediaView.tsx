import { Button } from "@/components/ui/button";
import { useCallback, useEffect, useRef, useState, type SyntheticEvent } from "react";
import { Film, Loader2, Music } from "lucide-react";
import { onAgentEvent, rpc, type MediaSource } from "../../ipc";
import { formatBytes } from "../../lib/format";
import { publishToast } from "../../lib/toasts";
import { useStore } from "../../state";

const INITIAL_PLAYBACK = { position: 0, volume: 1, muted: false, rate: 1, playing: false };

export function MediaView({ repoId, path, rev, kind }: { repoId: string; path: string; rev: string | null; kind: "audio" | "video" }) {
  const [source, setSource] = useState<MediaSource | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const [retry, setRetry] = useState(0);
  const [resumeBlocked, setResumeBlocked] = useState(false);
  const element = useRef<HTMLMediaElement | null>(null);
  const playback = useRef({ ...INITIAL_PLAYBACK });
  const selection = useRef(JSON.stringify([repoId, path, rev]));
  const restoring = useRef(true);
  const { state } = useStore();
  const nativeSessionId = state.repos.find(repo => repo.repo_id === repoId)?.native_session_id;

  const remember = useCallback((media: HTMLMediaElement) => {
    if (restoring.current || media !== element.current) return;
    playback.current = { ...playback.current, position: media.currentTime, volume: media.volume, muted: media.muted, rate: media.playbackRate };
  }, []);
  const stop = useCallback((media: HTMLMediaElement) => {
    remember(media);
    // React detaches refs before effect cleanup. Release the old element
    // here too, without recording teardown's empty/default playback state.
    restoring.current = true;
    media.pause(); media.removeAttribute("src"); media.load();
  }, [remember]);
  const attach = useCallback((media: HTMLMediaElement | null) => {
    if (element.current && element.current !== media) stop(element.current);
    element.current = media;
    if (!media) restoring.current = true;
  }, [stop]);

  useEffect(() => {
    const key = JSON.stringify([repoId, path, rev]);
    if (selection.current !== key) { selection.current = key; playback.current = { ...INITIAL_PLAYBACK }; }
    restoring.current = true;
    setSource(null); setError(null); setReady(false); setResumeBlocked(false);
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
      if (media) stop(media);
      if (captured) void close(captured);
    };
  }, [repoId, path, rev, retry, nativeSessionId, stop]);

  const failed = () => setError(current => current ?? "This media container or codec could not be played. Reopen the file to retry.");
  const rememberEvent = (event: SyntheticEvent<HTMLMediaElement>) => remember(event.currentTarget);
  const restore = (event: SyntheticEvent<HTMLMediaElement>) => {
    const media = event.currentTarget, saved = playback.current;
    if (media !== element.current) return;
    media.volume = saved.volume; media.muted = saved.muted; media.playbackRate = saved.rate;
    media.currentTime = Math.min(saved.position, Number.isFinite(media.duration) ? media.duration : saved.position);
    restoring.current = false; setReady(true);
    if (saved.playing) void media.play().catch(() => {
      if (element.current === media && !restoring.current) setResumeBlocked(true);
    });
  };
  const controls = {
    ref: attach, controls: true, preload: "metadata", crossOrigin: "anonymous" as const, src: source?.url,
    onLoadedMetadata: restore, onError: failed, onTimeUpdate: rememberEvent, onSeeked: rememberEvent,
    onVolumeChange: rememberEvent, onRateChange: rememberEvent,
    onPlay: (event: SyntheticEvent<HTMLMediaElement>) => { if (!restoring.current && event.currentTarget === element.current) { playback.current.playing = true; setResumeBlocked(false); rememberEvent(event); } },
    onPause: (event: SyntheticEvent<HTMLMediaElement>) => { if (!restoring.current && event.currentTarget === element.current && !event.currentTarget.error) { playback.current.playing = false; rememberEvent(event); } },
  };
  const icon = kind === "video" ? <Film size={14} /> : <Music size={14} />;
  return <section aria-label={`${kind === "video" ? "Video" : "Audio"} preview`} className="flex h-full min-h-0 flex-col items-center justify-center gap-4 bg-background p-4">
    <p className="flex max-w-full items-center gap-2 text-xs text-muted-foreground">{icon}<span className="truncate">{path.split("/").pop()}</span>{source && <span className="shrink-0">· {formatBytes(source.size)}</span>}</p>
    {!nativeSessionId ? <p role="status" className="text-xs text-muted-foreground">Waiting for repository recovery…</p> : error ? <div className="max-w-xl space-y-3 text-xs"><p role="alert" className="break-words rounded border border-destructive/40 bg-destructive/10 p-3 text-destructive">{error}</p><Button variant="outline" type="button" onClick={() => setRetry(retry + 1)}>Reopen media</Button></div> : <>
      {source && (kind === "video" ? <video {...controls} className="min-h-0 max-w-full flex-1 rounded" /> : <audio {...controls} className="w-full max-w-lg" />)}
      {resumeBlocked && <p role="status" className="text-xs text-muted-foreground">Media restored. Press Play to resume.</p>}
      {!ready && <p role="status" className="flex items-center gap-2 text-xs text-muted-foreground"><Loader2 size={14} className="animate-spin" />{source ? "Preparing media…" : "Opening media…"}</p>}
      {source?.kind === "lfs" && !ready && <p className="max-w-lg text-center text-xs text-muted-foreground">A first read may verify the complete LFS object before playback can begin.</p>}
    </>}
  </section>;
}
