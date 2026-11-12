import { Button } from "@/components/ui/button";
/**
 * Terminal crash recovery banner.
 *
 * Displayed when a PTY session exits unexpectedly (non-user-initiated).
 * Shows the exit code/signal and offers a "Restart Session" button.
 * Crashes are isolated to individual tabs — other tabs are unaffected.
 */

import { AlertTriangle, RotateCcw } from "lucide-react";

interface TerminalCrashBannerProps {
  /** The exit code from the crashed PTY. */
  exitCode: number;
  /** Optional signal name (e.g., "SIGKILL", "SIGSEGV"). */
  signal?: string;
  /** Callback to restart the session with the same configuration. */
  onRestart: () => void;
  /** Whether the restart is in progress. */
  restarting?: boolean;
}

/**
 * Format exit information for display.
 * Maps common exit codes and signals to human-readable descriptions.
 */
export function formatExitInfo(exitCode: number, signal?: string): string {
  if (signal) {
    return `Process terminated by signal: ${signal}`;
  }

  switch (exitCode) {
    case 1:
      return "Process exited with error (code 1)";
    case 2:
      return "Process interrupted (code 2)";
    case 126:
      return "Command not executable (code 126)";
    case 127:
      return "Command not found (code 127)";
    case 128:
      return "Invalid exit argument (code 128)";
    case 130:
      return "Process interrupted by Ctrl+C (code 130)";
    case 137:
      return "Process killed (SIGKILL, code 137)";
    case 139:
      return "Segmentation fault (SIGSEGV, code 139)";
    case 143:
      return "Process terminated (SIGTERM, code 143)";
    default:
      if (exitCode > 128 && exitCode < 192) {
        const sig = exitCode - 128;
        return `Process terminated by signal ${sig} (code ${exitCode})`;
      }
      return `Process exited with code ${exitCode}`;
  }
}

export function TerminalCrashBanner({
  exitCode,
  signal,
  onRestart,
  restarting = false,
}: TerminalCrashBannerProps) {
  const message = formatExitInfo(exitCode, signal);

  return (
    <div
      className="flex items-center gap-3 px-4 py-2 bg-destructive/10 border border-destructive/30 rounded mx-2 my-2"
      role="alert"
      data-testid="terminal-crash-banner"
    >
      <AlertTriangle className="w-4 h-4 text-destructive shrink-0" />
      <div className="flex-1 min-w-0">
        <p className="text-sm text-destructive font-medium">
          Terminal session ended unexpectedly
        </p>
        <p className="text-xs text-destructive/80 mt-0.5">{message}</p>
      </div>
      <Button
        variant="ghost"
        size="compact"
        onClick={onRestart}
        disabled={restarting}
        className="flex items-center gap-1.5 px-3 py-1.5 text-xs font-medium rounded bg-destructive/20 text-destructive hover:bg-destructive/30 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
      >
        <RotateCcw className={`w-3 h-3 ${restarting ? "animate-spin" : ""}`} />
        {restarting ? "Restarting..." : "Restart Session"}
      </Button>
    </div>
  );
}
