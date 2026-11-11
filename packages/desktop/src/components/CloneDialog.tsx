import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { NativeSelect } from "@/components/ui/native-select";
import { StorageConfigSection } from "../views/StorageConfigSection";
import { useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  AlertCircle,
  Check,
  ChevronDown,
  ChevronRight,
  ExternalLink,
  FolderOpen,
  GitBranch,
  Globe,
  Loader2,
  Lock,
  Server,
  X,
} from "lucide-react";
import { dialogs, nextJobId, rpc } from "../ipc";
import { useStore } from "../state";
import { useSetting } from "../settings";
import { useJobLines } from "../lib/useJobLines";
import { localPathName } from "../lib/local-path-name";
import { ManagedRepositoryPicker } from "./ManagedRepositoryPicker";

// ─── Types ──────────────────────────────────────────────────────────

type RepoProvider =
  | "github"
  | "gitlab"
  | "bitbucket"
  | "managed"
  | "crab"
  | "url";


interface ProviderDef {
  id: RepoProvider;
  label: string;
  icon: React.ReactNode;
  placeholder: string;
  description: string;
}

interface AdvancedOptions {
  shallow: boolean;
  shallowDepth: number;
  singleBranch: boolean;
  branch: string;
  sparseCheckout: boolean;
  sparsePatterns: string;
  noCheckout: boolean;
  recurseSubmodules: boolean;
  shallowSubmodules: boolean;
  jobs: number;
  filter: string;
  mirror: boolean;
  bare: boolean;
  dissociate: boolean;
  reference: string;
}

function GitHubIcon() {
  return (
    <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
      <path d="M12 2C6.477 2 2 6.477 2 12c0 4.42 2.865 8.166 6.839 9.489.5.092.682-.217.682-.482 0-.237-.008-.866-.013-1.7-2.782.604-3.369-1.34-3.369-1.34-.454-1.156-1.11-1.463-1.11-1.463-.908-.62.069-.608.069-.608 1.003.07 1.531 1.03 1.531 1.03.892 1.529 2.341 1.087 2.91.832.092-.647.35-1.088.636-1.338-2.22-.253-4.555-1.11-4.555-4.943 0-1.091.39-1.984 1.029-2.683-.103-.253-.446-1.27.098-2.647 0 0 .84-.269 2.75 1.025A9.578 9.578 0 0112 6.836c.85.004 1.705.115 2.504.337 1.909-1.294 2.747-1.025 2.747-1.025.546 1.377.203 2.394.1 2.647.64.699 1.028 1.592 1.028 2.683 0 3.842-2.339 4.687-4.566 4.935.359.309.678.919.678 1.852 0 1.336-.012 2.415-.012 2.743 0 .267.18.578.688.48C19.138 20.163 22 16.418 22 12c0-5.523-4.477-10-10-10z" />
    </svg>
  );
}

function GitLabIcon() {
  return (
    <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
      <path d="M22.65 14.39L12 22.13 1.35 14.39a.84.84 0 01-.3-.94l1.22-3.78 2.44-7.51A.42.42 0 014.82 2a.43.43 0 01.58 0 .42.42 0 01.11.18l2.44 7.49h8.1l2.44-7.51A.42.42 0 0118.6 2a.43.43 0 01.58 0 .42.42 0 01.11.18l2.44 7.51L23 13.45a.84.84 0 01-.35.94z" />
    </svg>
  );
}

function BitbucketIcon() {
  return (
    <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
      <path d="M2.65 3C2.3 3 2 3.3 2 3.65v.12l2.73 16.5c.07.42.43.73.85.73h13.05c.31 0 .59-.22.64-.54L22 3.77v-.12c0-.35-.3-.65-.65-.65H2.65zM14.1 14.95H9.94L8.81 9.07h6.3l-1.01 5.88z" />
    </svg>
  );
}

function CrabIcon() {
  return (
    <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" strokeWidth="1.5">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 3v18M3 12h18" strokeLinecap="round" />
      <path d="M7 7l10 10M17 7L7 17" strokeLinecap="round" strokeOpacity="0.4" />
    </svg>
  );
}

