import { useState } from "react";
import { Loader2 } from "lucide-react";
import { rpc } from "../ipc";
import { StorageConfigSection } from "../views/StorageConfigSection";

export function CrabSetupWizard({ repoId, onComplete }: { repoId: string; onComplete: () => void }) {
  const [url, setUrl] = useState("");
  const [connection, setConnection] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function initialize() {
    setBusy(true); setError(null);
    try {
      await rpc.storageConnectionBind(repoId, connection || null);
      const result = await rpc.crabInit(repoId, url.trim());
      if (!result.ok) throw new Error(result.error || "Repository initialization failed.");
      onComplete();
    } catch (error) { setError(error instanceof Error ? error.message : String(error)); }
    finally { setBusy(false); }
  }

  return (
    <div className="h-full overflow-auto p-6"><div className="mx-auto max-w-xl space-y-4">
      <h2 className="text-lg font-semibold">Set up large-file storage</h2>
      <p className="text-sm text-gx-muted">Select a saved connection or use default provider credentials, then initialize your repository.</p>
      <StorageConfigSection onSelect={setConnection} selectedId={connection} />
      <label className="block space-y-1 text-xs text-gx-muted"><span>Repository URL</span><input className="w-full rounded border border-gx-border bg-gx-bg px-3 py-2 text-sm" value={url} onChange={event => setUrl(event.target.value)} placeholder="crab://bucket/repository" disabled={busy} /><span className="block">Azure: az://account/container/repository. GCS: gs://bucket/repository.</span></label>
      {error && <p role="alert" className="break-words text-xs text-gx-error">{error}</p>}
      <button type="button" className="btn btn-primary text-sm" disabled={busy || !url.trim()} onClick={() => void initialize()}>{busy && <Loader2 size={14} className="animate-spin" />}Initialize repository</button>
    </div></div>
  );
}
