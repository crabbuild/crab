import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Slider } from "@/components/ui/slider";
/**
 * RemoteWorkspaceDialog — multi-step wizard for adding a remote
 * workspace source to the current workspace.
 *
 * Steps:
 *   1. Select SSH connection (from existing profiles)
 *   2. Browse remote directory (reuses RemoteFileBrowser in directory-only mode)
 *   3. Configure discovery (max depth, exclude patterns)
 *   4. Confirm and trigger discovery
 *
 * Displays error state with retry button if remote directory is
 * inaccessible or SSH connection fails. Shows loading spinner during
 * discovery scan.
 */

import clsx from "clsx";
import {
  AlertCircle,
  ArrowLeft,
  ArrowRight,
  Check,
  Cloud,
  Folder,
  FolderGit2,
  Loader2,
  RefreshCw,
  Server,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import type { SshConnectionState } from "../../state";
import type { SshProfile } from "../ssh/ssh-profile-types";

// ── Types ────────────────────────────────────────────────────────────

interface RemoteWorkspaceDialogProps {
  open: boolean;
  onClose: () => void;
  /** SSH connection states from the store. */
  sshConnections: Map<string, SshConnectionState>;
}

type WizardStep = "select-profile" | "browse-directory" | "configure" | "confirm";

interface DiscoveryConfig {
  maxDepth: number;
  excludePatterns: string[];
}

interface DiscoveredRepo {
  absolute_path: string;
  relative_path: string;
  name: string;
}

const DEFAULT_EXCLUDE_PATTERNS = ["node_modules", ".git", "vendor", "target", "build"];
const DEFAULT_MAX_DEPTH = 3;

// ── Main Component ───────────────────────────────────────────────────

export function RemoteWorkspaceDialog({
  open,
  onClose,
  sshConnections,
}: RemoteWorkspaceDialogProps) {
  const [step, setStep] = useState<WizardStep>("select-profile");
  const [profiles, setProfiles] = useState<SshProfile[]>([]);
  const [selectedProfile, setSelectedProfile] = useState<SshProfile | null>(null);
  const [remotePath, setRemotePath] = useState<string>("");
  const [config, setConfig] = useState<DiscoveryConfig>({
    maxDepth: DEFAULT_MAX_DEPTH,
    excludePatterns: [...DEFAULT_EXCLUDE_PATTERNS],
  });
  const [discovering, setDiscovering] = useState(false);
  const [discoveredRepos, setDiscoveredRepos] = useState<DiscoveredRepo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [excludeInput, setExcludeInput] = useState("");

  // Load SSH profiles on mount
  useEffect(() => {
    if (!open) return;
    loadProfiles();
  }, [open]);

  async function loadProfiles() {
    try {
      const result = await (window as any).crab.ssh.listProfiles();
      setProfiles(result ?? []);
    } catch {
      setProfiles([]);
    }
  }

  // Reset state when dialog closes
  useEffect(() => {
    if (!open) {
      setStep("select-profile");
      setSelectedProfile(null);
      setRemotePath("");
      setConfig({ maxDepth: DEFAULT_MAX_DEPTH, excludePatterns: [...DEFAULT_EXCLUDE_PATTERNS] });
      setDiscovering(false);
      setDiscoveredRepos([]);
      setError(null);
      setExcludeInput("");
    }
  }, [open]);

  const getConnectionStatus = useCallback(
    (profileId: string): SshConnectionState["status"] | undefined => {
      return sshConnections.get(profileId)?.status;
    },
    [sshConnections],
  );

  // Step navigation
  const canGoNext = useMemo(() => {
    switch (step) {
      case "select-profile":
        return selectedProfile !== null;
      case "browse-directory":
        return remotePath.trim().length > 0;
      case "configure":
        return config.maxDepth >= 1;
      case "confirm":
        return !discovering;
      default:
        return false;
    }
  }, [step, selectedProfile, remotePath, config, discovering]);

  function handleNext() {
    switch (step) {
      case "select-profile":
        // Ensure the SSH profile is connected before proceeding to browse.
        // If not connected, attempt to connect first.
        if (selectedProfile) {
          const status = getConnectionStatus(selectedProfile.id);
          if (status !== "connected") {
            setError(null);
            (async () => {
              try {
                await (window as any).crab.ssh.connect(selectedProfile.id);
                setStep("browse-directory");
              } catch (err) {
                setError(
                  `Failed to connect to ${selectedProfile.label}: ${err instanceof Error ? err.message : String(err)}`,
                );
              }
            })();
          } else {
            setStep("browse-directory");
          }
        }
        break;
      case "browse-directory":
        setStep("configure");
        break;
      case "configure":
        setStep("confirm");
        triggerDiscovery();
        break;
    }
  }

  function handleBack() {
    setError(null);
    switch (step) {
      case "browse-directory":
        setStep("select-profile");
        break;
      case "configure":
        setStep("browse-directory");
        break;
      case "confirm":
        setStep("configure");
        setDiscoveredRepos([]);
        setDiscovering(false);
        break;
    }
  }

  async function triggerDiscovery() {
    if (!selectedProfile) return;
    setDiscovering(true);
    setError(null);
    setDiscoveredRepos([]);
    try {
      const result = await (window as any).crab.remoteWorkspace.discover(
        selectedProfile.id,
        remotePath,
        { maxDepth: config.maxDepth, excludePatterns: config.excludePatterns },
      );
      if (result && !result.ok) {
        // IPC returned an error (e.g., agent doesn't support discover_repos,
        // SSH connection lost, or remote directory inaccessible).
        setError(result.error ?? "Discovery failed. Make sure the remote agent is up to date (rebuild and redeploy).");
      } else {
        setDiscoveredRepos(result?.repos ?? []);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setDiscovering(false);
    }
  }

  async function handleConfirm() {
    if (!selectedProfile) return;
    setError(null);
    try {
      // Open the remote workspace directly (no hybrid source persistence)
      await (window as any).crab.remoteWorkspace.open(
        selectedProfile.id,
        remotePath,
        { maxDepth: config.maxDepth, excludePatterns: config.excludePatterns },
      );
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  function handleAddExcludePattern() {
    const trimmed = excludeInput.trim();
    if (trimmed && !config.excludePatterns.includes(trimmed)) {
      setConfig((prev) => ({
        ...prev,
        excludePatterns: [...prev.excludePatterns, trimmed],
      }));
    }
    setExcludeInput("");
  }

  function handleRemoveExcludePattern(pattern: string) {
    setConfig((prev) => ({
      ...prev,
      excludePatterns: prev.excludePatterns.filter((p) => p !== pattern),
    }));
  }

  if (!open) return null;

  const stepIndex = ["select-profile", "browse-directory", "configure", "confirm"].indexOf(step);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 flex max-h-[85vh] w-[min(560px,92vw)] flex-col rounded-xl border border-border bg-card shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-5 py-4">
          <div className="flex items-center gap-2">
            <Cloud size={18} className="text-primary" />
            <DialogTitle className="text-base font-semibold">
              Connect to Remote Workspace
            </DialogTitle>
          </div>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={18} />
          </Button>
        </div>

        {/* Step indicator */}
        <StepIndicator currentStep={stepIndex} />

        {/* Body */}
        <div className="flex-1 overflow-y-auto px-5 py-4 min-h-[300px]">
          {step === "select-profile" && (
            <SelectProfileStep
              profiles={profiles}
              selectedProfile={selectedProfile}
              onSelect={setSelectedProfile}
              getConnectionStatus={getConnectionStatus}
            />
          )}

          {step === "browse-directory" && selectedProfile && (
            <BrowseDirectoryStep
              profileId={selectedProfile.id}
              remotePath={remotePath}
              onPathChange={setRemotePath}
              error={error}
              onRetry={() => setError(null)}
            />
          )}

          {step === "configure" && (
            <ConfigureStep
              config={config}
              onConfigChange={setConfig}
              excludeInput={excludeInput}
              onExcludeInputChange={setExcludeInput}
              onAddPattern={handleAddExcludePattern}
              onRemovePattern={handleRemoveExcludePattern}
            />
          )}

          {step === "confirm" && (
            <ConfirmStep
              profileLabel={selectedProfile?.label ?? ""}
              remotePath={remotePath}
              config={config}
              discovering={discovering}
              discoveredRepos={discoveredRepos}
              error={error}
              onRetry={triggerDiscovery}
            />
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between border-t border-border px-5 py-3">
          <div>
            {step !== "select-profile" && (
              <Button variant="outline"
                type="button"
                className="flex items-center gap-1"
                onClick={handleBack}
                disabled={discovering}
              >
                <ArrowLeft size={14} />
                Back
              </Button>
            )}
          </div>
          <div className="flex items-center gap-2">
            <Button variant="outline" type="button" onClick={onClose} disabled={discovering}>
              Cancel
            </Button>
            {step !== "confirm" ? (
              <Button variant="default"
                type="button"
                className="flex items-center gap-1"
                onClick={handleNext}
                disabled={!canGoNext}
              >
                Next
                <ArrowRight size={14} />
              </Button>
            ) : (
              <Button variant="default"
                type="button"
                className="flex items-center gap-1"
                onClick={handleConfirm}
                disabled={discovering || (discoveredRepos.length === 0 && !error)}
              >
                <Check size={14} />
                Add to Workspace
              </Button>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── Step Indicator ───────────────────────────────────────────────────

function StepIndicator({ currentStep }: { currentStep: number }) {
  const steps = ["Connection", "Directory", "Configure", "Discover"];

  return (
    <div className="flex items-center gap-1 px-5 py-2 border-b border-border bg-background">
      {steps.map((label, i) => (
        <div key={label} className="flex items-center gap-1 flex-1">
          <div
            className={clsx(
              "flex items-center justify-center w-5 h-5 rounded-full text-[10px] font-bold shrink-0",
              i < currentStep && "bg-primary text-primary-foreground",
              i === currentStep && "bg-primary/20 text-primary ring-1 ring-primary",
              i > currentStep && "bg-border text-muted-foreground",
            )}
          >
            {i < currentStep ? <Check size={10} /> : i + 1}
          </div>
          <span
            className={clsx(
              "text-[10px] truncate",
              i <= currentStep ? "text-foreground font-medium" : "text-muted-foreground",
            )}
          >
            {label}
          </span>
          {i < steps.length - 1 && (
            <div
              className={clsx(
                "flex-1 h-px mx-1",
                i < currentStep ? "bg-primary" : "bg-border",
              )}
            />
          )}
        </div>
      ))}
    </div>
  );
}

// ── Step 1: Select SSH Profile ───────────────────────────────────────

interface SelectProfileStepProps {
  profiles: SshProfile[];
  selectedProfile: SshProfile | null;
  onSelect: (profile: SshProfile) => void;
  getConnectionStatus: (profileId: string) => SshConnectionState["status"] | undefined;
}

function SelectProfileStep({
  profiles,
  selectedProfile,
  onSelect,
  getConnectionStatus,
}: SelectProfileStepProps) {
  if (profiles.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-12 gap-3">
        <Server size={32} className="text-muted-foreground/50" />
        <p className="text-sm text-muted-foreground text-center">
          No SSH connections configured.
        </p>
        <p className="text-xs text-muted-foreground/70 text-center max-w-xs">
          Add an SSH connection in the SSH view first, then come back here to
          add a remote workspace.
        </p>
      </div>
    );
  }

  return (
    <div>
      <p className="text-xs text-muted-foreground mb-3">
        Select an SSH connection to use for the remote workspace.
      </p>
      <div className="space-y-1.5 max-h-[320px] overflow-y-auto">
        {profiles.map((profile) => {
          const status = getConnectionStatus(profile.id);
          const isSelected = selectedProfile?.id === profile.id;

          return (
            <Button
              variant="ghost"
              size="compact"
              key={profile.id}
              type="button"
              className={clsx(
                "flex w-full items-center gap-3 rounded-lg border px-3 py-2.5 text-left transition",
                isSelected
                  ? "border-primary bg-primary/10"
                  : "border-border hover:border-primary/50 hover:bg-accent",
              )}
              onClick={() => onSelect(profile)}
            >
              {/* Colour dot */}
              <span
                className="w-2.5 h-2.5 rounded-full shrink-0"
                style={{ backgroundColor: profile.colour ?? "rgb(var(--muted-foreground))" }}
              />
              {/* Profile info */}
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium text-foreground truncate">
                    {profile.label}
                  </span>
                  <ConnectionStatusBadge status={status} />
                </div>
                <span className="text-[11px] text-muted-foreground truncate block">
                  {profile.username}@{profile.host}:{profile.port}
                </span>
              </div>
              {/* Selection indicator */}
              {isSelected && (
                <Check size={16} className="shrink-0 text-primary" />
              )}
            </Button>
          );
        })}
      </div>
    </div>
  );
}

function ConnectionStatusBadge({ status }: { status: SshConnectionState["status"] | undefined }) {
  if (!status || status === "disconnected") {
    return (
      <span className="inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[9px] bg-border/50 text-muted-foreground">
        offline
      </span>
    );
  }
  if (status === "connected") {
    return (
      <span className="inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[9px] bg-success/20 text-success">
        connected
      </span>
    );
  }
  if (status === "connecting" || status === "deploying" || status === "reconnecting") {
    return (
      <span className="inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[9px] bg-warning/20 text-warning">
        {status}
      </span>
    );
  }
  if (status === "error") {
    return (
      <span className="inline-flex items-center gap-0.5 rounded px-1 py-0.5 text-[9px] bg-destructive/20 text-destructive">
        error
      </span>
    );
  }
  return null;
}

// ── Step 2: Browse Remote Directory ──────────────────────────────────

interface BrowseDirectoryStepProps {
  profileId: string;
  remotePath: string;
  onPathChange: (path: string) => void;
  error: string | null;
  onRetry: () => void;
}

function BrowseDirectoryStep({
  profileId,
  remotePath,
  onPathChange,
  error,
  onRetry,
}: BrowseDirectoryStepProps) {
  const [browseError, setBrowseError] = useState<string | null>(null);
  const [entries, setEntries] = useState<Array<{ name: string; path: string; kind: "file" | "dir" }>>([]);
  const [currentBrowsePath, setCurrentBrowsePath] = useState<string>("/");
  const [loading, setLoading] = useState(false);

  // Load initial directory (home dir)
  useEffect(() => {
    loadDirectory();
  }, [profileId]);

  async function loadDirectory(path?: string) {
    setLoading(true);
    setBrowseError(null);
    try {
      const targetPath = path ?? currentBrowsePath;
      const result = await (window as any).crab.ssh.browsePath(profileId, targetPath);
      const dirs = (result.entries as Array<{ name: string; path: string; kind: string }>)
        .filter((e) => e.kind === "dir")
        .sort((a, b) => a.name.localeCompare(b.name));
      setEntries(dirs as Array<{ name: string; path: string; kind: "file" | "dir" }>);
      setCurrentBrowsePath(result.path);
      if (!remotePath) {
        onPathChange(result.path);
      }
    } catch (err) {
      setBrowseError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }

  // Try to load home directory on first mount
  useEffect(() => {
    (async () => {
      try {
        const result = await (window as any).crab.ssh.homeDir(profileId);
        const homePath = (result as { path: string }).path ?? "/home";
        setCurrentBrowsePath(homePath);
        onPathChange(homePath);
        loadDirectory(homePath);
      } catch {
        loadDirectory("/");
      }
    })();
  }, [profileId]);

  function handleNavigate(path: string) {
    setCurrentBrowsePath(path);
    onPathChange(path);
    loadDirectory(path);
  }

  // Breadcrumb segments
  const segments = useMemo(() => {
    const parts = currentBrowsePath.split("/").filter(Boolean);
    const segs: { label: string; path: string }[] = [{ label: "/", path: "/" }];
    let current = "";
    for (const part of parts) {
      current += "/" + part;
      segs.push({ label: part, path: current });
    }
    return segs;
  }, [currentBrowsePath]);

  const displayError = browseError || error;

  return (
    <div className="flex flex-col gap-3">
      <p className="text-xs text-muted-foreground">
        Select the remote directory to scan for git repositories.
      </p>

      {/* Manual path input */}
      <div>
        <label className="text-[11px] text-muted-foreground block mb-1">
          Remote path
        </label>
        <div className="flex gap-2">
          <Input
            className="flex-1 font-mono text-sm"
            value={remotePath}
            onChange={(e) => onPathChange(e.target.value)}
            placeholder="/home/user/projects"
          />
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => loadDirectory(remotePath)}
            title="Navigate to path"
          >
            Go
          </Button>
        </div>
      </div>

      {/* Breadcrumb */}
      <div className="flex items-center gap-0.5 text-[11px] text-muted-foreground overflow-x-auto scrollbar-none">
        {segments.map((seg, i) => (
          <span key={seg.path} className="flex items-center gap-0.5 shrink-0">
            {i > 0 && <span className="text-muted-foreground/50">/</span>}
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => handleNavigate(seg.path)}
              className={clsx(
                "px-1 py-0.5 rounded hover:bg-accent hover:text-foreground transition-colors",
                seg.path === currentBrowsePath ? "font-medium text-foreground" : "text-muted-foreground",
              )}
            >
              {seg.label}
            </Button>
          </span>
        ))}
      </div>

      {/* Error state */}
      {displayError && !loading && (
        <div className="flex flex-col items-center justify-center py-6 gap-3 rounded-lg border border-destructive/30 bg-destructive/5">
          <AlertCircle size={24} className="text-destructive" />
          <p className="text-xs text-destructive text-center max-w-xs">
            {displayError}
          </p>
          <Button variant="outline"
            type="button"
            className="text-xs flex items-center gap-1"
            onClick={() => {
              onRetry();
              setBrowseError(null);
              loadDirectory(currentBrowsePath);
            }}
          >
            <RefreshCw size={12} />
            Retry
          </Button>
        </div>
      )}

      {/* Loading state */}
      {loading && (
        <div className="flex items-center justify-center py-8 gap-2">
          <Loader2 size={16} className="animate-spin text-primary" />
          <span className="text-xs text-muted-foreground">
            Loading directory...
          </span>
        </div>
      )}

      {/* Directory listing (directories only) */}
      {!loading && !displayError && (
        <div className="border border-border rounded-lg max-h-[200px] overflow-y-auto">
          {entries.length === 0 ? (
            <div className="flex items-center justify-center py-6 text-xs text-muted-foreground">
              No subdirectories found
            </div>
          ) : (
            <div className="divide-y divide-border/30">
              {entries.map((entry) => (
                <Button
                  variant="ghost"
                  size="compact"
                  key={entry.path}
                  type="button"
                  className={clsx(
                    "flex items-center gap-2 w-full px-3 py-2 text-left text-xs",
                    "hover:bg-accent transition-colors",
                  )}
                  onClick={() => handleNavigate(entry.path)}
                >
                  <Folder size={14} className="shrink-0 text-primary" />
                  <span className="truncate text-foreground">{entry.name}</span>
                </Button>
              ))}
            </div>
          )}
        </div>
      )}

      {/* Selected path confirmation */}
      {remotePath && !displayError && (
        <div className="flex items-center gap-2 px-3 py-2 rounded-lg bg-primary/10 border border-primary/20">
          <Folder size={14} className="text-primary shrink-0" />
          <span className="text-xs text-primary font-medium truncate">
            Selected: {remotePath}
          </span>
        </div>
      )}
    </div>
  );
}

// ── Step 3: Configure Discovery ──────────────────────────────────────

interface ConfigureStepProps {
  config: DiscoveryConfig;
  onConfigChange: (config: DiscoveryConfig) => void;
  excludeInput: string;
  onExcludeInputChange: (value: string) => void;
  onAddPattern: () => void;
  onRemovePattern: (pattern: string) => void;
}

function ConfigureStep({
  config,
  onConfigChange,
  excludeInput,
  onExcludeInputChange,
  onAddPattern,
  onRemovePattern,
}: ConfigureStepProps) {
  return (
    <div className="flex flex-col gap-4">
      <p className="text-xs text-muted-foreground">
        Configure how deep to scan for git repositories and which directories to
        skip.
      </p>

      {/* Max depth */}
      <div>
        <label className="text-[11px] text-muted-foreground block mb-1">
          Maximum scan depth
        </label>
        <div className="flex items-center gap-3">
          <Slider
            aria-label="Maximum scan depth"
            min={1}
            max={10}
            value={[Number(config.maxDepth)]}
            onValueChange={([value]) =>
              onConfigChange({
                ...config,
                maxDepth: parseInt(String(value), 10),
              })
            }
            className="flex-1 accent-primary"
          />
          <span className="text-sm font-mono text-foreground w-6 text-center">
            {config.maxDepth}
          </span>
        </div>
        <p className="text-[10px] text-muted-foreground mt-1">
          How many directory levels deep to scan for .git directories.
        </p>
      </div>

      {/* Exclude patterns */}
      <div>
        <label className="text-[11px] text-muted-foreground block mb-1">
          Exclude patterns
        </label>
        <div className="flex gap-2 mb-2">
          <Input
            className="flex-1 text-xs font-mono"
            value={excludeInput}
            onChange={(e) => onExcludeInputChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                onAddPattern();
              }
            }}
            placeholder="e.g. .cache, dist"
          />
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={onAddPattern}
            disabled={!excludeInput.trim()}
          >
            Add
          </Button>
        </div>
        <div className="flex flex-wrap gap-1.5">
          {config.excludePatterns.map((pattern) => (
            <span
              key={pattern}
              className="inline-flex items-center gap-1 rounded bg-background border border-border px-2 py-0.5 text-[11px] text-muted-foreground font-mono"
            >
              {pattern}
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="text-muted-foreground hover:text-destructive transition-colors"
                onClick={() => onRemovePattern(pattern)}
                aria-label={`Remove ${pattern}`}
              >
                <X size={10} />
              </Button>
            </span>
          ))}
        </div>
        <p className="text-[10px] text-muted-foreground mt-1.5">
          Directories matching these patterns will be skipped during discovery.
        </p>
      </div>
    </div>
  );
}

// ── Step 4: Confirm & Discover ───────────────────────────────────────

interface ConfirmStepProps {
  profileLabel: string;
  remotePath: string;
  config: DiscoveryConfig;
  discovering: boolean;
  discoveredRepos: DiscoveredRepo[];
  error: string | null;
  onRetry: () => void;
}

function ConfirmStep({
  profileLabel,
  remotePath,
  config,
  discovering,
  discoveredRepos,
  error,
  onRetry,
}: ConfirmStepProps) {
  return (
    <div className="flex flex-col gap-4">
      {/* Summary */}
      <div className="rounded-lg border border-border bg-background p-3 space-y-2">
        <div className="flex items-center gap-2 text-xs">
          <Server size={12} className="text-muted-foreground shrink-0" />
          <span className="text-muted-foreground">Connection:</span>
          <span className="font-medium text-foreground">{profileLabel}</span>
        </div>
        <div className="flex items-center gap-2 text-xs">
          <Folder size={12} className="text-muted-foreground shrink-0" />
          <span className="text-muted-foreground">Path:</span>
          <span className="font-medium text-foreground font-mono truncate">
            {remotePath}
          </span>
        </div>
        <div className="flex items-center gap-2 text-xs">
          <span className="text-muted-foreground ml-5">
            Depth: {config.maxDepth} | Excludes: {config.excludePatterns.length}
          </span>
        </div>
      </div>

      {/* Discovery loading state */}
      {discovering && (
        <div className="flex flex-col items-center justify-center py-8 gap-3">
          <Loader2 size={24} className="animate-spin text-primary" />
          <p className="text-sm text-muted-foreground">
            Scanning for repositories...
          </p>
          <p className="text-[10px] text-muted-foreground/70">
            Searching up to {config.maxDepth} levels deep in {remotePath}
          </p>
        </div>
      )}

      {/* Error state */}
      {error && !discovering && (
        <div className="flex flex-col items-center justify-center py-6 gap-3 rounded-lg border border-destructive/30 bg-destructive/5">
          <AlertCircle size={24} className="text-destructive" />
          <p className="text-xs text-destructive text-center max-w-xs">
            {error}
          </p>
          <Button variant="outline"
            type="button"
            className="text-xs flex items-center gap-1"
            onClick={onRetry}
          >
            <RefreshCw size={12} />
            Retry
          </Button>
        </div>
      )}

      {/* Discovery results */}
      {!discovering && !error && discoveredRepos.length > 0 && (
        <div>
          <div className="flex items-center gap-2 mb-2">
            <FolderGit2 size={14} className="text-primary" />
            <span className="text-xs font-medium text-foreground">
              Found {discoveredRepos.length} repositor
              {discoveredRepos.length === 1 ? "y" : "ies"}
            </span>
          </div>
          <div className="border border-border rounded-lg max-h-[200px] overflow-y-auto divide-y divide-border/30">
            {discoveredRepos.map((repo) => (
              <div
                key={repo.absolute_path}
                className="flex items-center gap-2 px-3 py-2 text-xs"
              >
                <FolderGit2 size={12} className="shrink-0 text-primary" />
                <span className="font-medium text-foreground">{repo.name}</span>
                <span className="text-muted-foreground truncate ml-auto font-mono text-[10px]">
                  {repo.relative_path}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* No repos found */}
      {!discovering && !error && discoveredRepos.length === 0 && (
        <div className="flex flex-col items-center justify-center py-8 gap-2">
          <Folder size={24} className="text-muted-foreground/50" />
          <p className="text-sm text-muted-foreground">No repositories found</p>
          <p className="text-[10px] text-muted-foreground/70 text-center max-w-xs">
            Try increasing the scan depth or choosing a different directory.
          </p>
          <Button variant="outline"
            type="button"
            className="text-xs flex items-center gap-1 mt-2"
            onClick={onRetry}
          >
            <RefreshCw size={12} />
            Scan Again
          </Button>
        </div>
      )}
    </div>
  );
}