const PROVIDERS: ProviderDef[] = [
  {
    id: "github",
    label: "GitHub",
    icon: <GitHubIcon />,
    placeholder: "https://github.com/owner/repo.git",
    description: "Clone from GitHub repositories",
  },
  {
    id: "gitlab",
    label: "GitLab",
    icon: <GitLabIcon />,
    placeholder: "https://gitlab.com/owner/repo.git",
    description: "Clone from GitLab repositories",
  },
  {
    id: "bitbucket",
    label: "Bitbucket",
    icon: <BitbucketIcon />,
    placeholder: "https://bitbucket.org/owner/repo.git",
    description: "Clone from Bitbucket repositories",
  },
  {
    id: "managed",
    label: "Crab Cloud",
    icon: <CrabIcon />,
    placeholder: "crab://crab.build/acme/models",
    description: "Clone from Crab Cloud or an enterprise managed service",
  },
  {
    id: "crab",
    label: "Crab Storage",
    icon: <CrabIcon />,
    placeholder: "crab://bucket/repo",
    description: "Clone directly from your S3, GCS, or Azure storage",
  },
  {
    id: "url",
    label: "Any URL",
    icon: <Globe size={20} />,
    placeholder: "https://example.com/repo.git or ssh://...",
    description: "Clone from any git-compatible URL",
  },
];

const DEFAULT_ADVANCED: AdvancedOptions = {
  shallow: false,
  shallowDepth: 1,
  singleBranch: false,
  branch: "",
  sparseCheckout: false,
  sparsePatterns: "",
  noCheckout: false,
  recurseSubmodules: true,
  shallowSubmodules: false,
  jobs: 0,
  filter: "",
  mirror: false,
  bare: false,
  dissociate: false,
  reference: "",
};

type IntegrationStatus = "unknown" | "checking" | "connected" | "not_connected";

function useIntegrationStatus(provider: RepoProvider) {
  const [status, setStatus] = useState<IntegrationStatus>("unknown");

  useEffect(() => {
    if (provider === "url" || provider === "crab" || provider === "managed") {
      setStatus("unknown");
      return;
    }

    setStatus("checking");
    // Check keychain for stored PAT or OAuth token
    const service = `crab-forge-${provider}`;
    Promise.all([
      window.crab.keychain.getPassword(service, "pat-token"),
      window.crab.keychain.getPassword(service, "oauth-token"),
    ])
      .then(([pat, oauth]) => {
        setStatus(pat || oauth ? "connected" : "not_connected");
      })
      .catch(() => setStatus("not_connected"));
  }, [provider]);

  return status;
}

// ─── Forge detection from URL ───────────────────────────────────────

/**
 * Determine which forge provider a URL belongs to, either from the
 * explicit provider selection or by inspecting the URL hostname.
 * Returns the keychain service suffix ("github", "gitlab", "bitbucket")
 * or undefined if no forge is detected (e.g. crab:// or unknown hosts).
 */
function detectForgeFromProvider(
  provider: RepoProvider,
  url: string,
): "github" | "gitlab" | "bitbucket" | undefined {
  // Explicit provider selection — trust it directly
  if (provider === "github") return "github";
  if (provider === "gitlab") return "gitlab";
  if (provider === "bitbucket") return "bitbucket";

  // For "url" provider, detect from the URL hostname
  if (provider === "url") {
    const host = extractHost(url);
    if (!host) return undefined;
    const lower = host.toLowerCase();
    if (lower === "github.com" || lower.endsWith(".github.com")) return "github";
    if (lower === "gitlab.com" || lower.endsWith(".gitlab.com")) return "gitlab";
    if (lower === "bitbucket.org" || lower.endsWith(".bitbucket.org")) return "bitbucket";
  }

  // crab:// or unrecognised — no forge token needed
  return undefined;
}

/**
 * Extract the hostname from a git remote URL.
 * Handles HTTPS, SSH (git@host:path), and ssh:// formats.
 */
function extractHost(url: string): string | undefined {
  // HTTPS / HTTP
  const httpsMatch = url.match(/^https?:\/\/([^/:@]+)/);
  if (httpsMatch) return httpsMatch[1];

  // SSH shorthand: git@host:owner/repo
  const sshShort = url.match(/^[^@]+@([^:]+):/);
  if (sshShort) return sshShort[1];

  // ssh://user@host/path
  const sshProto = url.match(/^ssh:\/\/[^@]+@([^/:]+)/);
  if (sshProto) return sshProto[1];

  return undefined;
}

// ─── Main component ─────────────────────────────────────────────────

