import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { PopoverContent } from "@/components/ui/popover";
import {
Cloud,
GitBranch,
Loader2,
Plus,
Trash2,
Upload,
Wifi,
WifiOff,
X
} from "lucide-react";
import { useCallback,useEffect,useState } from "react";
import { rpc } from "../ipc";
import { sanitizeRemoteUrl } from "../lib/format";
import { useActiveRepo } from "../state";
import { StorageConfigSection } from "../views/StorageConfigSection";

type RemoteStatus = "unchecked" | "verifying" | "connected" | "failed";

interface RemoteEntry {
  name: string;
  url: string;
  type: string; // "fetch" | "push"
  kind: RemoteKind;
  status: RemoteStatus;
}

type RemoteKind = "crab" | "git";

interface Props {
  open: boolean;
  onClose: () => void;
  onPushTo?: (remoteName: string) => void;
}

function classifyRemote(url: string): RemoteKind {
  if (/^(crab|s3|gs|gcs|az|azure):\/\//.test(url)) {
    return "crab";
  }
  return "git";
}

function remoteProviderLabel(url: string): string {
  if (url.startsWith("crab://")) return "Crab storage";
  if (url.startsWith("s3://")) return "S3";
  if (url.startsWith("gs://") || url.startsWith("gcs://")) return "GCS";
  if (url.startsWith("az://") || url.startsWith("azure://")) return "Azure";
  if (url.includes("github.com")) return "GitHub";
  if (url.includes("gitlab.com") || url.includes("gitlab")) return "GitLab";
  if (url.includes("bitbucket.org") || url.includes("bitbucket")) return "Bitbucket";
  if (url.includes("codecommit")) return "CodeCommit";
  if (url.startsWith("https://") || url.startsWith("http://")) return "Git (HTTPS)";
  if (url.startsWith("git@") || url.includes("ssh://")) return "Git (SSH)";
  return "Git";
}

/**
 * Popover that appears when clicking the connection icon in the status bar.
 * Lists all configured remotes (both object-store and git server), shows
 * connection status, and lets users add/remove remotes or push to a
 * specific one.
 */
