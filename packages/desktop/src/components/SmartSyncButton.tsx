import { useState, useCallback, useMemo } from "react";
import { ArrowDown, ArrowUp, Loader2, RefreshCw, Upload } from "lucide-react";
import { executeCommand, type CommandContext } from "../lib/command-registry";

export interface SmartSyncButtonProps {
  repoId: string;
  currentBranch: string;
  upstream: string | null;
  ahead: number;
  behind: number;
}

type SyncMode = "publish" | "push" | "pull" | "fetch";

interface ModeConfig {
  label: string;
  icon: typeof Upload;
  commandId: string;
}

const MODE_CONFIG: Record<SyncMode, ModeConfig> = {
  publish: { label: "Publish Branch", icon: Upload, commandId: "git.push" },
  push: { label: "Push", icon: ArrowUp, commandId: "git.push" },
  pull: { label: "Pull", icon: ArrowDown, commandId: "git.pull" },
  fetch: { label: "Fetch", icon: RefreshCw, commandId: "git.fetch" },
};

function deriveMode(upstream: string | null, ahead: number, behind: number): SyncMode {
  if (upstream === null) return "publish";
  if (ahead > 0) return "push";
  if (behind > 0) return "pull";
  return "fetch";
}

/**
 * Smart sync button that adapts its label and behavior based on branch state.
 * Shows the most useful next action: Publish → Push → Pull → Fetch.
 * Executes through the CommandRegistry for consistent toast feedback and
 * operation history.
 */
export function SmartSyncButton({
  repoId,
  currentBranch,
  upstream,
  ahead,
  behind,
}: SmartSyncButtonProps) {
  const [loading, setLoading] = useState(false);

  const mode = useMemo(
    () => deriveMode(upstream, ahead, behind),
    [upstream, ahead, behind],
  );

  const config = MODE_CONFIG[mode];
  const Icon = config.icon;

  const buildContext = useCallback((): CommandContext => ({
    repoId,
    currentBranch,
    upstream,
    ahead,
    behind,
    dirtyCount: 0,
    hasStagedChanges: false,
    activeOperation: null,
    forgeProvider: null,
  }), [repoId, currentBranch, upstream, ahead, behind]);

  const handleClick = useCallback(async () => {
    if (loading) return;
    setLoading(true);
    try {
      await executeCommand(config.commandId, buildContext());
    } finally {
      setLoading(false);
    }
  }, [loading, config.commandId, buildContext]);

  return (
    <button
      type="button"
      onClick={handleClick}
      disabled={loading}
      className="flex items-center gap-1.5 rounded px-2 py-0.5 text-[11px] text-foreground hover:bg-accent disabled:opacity-50 disabled:cursor-not-allowed"
      title={config.label}
    >
      {loading ? (
        <Loader2 size={12} className="animate-spin" />
      ) : (
        <Icon size={12} />
      )}
      <span>{config.label}</span>
    </button>
  );
}