interface Props {
  open: boolean;
  onClose: () => void;
  /** When set, the cloned repo will be added to this specific workspace. */
  targetWorkspaceRootPath?: string | null;
}

export function CloneDialog({ open, onClose, targetWorkspaceRootPath }: Props) {
  const [provider, setProvider] = useState<RepoProvider>("github");
  const [url, setUrl] = useState("");
  const [dest, setDest] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [advanced, setAdvanced] = useState<AdvancedOptions>(DEFAULT_ADVANCED);
  const [storageConnectionId, setStorageConnectionId] = useState("");
  const [cloneJobId, setCloneJobId] = useState<string | null>(null);

  const { addRepo } = useStore();
  const defaultCloneDir = useSetting("defaultCloneDir");
  const integrationStatus = useIntegrationStatus(provider);
  const progressLines = useJobLines(cloneJobId);

  const currentProvider = useMemo(
    () => PROVIDERS.find((p) => p.id === provider)!,
    [provider],
  );

  // Derive a repo slug from the URL for auto-filling the destination.
  const repoSlug = useMemo(() => {
    if (!url) return "";
    // Handle various URL formats:
    // https://github.com/owner/repo.git → repo
    // crab://bucket/repo → repo
    // git@github.com:owner/repo.git → repo
    // s3://bucket/path/repo → repo
    const cleaned = url
      .replace(/\.git$/, "")
      .replace(/\/$/, "");
    // For SSH-style URLs (git@host:owner/repo)
    const sshMatch = cleaned.match(/:([^/]+\/)?([^/]+)$/);
    if (sshMatch) return sshMatch[2];
    // For everything else, take the last path segment
    const segments = cleaned.split("/").filter(Boolean);
    return segments.pop() ?? "";
  }, [url]);

  // When targeting a workspace, the destination is always the workspace root.
  // This is enforced (immutable) because repos must live at the workspace root.
  const isWorkspaceClone = !!targetWorkspaceRootPath;
  const workspaceDest = useMemo(() => {
    if (!targetWorkspaceRootPath || !repoSlug) return targetWorkspaceRootPath ?? "";
    return `${targetWorkspaceRootPath}/${repoSlug}`;
  }, [targetWorkspaceRootPath, repoSlug]);

  // Auto-fill destination from URL when defaultCloneDir is set (only for non-workspace clones)
  useEffect(() => {
    if (isWorkspaceClone) return;
    if (!defaultCloneDir || !repoSlug) return;
    // Always update dest when URL changes and we have a default dir
    setDest(`${defaultCloneDir}/${repoSlug}`);
  }, [repoSlug, defaultCloneDir, isWorkspaceClone]);

  // Reset state when provider changes
  useEffect(() => {
    setUrl("");
    setDest("");
    setError(null);
  }, [provider]);


  if (!open) return null;

  async function pickDest() {
    const picked = await dialogs.pickCloneDestination();
    if (!picked) return;
    // The user picks a parent directory — append the repo slug so
    // git clone creates a new subdirectory rather than trying to
    // clone into an existing non-empty folder.
    if (repoSlug) {
      setDest(`${picked}/${repoSlug}`);
    } else {
      setDest(picked);
    }
  }

  // Effective destination: workspace path overrides manual dest
  const effectiveDest = isWorkspaceClone ? workspaceDest : dest;

  async function doClone() {
    if (!url || !effectiveDest) return;
    setBusy(true);
    setError(null);
    try {
      // Ensure the destination path includes a repo-specific subdirectory.
      // If the user typed a bare path without a slug, append one automatically.
      let finalDest = effectiveDest;
      if (!isWorkspaceClone && repoSlug && !effectiveDest.endsWith(repoSlug)) {
        // Check if the path looks like it already has a project name
        // (i.e., the last segment isn't a common parent like "Projects",
        // "repos", "src", home dir, etc.)
        const lastSegment = effectiveDest.split("/").filter(Boolean).pop() ?? "";
        const commonParents = new Set([
          "", "home", "users", "projects", "repos", "src", "code",
          "workspace", "workspaces", "dev", "development", "git",
        ]);
        if (commonParents.has(lastSegment.toLowerCase()) || effectiveDest === "/") {
          finalDest = `${effectiveDest.replace(/\/$/, "")}/${repoSlug}`;
        }
      }



      // Generate a job ID so we can track progress lines
      const jobId = nextJobId("clone");
      setCloneJobId(jobId);

      // Retrieve auth token from keychain for forge providers (GitHub,
      // GitLab, Bitbucket) so private repos can be cloned over HTTPS.
      // The agent will rewrite SSH URLs to HTTPS and embed the token.
      // For the "Any URL" provider, detect the forge from the URL itself.
      let authToken: string | undefined;
      let forgeId: string | undefined;
      const forgeProvider = detectForgeFromProvider(provider, url);
      if (forgeProvider) {
        forgeId = forgeProvider;
        const service = `crab-forge-${forgeProvider}`;
        const pat = await window.crab.keychain.getPassword(service, "pat-token");
        const oauth = await window.crab.keychain.getPassword(service, "oauth-token");
        authToken = pat ?? oauth ?? undefined;
      }

      // Build clone args based on advanced options
      await rpc.clone(url, finalDest, jobId, authToken, forgeId, provider === "crab" ? storageConnectionId || undefined : undefined);

      const handle = await rpc.openLocal(finalDest);
      addRepo({
        ...handle,
        label: localPathName(finalDest),
        path: finalDest,
        openedAt: Date.now(),
      });

      // If a workspace is targeted (or any workspace is open), refresh to discover the new repo
      if (targetWorkspaceRootPath || window.crab?.workspace) {
        window.crab.workspace.refresh().catch(() => {
          // Non-fatal: repo is still opened standalone
        });
      }

      onClose();
      resetState();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
      setCloneJobId(null);
    }
  }

  function resetState() {
    setUrl("");
    setDest("");
    setError(null);
    setShowAdvanced(false);
    setAdvanced(DEFAULT_ADVANCED);
    setStorageConnectionId("");
    setCloneJobId(null);
  }

  function handleClose() {
    onClose();
    resetState();
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onClick={handleClose}
      role="dialog"
      aria-modal="true"
      aria-labelledby="clone-dialog-title"
    >
      <div
        className="flex max-h-[85vh] w-[min(720px,92vw)] flex-col rounded-xl border border-border bg-card shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-5 py-4">
          <div>
            <h2 id="clone-dialog-title" className="text-base font-semibold">
              Clone Repository
            </h2>
            {targetWorkspaceRootPath && (
              <p className="mt-0.5 text-[11px] text-muted-foreground">
                Into workspace: <span className="font-medium text-primary">{localPathName(targetWorkspaceRootPath)}</span>
              </p>
            )}
          </div>
          <button
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={handleClose}
            aria-label="Close"
          >
            <X size={18} />
          </button>
        </div>

        {/* Scrollable body */}
        <div className="flex-1 overflow-y-auto px-5 py-4">
          {/* Provider selector */}
          <label className="mb-2 block text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Provider
          </label>
          <div className="mb-4 grid grid-cols-3 gap-2 sm:grid-cols-6">
            {PROVIDERS.map((p) => (
              <button
                key={p.id}
                type="button"
                className={clsx(
                  "flex flex-col items-center gap-1.5 rounded-lg border p-3 text-xs transition",
                  provider === p.id
                    ? "border-primary bg-primary/10 text-primary"
                    : "border-border bg-background text-muted-foreground hover:border-primary/50 hover:text-foreground",
                )}
                onClick={() => setProvider(p.id)}
                title={p.description}
              >
                <span className="flex h-6 w-6 items-center justify-center">{p.icon}</span>
                <span className="font-medium">{p.label}</span>
              </button>
            ))}
          </div>

          {/* Integration status banner for forge providers */}
          {(provider === "github" || provider === "gitlab" || provider === "bitbucket") && (
            <IntegrationBanner provider={provider} status={integrationStatus} />
          )}

          {provider === "managed" ? (
            <div className="mb-4">
              <ManagedRepositoryPicker
                selectedUrl={url}
                onSelectRepository={setUrl}
              />
            </div>
          ) : (
            <>
              <label className="mb-1 block text-xs text-muted-foreground">Remote URL</label>
              <Input
                className="mb-3 w-full font-mono text-sm"
                placeholder={currentProvider.placeholder}
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                autoFocus
              />
            </>
          )}

          {/* Destination folder */}
          <label className="mb-1 block text-xs text-muted-foreground">Destination folder</label>
          {isWorkspaceClone ? (
            /* Workspace clone: destination is pre-populated and immutable */
            <div className="flex gap-2">
              <div
                className="flex-1 font-mono text-sm bg-background text-muted-foreground cursor-not-allowed select-all"
                title="Destination is fixed to the workspace root directory"
              >
                {workspaceDest || `${targetWorkspaceRootPath}/…`}
              </div>
            </div>
          ) : (
            /* Non-workspace clone: editable destination with browse */
            <div className="flex gap-2">
              <Input
                className="flex-1 font-mono text-sm"
                placeholder={repoSlug ? `/path/to/parent/${repoSlug}` : "/path/to/new-clone"}
                value={dest}
                onChange={(e) => setDest(e.target.value)}
              />
              <Button variant="outline" type="button" onClick={pickDest}>
                <FolderOpen size={14} />
                Browse
              </Button>
            </div>
          )}
          {isWorkspaceClone && (
            <p className="mt-1 mb-3 text-[10px] text-muted-foreground">
              Repos must be cloned into the workspace root directory.
            </p>
          )}
          {!isWorkspaceClone && repoSlug && !dest.endsWith(repoSlug) && dest && (
            <p className="mt-1 mb-3 text-[10px] text-warning">
              Tip: The repo will be cloned into a new "{repoSlug}" subdirectory inside the selected folder.
            </p>
          )}
          {(!isWorkspaceClone && (!repoSlug || dest.endsWith(repoSlug) || !dest)) && <div className="mb-4" />}

          {provider === "crab" && <StorageConfigSection onSelect={setStorageConnectionId} selectedId={storageConnectionId} />}

          {/* Advanced options */}
          <AdvancedOptionsSection
            advanced={advanced}
            setAdvanced={setAdvanced}
            showAdvanced={showAdvanced}
            setShowAdvanced={setShowAdvanced}
            isCrab={provider === "crab" || provider === "managed"}
          />

          {/* Clone progress */}
          {busy && progressLines.length > 0 && (
            <CloneProgress lines={progressLines} />
          )}

          {/* Error display */}
          {error && (
            <div className="mt-3 flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
              <AlertCircle size={14} className="mt-0.5 shrink-0" />
              <span>{error}</span>
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-border px-5 py-3">
          <Button variant="outline" type="button" onClick={handleClose} disabled={busy}>
            Cancel
          </Button>
          <Button variant="default"
            type="button"

            onClick={doClone}
            disabled={busy || !url || !effectiveDest}
          >
            {busy ? (
              <>
                <Loader2 size={14} className="animate-spin" />
                Cloning…
              </>
            ) : (
              <>
                <GitBranch size={14} />
                Clone
              </>
            )}
          </Button>
        </div>
      </div>
    </div>
  );
}

// ─── Clone progress ─────────────────────────────────────────────────

function CloneProgress({ lines }: { lines: Array<{ line: string; stream: string }> }) {
  const scrollRef = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom as new lines arrive
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines.length]);

  // Parse git clone progress to extract a percentage if available
  const lastLine = lines[lines.length - 1]?.line ?? "";
  const percentMatch = lastLine.match(/(\d+)%/);
  const percent = percentMatch ? parseInt(percentMatch[1], 10) : null;

  // Determine the current phase from the last meaningful line
  const phase = useMemo(() => {
    for (let i = lines.length - 1; i >= 0; i--) {
      const l = lines[i].line;
      if (l.includes("Enumerating objects")) return "Enumerating objects";
      if (l.includes("Counting objects")) return "Counting objects";
      if (l.includes("Compressing objects")) return "Compressing objects";
      if (l.includes("Receiving objects")) return "Receiving objects";
      if (l.includes("Resolving deltas")) return "Resolving deltas";
      if (l.includes("Updating files")) return "Checking out files";
      if (l.includes("Checking out files")) return "Checking out files";
      if (l.includes("remote:")) return "Fetching from remote";
    }
    return "Cloning";
  }, [lines]);

  return (
    <div className="mt-3 rounded-lg border border-border bg-background p-3">
      {/* Phase label + percentage */}
      <div className="mb-2 flex items-center justify-between text-xs">
        <span className="flex items-center gap-2 text-muted-foreground">
          <Loader2 size={12} className="animate-spin text-primary" />
          {phase}…
        </span>
        {percent !== null && (
          <span className="font-mono text-[11px] text-primary">{percent}%</span>
        )}
      </div>

      {/* Progress bar */}
      {percent !== null && (
        <div className="mb-2 h-1.5 w-full overflow-hidden rounded-full bg-border">
          <div
            className="h-full rounded-full bg-primary transition-all duration-300 ease-out"
            style={{ width: `${percent}%` }}
          />
        </div>
      )}
      {percent === null && (
        <div className="mb-2 h-1.5 w-full overflow-hidden rounded-full bg-border">
          <div className="h-full w-1/3 animate-pulse rounded-full bg-primary/60" />
        </div>
      )}

      {/* Log output (last few lines) */}
      <div
        ref={scrollRef}
        className="max-h-24 overflow-y-auto rounded border border-border/50 bg-background p-2 font-mono text-[10px] leading-relaxed text-muted-foreground scrollbar-none"
      >
        {lines.slice(-20).map((l, i) => (
          <div key={i} className={l.stream === "stderr" ? "text-warning/80" : ""}>
            {l.line}
          </div>
        ))}
      </div>
    </div>
  );
}

