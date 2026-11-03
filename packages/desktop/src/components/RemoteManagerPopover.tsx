import { useCallback,useEffect,useState } from "react";
import clsx from "clsx";
import {
Cloud,
GitBranch,
Globe,
Loader2,
Plus,
Trash2,
Upload,
Wifi,
WifiOff,
X
} from "lucide-react";
import { rpc } from "../ipc";
import { useActiveRepo } from "../state";
import { sanitizeRemoteUrl } from "../lib/format";

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
  if (url.startsWith("crab://") || url.startsWith("gs://") || url.startsWith("az://")) {
    return "crab";
  }
  return "git";
}

function remoteProviderLabel(url: string): string {
  if (url.startsWith("crab://")) return "S3 (Crab)";
  if (url.startsWith("gs://")) return "GCS";
  if (url.startsWith("az://")) return "Azure";
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

    // Verify the storage config (applies to the primary crab remote).
    rpc.storageConfigVerify(activeRepo.repo_id).then((result) => {
      setRemotes((prev) =>
        prev.map((r) =>
          r.kind === "crab"
            ? { ...r, status: result.ok ? "connected" : "failed" }
            : r,
        ),
      );
    }).catch(() => {
      setRemotes((prev) =>
        prev.map((r) =>
          r.kind === "crab" ? { ...r, status: "failed" } : r,
        ),
      );
    });
  }, [open, activeRepo, remotes.length]);

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
    <div className="absolute bottom-8 right-2 z-50 w-80 rounded-lg border border-gx-border bg-gx-surface shadow-xl">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-gx-border px-3 py-2">
        <span className="text-xs font-semibold text-gx-text">Remotes</span>
        <div className="flex items-center gap-1">
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-bg-sub hover:text-gx-text"
            onClick={() => setShowAddForm(true)}
            title="Add remote"
          >
            <Plus size={12} />
          </button>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-bg-sub hover:text-gx-text"
            onClick={onClose}
            title="Close"
          >
            <X size={12} />
          </button>
        </div>
      </div>

      {/* Content */}
      <div className="max-h-64 overflow-y-auto p-2">
        {loading ? (
          <div className="flex items-center gap-2 p-2 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading remotes...
          </div>
        ) : remotes.length === 0 ? (
          <div className="p-2 text-xs text-gx-muted">
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
          <div className="mt-2 rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
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
    </div>
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
      <Wifi size={10} className="text-gx-success" />
    ) : remote.status === "failed" ? (
      <WifiOff size={10} className="text-gx-error" />
    ) : remote.status === "verifying" ? (
      <Loader2 size={10} className="animate-spin text-gx-warn" />
    ) : null;

  const kindIcon =
    remote.kind === "crab" ? (
      <Cloud size={11} className="text-gx-accent" />
    ) : (
      <GitBranch size={11} className="text-gx-muted" />
    );

  return (
    <div className="group rounded border border-transparent px-2 py-1.5 hover:border-gx-border hover:bg-gx-bg-sub">
      <div className="flex items-center gap-2">
        {kindIcon}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-1.5">
            <span className="text-xs font-medium text-gx-text truncate">
              {remote.name}
            </span>
            {statusIcon}
            <span className="text-[10px] text-gx-muted">
              {remoteProviderLabel(remote.url)}
            </span>
          </div>
          <div className="truncate text-[10px] font-mono text-gx-muted">
            {sanitizeRemoteUrl(remote.url)}
          </div>
        </div>
        <div className="flex items-center gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity">
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-accent/10 hover:text-gx-accent"
            onClick={onPush}
            title={`Push to ${remote.name}`}
          >
            <Upload size={11} />
          </button>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-error/10 hover:text-gx-error"
            onClick={() => setConfirmDelete(true)}
            title={`Remove ${remote.name}`}
          >
            <Trash2 size={11} />
          </button>
        </div>
      </div>

      {/* Delete confirmation */}
      {confirmDelete && (
        <div className="mt-1.5 flex items-center gap-2 rounded border border-gx-error/30 bg-gx-error/5 p-1.5 text-[10px]">
          <span className="text-gx-error">Remove remote "{remote.name}"?</span>
          <button
            type="button"
            className="rounded bg-gx-error/20 px-2 py-0.5 text-gx-error hover:bg-gx-error/30"
            onClick={() => { onRemove(); setConfirmDelete(false); }}
          >
            Remove
          </button>
          <button
            type="button"
            className="rounded px-2 py-0.5 text-gx-muted hover:text-gx-text"
            onClick={() => setConfirmDelete(false)}
          >
            Cancel
          </button>
        </div>
      )}
    </div>
  );
}

