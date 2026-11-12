import { Button } from "@/components/ui/button";
import { ArrowDown, ArrowUp, GitBranch, Loader2, RefreshCw } from "lucide-react";
import { useCallback, useState } from "react";
import { executeCommand, type CommandContext } from "../lib/command-registry";

export interface StatusBarToolbarProps {
  repoId: string;
  currentBranch: string;
  ahead: number;
  behind: number;
  upstream: string | null;
  /** Called after a command finishes executing (success or failure). */
  onCommandExecuted?: (commandId: string) => void;
}

type SyncOp = "fetch" | "pull" | "push";

/**
 * Compact toolbar rendered in the StatusBar's right section.
 * Shows branch name with ahead/behind badges and Fetch/Pull/Push
 * icon buttons that execute through the CommandRegistry for
 * consistent toast feedback and operation history.
 */
export function StatusBarToolbar({
  repoId,
  currentBranch,
  ahead,
  behind,
  upstream,
  onCommandExecuted,
}: StatusBarToolbarProps) {
  const [inflight, setInflight] = useState<SyncOp | null>(null);

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

  const handleClick = useCallback(async (op: SyncOp) => {
    if (inflight) return;
    setInflight(op);
    try {
      const commandId = op === "fetch" ? "git.fetch" : op === "pull" ? "git.pull" : "git.push";
      await executeCommand(commandId, buildContext());
      onCommandExecuted?.(commandId);
    } finally {
      setInflight(null);
    }
  }, [inflight, buildContext, onCommandExecuted]);

  return (
    <div className="flex items-center gap-2 text-[11px]">
      {/* Branch name + ahead/behind badges */}
      <span className="flex items-center gap-1 text-foreground">
        <GitBranch size={11} />
        <span className="max-w-[120px] truncate">{currentBranch}</span>
        {ahead > 0 && (
          <span className="text-primary" title={`${ahead} commit${ahead === 1 ? "" : "s"} ahead`}>
            ↑{ahead}
          </span>
        )}
        {behind > 0 && (
          <span className="text-warning" title={`${behind} commit${behind === 1 ? "" : "s"} behind`}>
            ↓{behind}
          </span>
        )}
      </span>

      {/* Fetch button */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => handleClick("fetch")}
        disabled={inflight !== null}
        className="flex items-center justify-center rounded p-0.5 hover:text-foreground disabled:opacity-50 disabled:cursor-not-allowed"
        title="Fetch"
      >
        {inflight === "fetch" ? (
          <Loader2 size={12} className="animate-spin" />
        ) : (
          <RefreshCw size={12} />
        )}
      </Button>

      {/* Pull button */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => handleClick("pull")}
        disabled={inflight !== null}
        className="flex items-center justify-center rounded p-0.5 hover:text-foreground disabled:opacity-50 disabled:cursor-not-allowed"
        title="Pull (⌘⇧L)"
      >
        {inflight === "pull" ? (
          <Loader2 size={12} className="animate-spin" />
        ) : (
          <ArrowDown size={12} />
        )}
      </Button>

      {/* Push button */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => handleClick("push")}
        disabled={inflight !== null}
        className="flex items-center justify-center rounded p-0.5 hover:text-foreground disabled:opacity-50 disabled:cursor-not-allowed"
        title="Push (⌘⇧K)"
      >
        {inflight === "push" ? (
          <Loader2 size={12} className="animate-spin" />
        ) : (
          <ArrowUp size={12} />
        )}
      </Button>
    </div>
  );
}