// ─── Integration banner ─────────────────────────────────────────────

function IntegrationBanner({
  provider,
  status,
}: {
  provider: RepoProvider;
  status: IntegrationStatus;
}) {
  const providerLabel =
    provider === "github" ? "GitHub" : provider === "gitlab" ? "GitLab" : "Bitbucket";

  if (status === "checking") {
    return (
      <div className="mb-4 flex items-center gap-2 rounded-lg border border-border bg-background p-3 text-xs text-muted-foreground">
        <Loader2 size={14} className="animate-spin" />
        Checking {providerLabel} integration…
      </div>
    );
  }

  if (status === "connected") {
    return (
      <div className="mb-4 flex items-center gap-2 rounded-lg border border-success/30 bg-success/10 p-3 text-xs text-success">
        <Check size={14} />
        Connected to {providerLabel}. You can clone private repositories.
      </div>
    );
  }

  if (status === "not_connected") {
    return (
      <div className="mb-4 rounded-lg border border-warning/30 bg-warning/10 p-3">
        <div className="flex items-center gap-2 text-xs text-warning">
          <Lock size={14} />
          <span className="font-medium">Not connected to {providerLabel}</span>
        </div>
        <p className="mt-1.5 text-xs text-muted-foreground">
          You can still clone public repositories. To access private repos,
          connect your {providerLabel} account in{" "}
          <span className="font-medium text-foreground">Settings → Integrations</span>.
        </p>
        <Button variant="outline"
          type="button"
          className="mt-2 text-xs"
          onClick={() => {
            // Dispatch event to open settings at integrations section
            window.dispatchEvent(
              new CustomEvent("crab:open-settings", { detail: { section: "integrations" } }),
            );
          }}
        >
          <ExternalLink size={12} />
          Connect {providerLabel}
        </Button>
      </div>
    );
  }

  return null;
}

