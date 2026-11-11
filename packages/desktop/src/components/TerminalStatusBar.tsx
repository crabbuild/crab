/**
 * Compact status bar displayed below the terminal viewport.
 * Shows shell name, cwd, running command duration, scrollback usage,
 * and connection status.
 */

import { useEffect, useState } from "react";
import { Folder, Terminal as TerminalIcon, Clock, Database, Wifi } from "lucide-react";
import type { ShellIntegrationState } from "../lib/terminal-shell-integration";
import { formatDuration } from "../lib/terminal-shell-integration";
import type { AgentType } from "../lib/terminal-ux-enhancements";
import { agentIcon } from "../lib/terminal-ux-enhancements";

interface Props {
  shellName: string;
  cwd: string | null;
  shellIntegration: ShellIntegrationState | null;
  scrollbackLines: number;
  maxScrollback: number;
  connected: boolean;
  agentType: AgentType | null;
  agentDuration: number | null;
  onCwdClick?: (cwd: string) => void;
  onShellClick?: () => void;
}

export function TerminalStatusBar({
  shellName,
  cwd,
  shellIntegration,
  scrollbackLines,
  maxScrollback,
  connected,
  agentType,
  agentDuration,
  onCwdClick,
  onShellClick,
}: Props) {
  const [runningDuration, setRunningDuration] = useState<number | null>(null);

  // Update running command duration every second.
  useEffect(() => {
    if (!shellIntegration?.commandStartedAt) {
      setRunningDuration(null);
      return;
    }
    const update = () => {
      if (shellIntegration.commandStartedAt) {
        setRunningDuration(Date.now() - shellIntegration.commandStartedAt);
      }
    };
    update();
    const interval = setInterval(update, 1000);
    return () => clearInterval(interval);
  }, [shellIntegration?.commandStartedAt]);

  const scrollbackPercent = maxScrollback > 0
    ? Math.min(100, Math.round((scrollbackLines / maxScrollback) * 100))
    : 0;

  const cwdDisplay = cwd
    ? cwd.split("/").filter(Boolean).slice(-2).join("/")
    : "—";

  return (
    <div className="flex shrink-0 items-center gap-3 border-t border-border bg-card px-2 py-0.5 text-[10px] text-muted-foreground">
      {/* Shell name */}
      <button
        type="button"
        onClick={onShellClick}
        className="flex items-center gap-1 hover:text-foreground transition-colors"
        title="Shell type (click to change)"
      >
        <TerminalIcon size={9} />
        <span>{shellName}</span>
      </button>

      {/* Agent indicator */}
      {agentType && (
        <span className="flex items-center gap-1 text-primary" title={`Agent: ${agentType}`}>
          <span>{agentIcon(agentType)}</span>
          {agentDuration !== null && (
            <span className="font-mono">{formatDuration(agentDuration)}</span>
          )}
        </span>
      )}

      {/* CWD */}
      <button
        type="button"
        onClick={() => cwd && onCwdClick?.(cwd)}
        className="flex items-center gap-1 hover:text-foreground transition-colors truncate max-w-[200px]"
        title={cwd || "Unknown directory"}
      >
        <Folder size={9} />
        <span className="truncate">{cwdDisplay}</span>
      </button>

      {/* Running command duration */}
      {runningDuration !== null && (
        <span className="flex items-center gap-1 text-warning" title="Command running">
          <Clock size={9} />
          <span className="font-mono">{formatDuration(runningDuration)}</span>
        </span>
      )}

      <div className="flex-1" />

      {/* Scrollback usage */}
      <span
        className="flex items-center gap-1"
        title={`Scrollback: ${scrollbackLines.toLocaleString()} / ${maxScrollback.toLocaleString()} lines`}
      >
        <Database size={9} />
        <span className="font-mono">{scrollbackPercent}%</span>
      </span>

      {/* Connection status */}
      <span
        className={`flex items-center gap-1 ${connected ? "text-success" : "text-destructive"}`}
        title={connected ? "Connected" : "Disconnected"}
      >
        <Wifi size={9} />
      </span>
    </div>
  );
}
