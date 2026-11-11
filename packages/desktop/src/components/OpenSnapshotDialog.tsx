import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useEffect, useId, useRef, useState } from "react";
import { Loader2, X } from "lucide-react";
import { isCancelledError, isStorageRepositoryUrl, rpc } from "../ipc";
import { useStore } from "../state";
import { localPathName } from "../lib/local-path-name";
import { StorageConfigSection } from "../views/StorageConfigSection";

export function OpenSnapshotDialog({ onClose }: { onClose: () => void }) {
  const { addRepo } = useStore();
  const dialog = useRef<HTMLDialogElement>(null);
  const opening = useRef<AbortController | null>(null);
  const [repoId] = useState(() => `url-${crypto.randomUUID()}`);
  const [url, setUrl] = useState("");
  const [name, setName] = useState("");
  const [connectionId, setConnectionId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const heading = useId();
  const form = useId();
  useEffect(() => {
    const element = dialog.current;
    element?.showModal();
    return () => { opening.current?.abort(); element?.close(); };
  }, []);

  function cancel() {
    if (!opening.current) { onClose(); return; }
    setCancelling(true);
    opening.current.abort();
  }

  async function open() {
    if (opening.current) return;
    const source = url.trim();
    setError(null);
    try {
      if (!isStorageRepositoryUrl(source)) throw new Error("Enter a crab://, s3://, gs:// or az:// repository URL.");
      const parsed = new URL(source);
      if (!parsed.hostname || parsed.username || parsed.password || parsed.search || parsed.hash) throw new Error("Repository URLs must contain no credentials, query or fragment. Select a storage connection below.");
      const controller = new AbortController();
      opening.current = controller;
      setBusy(true);
      const handle = await rpc.openUrl(source, connectionId, repoId, controller.signal);
      // Cancellation may race successful publication. Retire that owner before
      // dismissing, rather than leaving an unlisted native snapshot alive.
      if (controller.signal.aborted) await rpc.close(handle.repo_id);
      else addRepo({ ...handle, path: source, label: name.trim() || localPathName(source), openedAt: Date.now() });
      onClose();
    } catch (failure) {
      if (isCancelledError(failure)) onClose();
      else setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      opening.current = null;
      setBusy(false); setCancelling(false);
    }
  }

  return (
    <dialog ref={dialog} aria-labelledby={heading} onCancel={event => { event.preventDefault(); cancel(); }} className="m-auto w-[min(42rem,calc(100vw-2rem))] max-h-[90vh] overflow-y-auto rounded-lg border border-border bg-card p-0 text-foreground shadow-2xl backdrop:bg-black/50">
      <div className="flex items-center justify-between border-b border-border p-4">
        <h2 id={heading} className="text-sm font-semibold">Open storage snapshot</h2>
        <Button variant="outline" type="button" className="px-1.5" aria-label="Close snapshot dialog" disabled={cancelling} onClick={cancel}><X size={16} /></Button>
      </div>
      <p className="px-4 pt-4 text-xs text-muted-foreground">Browse stored content without a working copy. Clone a repository to edit or publish changes.</p>
      <form id={form} onSubmit={event => { event.preventDefault(); void open(); }} className="space-y-3 p-4">
        <label className="block space-y-1 text-xs"><span>Repository URL</span><Input autoFocus required disabled={busy} value={url} onChange={event => setUrl(event.target.value)} placeholder="crab://bucket/repository" className="w-full py-2" /></label>
        <label className="block space-y-1 text-xs"><span>Name (optional)</span><Input disabled={busy} maxLength={120} value={name} onChange={event => setName(event.target.value)} className="w-full py-2" /></label>
      </form>
      <fieldset disabled={busy}><StorageConfigSection onSelect={setConnectionId} selectedId={connectionId ?? ""} /></fieldset>
      {error && <p role="alert" className="mx-4 mb-4 break-words text-xs text-destructive">{error}</p>}
      <div className="flex justify-end gap-2 border-t border-border p-4">
        <Button variant="outline" type="button" className="text-xs" disabled={cancelling} onClick={cancel}>{cancelling ? "Cancelling…" : "Cancel"}</Button>
        <Button variant="default" form={form} type="submit" disabled={busy || !url.trim()} className="text-xs">{busy && <Loader2 size={14} className="animate-spin" />}{busy ? "Opening…" : "Open snapshot"}</Button>
      </div>
    </dialog>
  );
}