function AdvancedOptionsSection({
  advanced,
  setAdvanced,
  showAdvanced,
  setShowAdvanced,
  isCrab,
}: {
  advanced: AdvancedOptions;
  setAdvanced: React.Dispatch<React.SetStateAction<AdvancedOptions>>;
  showAdvanced: boolean;
  setShowAdvanced: (v: boolean) => void;
  isCrab: boolean;
}) {
  const activeCount = useMemo(() => {
    let count = 0;
    if (advanced.shallow) count++;
    if (advanced.singleBranch) count++;
    if (advanced.sparseCheckout) count++;
    if (advanced.noCheckout) count++;
    if (!advanced.recurseSubmodules) count++;
    if (advanced.filter) count++;
    if (advanced.mirror) count++;
    if (advanced.bare) count++;
    if (advanced.branch) count++;
    return count;
  }, [advanced]);

  return (
    <div>
      <button
        type="button"
        className="flex w-full items-center gap-2 rounded-lg border border-border bg-background px-3 py-2.5 text-xs text-muted-foreground hover:border-primary/50 hover:text-foreground transition"
        onClick={() => setShowAdvanced(!showAdvanced)}
      >
        {showAdvanced ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        <Server size={14} />
        <span className="font-medium">Advanced Options</span>
        {activeCount > 0 && (
          <span className="ml-auto rounded-full bg-primary/20 px-1.5 py-0.5 text-[10px] font-medium text-primary">
            {activeCount} active
          </span>
        )}
      </button>

      {showAdvanced && (
        <div className="mt-2 rounded-lg border border-border bg-background p-4 space-y-3">
          {/* Shallow clone */}
          <div className="flex items-start gap-3">
            <Checkbox
              checked={advanced.shallow}
              onChange={(v) => setAdvanced((p) => ({ ...p, shallow: v }))}
              id="opt-shallow"
            />
            <div className="flex-1">
              <label htmlFor="opt-shallow" className="text-xs font-medium text-foreground cursor-pointer">
                Shallow clone
              </label>
              <p className="text-[10px] text-muted-foreground">
                Only fetch the most recent commits (faster for large repos)
              </p>
              {advanced.shallow && (
                <div className="mt-1.5 flex items-center gap-2">
                  <label className="text-[10px] text-muted-foreground">Depth:</label>
                  <Input
                    type="number"
                    min={1}
                    max={9999}
                    className="w-20 text-xs"
                    value={advanced.shallowDepth}
                    onChange={(e) =>
                      setAdvanced((p) => ({ ...p, shallowDepth: Math.max(1, parseInt(e.target.value) || 1) }))
                    }
                  />
                </div>
              )}
            </div>
          </div>

          {/* Single branch */}
          <div className="flex items-start gap-3">
            <Checkbox
              checked={advanced.singleBranch}
              onChange={(v) => setAdvanced((p) => ({ ...p, singleBranch: v }))}
              id="opt-single-branch"
            />
            <div className="flex-1">
              <label htmlFor="opt-single-branch" className="text-xs font-medium text-foreground cursor-pointer">
                Single branch
              </label>
              <p className="text-[10px] text-muted-foreground">
                Only clone history for a single branch
              </p>
              {advanced.singleBranch && (
                <div className="mt-1.5 flex items-center gap-2">
                  <label className="text-[10px] text-muted-foreground">Branch:</label>
                  <Input
                    type="text"
                    className="flex-1 text-xs"
                    placeholder="main (default: remote HEAD)"
                    value={advanced.branch}
                    onChange={(e) => setAdvanced((p) => ({ ...p, branch: e.target.value }))}
                  />
                </div>
              )}
            </div>
          </div>

          {/* Sparse checkout */}
          {!isCrab && (
            <div className="flex items-start gap-3">
              <Checkbox
                checked={advanced.sparseCheckout}
                onChange={(v) => setAdvanced((p) => ({ ...p, sparseCheckout: v }))}
                id="opt-sparse"
              />
              <div className="flex-1">
                <label htmlFor="opt-sparse" className="text-xs font-medium text-foreground cursor-pointer">
                  Sparse checkout
                </label>
                <p className="text-[10px] text-muted-foreground">
                  Only check out specific directories (cone mode)
                </p>
                {advanced.sparseCheckout && (
                  <div className="mt-1.5">
                    <Textarea
                      className="w-full resize-y font-mono text-xs"
                      rows={3}
                      placeholder={"src/\ndocs/\n*.md"}
                      value={advanced.sparsePatterns}
                      onChange={(e) => setAdvanced((p) => ({ ...p, sparsePatterns: e.target.value }))}
                    />
                    <p className="mt-0.5 text-[10px] text-muted-foreground">
                      One pattern per line. Directories should end with /
                    </p>
                  </div>
                )}
              </div>
            </div>
          )}

          {/* Partial clone (filter) */}
          <div className="flex items-start gap-3">
            <Checkbox
              checked={!!advanced.filter}
              onChange={(v) =>
                setAdvanced((p) => ({ ...p, filter: v ? "blob:none" : "" }))
              }
              id="opt-filter"
            />
            <div className="flex-1">
              <label htmlFor="opt-filter" className="text-xs font-medium text-foreground cursor-pointer">
                Partial clone (blobless)
              </label>
              <p className="text-[10px] text-muted-foreground">
                Defer downloading file contents until checkout (treeless or blobless)
              </p>
              {advanced.filter && (
                <div className="mt-1.5 flex items-center gap-2">
                  <label className="text-[10px] text-muted-foreground">Filter:</label>
                  <NativeSelect
                    className="text-xs"
                    value={advanced.filter}
                    onChange={(e) => setAdvanced((p) => ({ ...p, filter: e.target.value }))}
                  >
                    <option value="blob:none">blob:none (blobless)</option>
                    <option value="tree:0">tree:0 (treeless)</option>
                    <option value="blob:limit=1m">blob:limit=1m</option>
                    <option value="blob:limit=10m">blob:limit=10m</option>
                    <option value="blob:limit=100m">blob:limit=100m</option>
                  </NativeSelect>
                </div>
              )}
            </div>
          </div>

          {/* No checkout */}
          <div className="flex items-start gap-3">
            <Checkbox
              checked={advanced.noCheckout}
              onChange={(v) => setAdvanced((p) => ({ ...p, noCheckout: v }))}
              id="opt-no-checkout"
            />
            <div className="flex-1">
              <label htmlFor="opt-no-checkout" className="text-xs font-medium text-foreground cursor-pointer">
                No checkout
              </label>
              <p className="text-[10px] text-muted-foreground">
                Skip checking out files after clone (useful for scripted workflows)
              </p>
            </div>
          </div>

          {/* Recurse submodules */}
          <div className="flex items-start gap-3">
            <Checkbox
              checked={advanced.recurseSubmodules}
              onChange={(v) => setAdvanced((p) => ({ ...p, recurseSubmodules: v }))}
              id="opt-submodules"
            />
            <div className="flex-1">
              <label htmlFor="opt-submodules" className="text-xs font-medium text-foreground cursor-pointer">
                Recurse submodules
              </label>
              <p className="text-[10px] text-muted-foreground">
                Initialize and clone submodules recursively
              </p>
              {advanced.recurseSubmodules && (
                <div className="mt-1.5 flex items-center gap-3">
                  <Checkbox
                    checked={advanced.shallowSubmodules}
                    onChange={(v) => setAdvanced((p) => ({ ...p, shallowSubmodules: v }))}
                    id="opt-shallow-sub"
                  />
                  <label htmlFor="opt-shallow-sub" className="text-[10px] text-muted-foreground cursor-pointer">
                    Shallow submodules (depth 1)
                  </label>
                </div>
              )}
            </div>
          </div>

          {/* Parallel jobs */}
          <div className="flex items-center gap-3">
            <div className="w-4" /> {/* spacer for alignment */}
            <label className="text-xs text-muted-foreground">Parallel fetch jobs:</label>
            <Input
              type="number"
              min={0}
              max={32}
              className="w-16 text-xs"
              value={advanced.jobs}
              onChange={(e) =>
                setAdvanced((p) => ({ ...p, jobs: Math.max(0, parseInt(e.target.value) || 0) }))
              }
            />
            <span className="text-[10px] text-muted-foreground">(0 = auto)</span>
          </div>

          {/* Mirror / Bare */}
          <div className="flex gap-6">
            <div className="flex items-center gap-2">
              <Checkbox
                checked={advanced.mirror}
                onChange={(v) => setAdvanced((p) => ({ ...p, mirror: v, bare: v ? true : p.bare }))}
                id="opt-mirror"
              />
              <label htmlFor="opt-mirror" className="text-xs text-foreground cursor-pointer">
                Mirror
              </label>
            </div>
            <div className="flex items-center gap-2">
              <Checkbox
                checked={advanced.bare}
                onChange={(v) => setAdvanced((p) => ({ ...p, bare: v }))}
                id="opt-bare"
              />
              <label htmlFor="opt-bare" className="text-xs text-foreground cursor-pointer">
                Bare
              </label>
            </div>
          </div>

          {/* Reference repository */}
          <div>
            <label className="mb-1 block text-xs text-muted-foreground">
              Reference repository (optional)
            </label>
            <Input
              type="text"
              className="w-full text-xs"
              placeholder="/path/to/reference/repo.git"
              value={advanced.reference}
              onChange={(e) => setAdvanced((p) => ({ ...p, reference: e.target.value }))}
            />
            <p className="mt-0.5 text-[10px] text-muted-foreground">
              Use objects from a local reference repo to speed up clone (--reference)
            </p>
          </div>
        </div>
      )}
    </div>
  );
}

// ─── Reusable form primitives ───────────────────────────────────────

function Checkbox({
  checked,
  onChange,
  id,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  id: string;
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={checked}
      id={id}
      className={clsx(
        "mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded border transition",
        checked
          ? "border-primary bg-primary text-primary-foreground"
          : "border-border bg-background hover:border-primary/50",
      )}
      onClick={() => onChange(!checked)}
    >
      {checked && <Check size={10} strokeWidth={3} />}
    </button>
  );
}