// ── Add remote form ──────────────────────────────────────────────────

type AddRemoteType = "crab-s3" | "crab-gcs" | "crab-azure" | "git";

const ADD_REMOTE_TYPES: { value: AddRemoteType; label: string; icon: typeof Cloud }[] = [
  { value: "crab-s3", label: "AWS S3", icon: Cloud },
  { value: "crab-gcs", label: "Google Cloud", icon: Cloud },
  { value: "crab-azure", label: "Azure Blob", icon: Cloud },
  { value: "git", label: "Git Server", icon: Globe },
];

function AddRemoteForm({
  onAdded,
  onCancel,
}: {
  onAdded: () => void;
  onCancel: () => void;
}) {
  const activeRepo = useActiveRepo();
  const [type, setType] = useState<AddRemoteType>("git");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [bucket, setBucket] = useState("");
  const [prefix, setPrefix] = useState("");
  const [region, setRegion] = useState("us-east-1");
  // Credential fields
  const [accessKeyId, setAccessKeyId] = useState("");
  const [secretAccessKey, setSecretAccessKey] = useState("");
  const [gcpServiceAccountJson, setGcpServiceAccountJson] = useState("");
  const [azureConnectionString, setAzureConnectionString] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function buildUrl(): string {
    switch (type) {
      case "crab-s3":
        return `crab://${bucket}${prefix ? `/${prefix}` : ""}`;
      case "crab-gcs":
        return `gs://${bucket}${prefix ? `/${prefix}` : ""}`;
      case "crab-azure":
        return `az://${bucket}${prefix ? `/${prefix}` : ""}`;
      case "git":
        return url;
    }
  }

  function providerKey(): string {
    switch (type) {
      case "crab-s3": return "s3";
      case "crab-gcs": return "gcs";
      case "crab-azure": return "azure";
      default: return "";
    }
  }

  async function handleAdd() {
    if (!activeRepo || !name.trim()) return;
    const remoteUrl = buildUrl();
    if (!remoteUrl.trim()) return;

    setSaving(true);
    setError(null);
    try {
      // Add the git remote.
      await rpc.gitRemoteAdd(activeRepo.repo_id, name.trim(), remoteUrl.trim());

      // For object-store remotes, also configure storage + credentials.
      if (type !== "git" && bucket.trim()) {
        const credentials: Record<string, string> = {};
        if (type === "crab-s3") {
          if (accessKeyId) credentials.access_key_id = accessKeyId;
          if (secretAccessKey) credentials.secret_access_key = secretAccessKey;
        } else if (type === "crab-gcs") {
          if (gcpServiceAccountJson) credentials.service_account_json = gcpServiceAccountJson;
        } else if (type === "crab-azure") {
          if (azureConnectionString) credentials.connection_string = azureConnectionString;
        }

        await rpc.storageConfigSet(activeRepo.repo_id, {
          provider: providerKey(),
          bucket: bucket.trim(),
          prefix: prefix.trim() || undefined,
          region: type === "crab-s3" ? region : undefined,
          credentials: Object.keys(credentials).length > 0 ? credentials : undefined,
        });
      }

      onAdded();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  const isObjectStore = type !== "git";
  const canSave = name.trim() && (isObjectStore ? bucket.trim() : url.trim());

  return (
    <div className="border-t border-gx-border p-3 space-y-3">
      <div className="text-xs font-semibold text-gx-text">Add Remote</div>

      {/* Type selector */}
      <div className="flex flex-wrap gap-1.5">
        {ADD_REMOTE_TYPES.map((t) => (
          <button
            key={t.value}
            type="button"
            className={clsx(
              "rounded border px-2 py-1 text-[10px] font-medium transition",
              type === t.value
                ? "border-gx-accent bg-gx-accent/10 text-gx-accent"
                : "border-gx-border text-gx-muted hover:border-gx-accent/50",
            )}
            onClick={() => setType(t.value)}
          >
            {t.label}
          </button>
        ))}
      </div>

      {/* Name */}
      <div>
        <label className="mb-0.5 block text-[10px] text-gx-muted">Remote name</label>
        <input
          type="text"
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={isObjectStore ? "storage" : "origin"}
        />
      </div>

      {/* URL fields */}
      {isObjectStore ? (
        <>
          <div>
            <label className="mb-0.5 block text-[10px] text-gx-muted">
              {type === "crab-azure" ? "Container" : "Bucket"}
            </label>
            <input
              type="text"
              className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
              value={bucket}
              onChange={(e) => setBucket(e.target.value)}
              placeholder="my-bucket"
            />
          </div>
          <div>
            <label className="mb-0.5 block text-[10px] text-gx-muted">
              Prefix <span className="text-gx-muted/60">(optional)</span>
            </label>
            <input
              type="text"
              className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
              value={prefix}
              onChange={(e) => setPrefix(e.target.value)}
              placeholder="team/project"
            />
          </div>
          {/* Region (S3 only) */}
          {type === "crab-s3" && (
            <div>
              <label className="mb-0.5 block text-[10px] text-gx-muted">Region</label>
              <input
                type="text"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
                value={region}
                onChange={(e) => setRegion(e.target.value)}
                placeholder="us-east-1"
              />
            </div>
          )}
          {/* Credentials */}
          {type === "crab-s3" && (
            <div className="space-y-1.5">
              <label className="block text-[10px] text-gx-muted">
                Credentials <span className="text-gx-muted/60">(optional — uses env/IAM if empty)</span>
              </label>
              <input
                type="password"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
                value={accessKeyId}
                onChange={(e) => setAccessKeyId(e.target.value)}
                placeholder="AWS Access Key ID"
                autoComplete="off"
              />
              <input
                type="password"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
                value={secretAccessKey}
                onChange={(e) => setSecretAccessKey(e.target.value)}
                placeholder="AWS Secret Access Key"
                autoComplete="off"
              />
            </div>
          )}
          {type === "crab-gcs" && (
            <div>
              <label className="mb-0.5 block text-[10px] text-gx-muted">
                Service Account JSON <span className="text-gx-muted/60">(optional)</span>
              </label>
              <textarea
                className="h-16 w-full rounded border border-gx-border bg-gx-bg-sub p-1.5 font-mono text-[10px] focus-visible:outline-none focus:border-gx-accent"
                value={gcpServiceAccountJson}
                onChange={(e) => setGcpServiceAccountJson(e.target.value)}
                placeholder='{"type":"service_account",...}'
              />
            </div>
          )}
          {type === "crab-azure" && (
            <div>
              <label className="mb-0.5 block text-[10px] text-gx-muted">
                Connection String <span className="text-gx-muted/60">(optional)</span>
              </label>
              <input
                type="password"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
                value={azureConnectionString}
                onChange={(e) => setAzureConnectionString(e.target.value)}
                placeholder="DefaultEndpointsProtocol=https;AccountName=..."
                autoComplete="off"
              />
            </div>
          )}
          <p className="text-[9px] text-gx-muted">
            Credentials are stored in your OS keychain, never written to disk.
          </p>
          {/* Preview */}
          {bucket && (
            <div className="rounded bg-gx-bg-sub px-2 py-1 font-mono text-[10px] text-gx-muted">
              {buildUrl()}
            </div>
          )}
        </>
      ) : (
        <div>
          <label className="mb-0.5 block text-[10px] text-gx-muted">URL</label>
          <input
            type="text"
            className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://github.com/user/repo.git"
          />
          <p className="mt-0.5 text-[9px] text-gx-muted">
            Supports GitHub, GitLab, Bitbucket, or any git URL (HTTPS/SSH).
          </p>
        </div>
      )}

      {/* Actions */}
      <div className="flex items-center gap-2">
        <button
          type="button"
          className="btn btn-primary text-xs"
          onClick={handleAdd}
          disabled={saving || !canSave}
        >
          {saving ? <Loader2 size={10} className="animate-spin" /> : <Plus size={10} />}
          Add Remote
        </button>
        <button type="button" className="btn text-xs" onClick={onCancel}>
          Cancel
        </button>
      </div>

      {error && (
        <div className="rounded border border-gx-error/40 bg-gx-error/10 p-1.5 text-[10px] text-gx-error">
          {error}
        </div>
      )}
    </div>
  );
}