export function RemoteManagerPopover({ open, onClose, onPushTo }: Props) {
  const activeRepo = useActiveRepo();
  const [remotes, setRemotes] = useState<RemoteEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [showAddForm, setShowAddForm] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadRemotes = useCallback(async () => {
    if (!activeRepo) return;
    setLoading(true);
    setError(null);
    try {
      const info = await rpc.gitRepoInfo(activeRepo.repo_id);
      // Deduplicate by name (git reports fetch + push separately).
      const byName = new Map<string, RemoteEntry>();
      for (const r of info.remotes) {
        if (!byName.has(r.name) || r.type === "push") {
          byName.set(r.name, {
            name: r.name,
            url: r.url,
            type: r.type,
            kind: classifyRemote(r.url),
            status: "unchecked",
          });
        }
      }
      setRemotes(Array.from(byName.values()));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [activeRepo]);

  useEffect(() => {
    if (open) loadRemotes();
  }, [open, loadRemotes]);

  // Verify connectivity for crab remotes.
  useEffect(() => {
    if (!open || !activeRepo || remotes.length === 0) return;
    const crabRemotes = remotes.filter((r) => r.kind === "crab" && r.status === "unchecked");
    if (crabRemotes.length === 0) return;

    let cancelled = false;
    Promise.all(crabRemotes.map(async remote => {
      try { const result = await rpc.storageConfigVerify(activeRepo.repo_id, remote.name); return [remote.name, result.ok] as const; }
      catch { return [remote.name, false] as const; }
    })).then(results => {
      if (cancelled) return;
      const checked = new Map(results);
      setRemotes(previous => previous.map(remote => checked.has(remote.name) ? { ...remote, status: checked.get(remote.name) ? "connected" : "failed" } : remote));
    });
    return () => { cancelled = true; };
  }, [open, activeRepo, remotes]);

  async function handleRemove(name: string) {
    if (!activeRepo) return;
    try {
      await rpc.gitRemoteRemove(activeRepo.repo_id, name);
      setRemotes((prev) => prev.filter((r) => r.name !== name));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  if (!open) return null;

  return (
    <PopoverContent side="top" align="end" className="w-80 p-0">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-3 py-2">
        <span className="text-xs font-semibold text-foreground">Remotes</span>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
            onClick={() => setShowAddForm(true)}
            title="Add remote"
          >
            <Plus size={12} />
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
            onClick={onClose}
            title="Close"
          >
            <X size={12} />
          </Button>
        </div>
      </div>

      {/* Content */}
      <div className="max-h-64 overflow-y-auto p-2">
        {loading ? (
          <div className="flex items-center gap-2 p-2 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Loading remotes...
          </div>
        ) : remotes.length === 0 ? (
          <div className="p-2 text-xs text-muted-foreground">
            No remotes configured. Add one to push your repository.
          </div>
        ) : (
          <div className="space-y-1">
            {remotes.map((remote) => (
              <RemoteRow
                key={remote.name}
                remote={remote}
                onRemove={() => handleRemove(remote.name)}
                onPush={() => onPushTo?.(remote.name)}
              />
            ))}
          </div>
        )}

        {error && (
          <div className="mt-2 rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
            {error}
          </div>
        )}
      </div>

      {/* Add remote form */}
      {showAddForm && (
        <AddRemoteForm
          onAdded={() => {
            setShowAddForm(false);
            loadRemotes();
          }}
          onCancel={() => setShowAddForm(false)}
        />
      )}
    </PopoverContent>
  );
}

// ── Remote row ───────────────────────────────────────────────────────

function RemoteRow({
  remote,
  onRemove,
  onPush,
}: {
  remote: RemoteEntry;
  onRemove: () => void;
  onPush: () => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState(false);

  const statusIcon =
    remote.status === "connected" ? (
      <Wifi size={10} className="text-success" />
    ) : remote.status === "failed" ? (
      <WifiOff size={10} className="text-destructive" />
    ) : remote.status === "verifying" ? (
      <Loader2 size={10} className="animate-spin text-warning" />
    ) : null;

  const kindIcon =
    remote.kind === "crab" ? (
      <Cloud size={11} className="text-primary" />
    ) : (
      <GitBranch size={11} className="text-muted-foreground" />
    );

  return (
    <div className="group rounded border border-transparent px-2 py-1.5 hover:border-border hover:bg-background">
      <div className="flex items-center gap-2">
        {kindIcon}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-1.5">
            <span className="text-xs font-medium text-foreground truncate">
              {remote.name}
            </span>
            {statusIcon}
            <span className="text-[10px] text-muted-foreground">
              {remoteProviderLabel(remote.url)}
            </span>
          </div>
          <div className="truncate text-[10px] font-mono text-muted-foreground">
            {sanitizeRemoteUrl(remote.url)}
          </div>
        </div>
        <div className="flex items-center gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-primary/10 hover:text-primary"
            onClick={onPush}
            title={`Push to ${remote.name}`}
          >
            <Upload size={11} />
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
            onClick={() => setConfirmDelete(true)}
            title={`Remove ${remote.name}`}
          >
            <Trash2 size={11} />
          </Button>
        </div>
      </div>

      {/* Delete confirmation */}
      {confirmDelete && (
        <div className="mt-1.5 flex items-center gap-2 rounded border border-destructive/30 bg-destructive/5 p-1.5 text-[10px]">
          <span className="text-destructive">
            Remove remote "{remote.name}"?
          </span>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded bg-destructive/20 px-2 py-0.5 text-destructive hover:bg-destructive/30"
            onClick={() => { onRemove(); setConfirmDelete(false); }}
          >
            Remove
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-2 py-0.5 text-muted-foreground hover:text-foreground"
            onClick={() => setConfirmDelete(false)}
          >
            Cancel
          </Button>
        </div>
      )}
    </div>
  );
}

// ── Add remote form ──────────────────────────────────────────────────

function AddRemoteForm({ onAdded, onCancel }: { onAdded: () => void; onCancel: () => void }) {
  const activeRepo = useActiveRepo();
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [connection, setConnection] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const objectStore = /^(crab|s3|gs|gcs|az|azure):\/\//.test(url.trim());

  async function add() {
    if (!activeRepo) return;
    setSaving(true); setError(null);
    try {
      if (objectStore) await rpc.storageConnectionBind(activeRepo.repo_id, connection || null);
      await rpc.gitRemoteAdd(activeRepo.repo_id, name.trim(), url.trim());
      onAdded();
    } catch (error) { setError(error instanceof Error ? error.message : String(error)); }
    finally { setSaving(false); }
  }

  return (
    <div className="space-y-3 border-t border-border p-3">
      <label className="block text-xs text-muted-foreground">
        Remote name
        <Input className="input mt-1 w-full" value={name} onChange={event => setName(event.target.value)} disabled={saving} />
      </label>
      <label className="block text-xs text-muted-foreground">
        Remote URL
        <Input className="input mt-1 w-full" value={url} onChange={event => setUrl(event.target.value)} disabled={saving} placeholder="crab://bucket/repository or a Git URL" />
      </label>
      {objectStore && (
        <StorageConfigSection onSelect={setConnection} selectedId={connection} />
      )}
      {error && (
        <p role="alert" className="break-words text-xs text-destructive">
          {error}
        </p>
      )}
      <div className="flex gap-2">
        <Button variant="default" type="button" className="text-xs" disabled={saving || !name.trim() || !url.trim()} onClick={() => void add()}>
          {saving && <Loader2 size={13} className="animate-spin" />}Add remote
        </Button>
        <Button variant="outline" type="button" className="text-xs" onClick={onCancel} disabled={saving}>
          Cancel
        </Button>
      </div>
    </div>
  );
}
