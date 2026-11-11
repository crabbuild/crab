import { NativeSelect } from "@/components/ui/native-select";
import { Input } from "@/components/ui/input";
/**
 * Tool Manager Panel — package manager detection, tool installation with
 * streaming output, status indicators, sudo confirmation, and error suggestions.
 */

import { useState, useCallback, useRef, useEffect } from "react";
import {
  Package,
  Play,
  Square,
  CheckCircle2,
  XCircle,
  Loader2,
  ShieldAlert,
  AlertTriangle,
  Terminal,
} from "lucide-react";

// ── Types ────────────────────────────────────────────────────────

type InstallStatus = "idle" | "running" | "succeeded" | "failed";

interface ToolManagerPanelProps {
  /** Detected package managers on the remote server */
  packageManagers: string[];
  /** Whether package manager detection is in progress */
  detecting?: boolean;
  /** Current installation stream output lines */
  outputLines: string[];
  /** Current installation status */
  installStatus: InstallStatus;
  /** Error message with suggested fix */
  errorMessage?: string;
  /** Suggested fix for the error */
  suggestedFix?: string;
  onInstall: (manager: string, tool: string, sudo: boolean) => void;
  onCancel: () => void;
}

// ── Sudo Confirmation Dialog ─────────────────────────────────────

function SudoConfirmDialog({
  tool,
  manager,
  onConfirm,
  onCancel,
}: {
  tool: string;
  manager: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="border border-warning border-warning rounded-md p-3 bg-warning/10 bg-warning/20 space-y-2">
      <div className="flex items-center gap-2">
        <ShieldAlert className="w-4 h-4 text-warning text-warning shrink-0" />
        <p className="text-xs font-medium text-warning text-warning">
          Elevated Privileges Required
        </p>
      </div>
      <p className="text-xs text-muted-foreground text-muted-foreground">
        Installing <span className="font-mono font-medium">{tool}</span> via{" "}
        <span className="font-mono">{manager}</span> requires sudo. The command will run with
        elevated privileges on the remote server.
      </p>
      <div className="flex justify-end gap-2 pt-1">
        <button
          onClick={onCancel}
          className="px-3 py-1 text-xs text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card rounded"
        >
          Cancel
        </button>
        <button
          onClick={onConfirm}
          className="px-3 py-1 text-xs bg-warning text-warning-foreground rounded hover:bg-warning"
        >
          Run with sudo
        </button>
      </div>
    </div>
  );
}

// ── Output Log ───────────────────────────────────────────────────

function OutputLog({ lines, maxLines = 1000 }: { lines: string[]; maxLines?: number }) {
  const logRef = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom on new output
  useEffect(() => {
    if (logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [lines]);

  const displayLines = lines.length > maxLines ? lines.slice(-maxLines) : lines;

  return (
    <div
      ref={logRef}
      className="bg-background rounded-md p-2 font-mono text-[11px] text-muted-foreground overflow-y-auto max-h-64 min-h-[100px]"
      role="log"
      aria-label="Installation output"
    >
      {displayLines.length === 0 ? (
        <span className="text-muted-foreground italic">Waiting for output...</span>
      ) : (
        displayLines.map((line, i) => (
          <div key={i} className="whitespace-pre-wrap break-all leading-relaxed">
            {line}
          </div>
        ))
      )}
    </div>
  );
}

// ── Status Indicator ─────────────────────────────────────────────

function StatusIndicator({ status }: { status: InstallStatus }) {
  switch (status) {
    case "running":
      return (
        <span className="flex items-center gap-1 text-xs text-info text-info">
          <Loader2 className="w-3.5 h-3.5 animate-spin" />
          Running
        </span>
      );
    case "succeeded":
      return (
        <span className="flex items-center gap-1 text-xs text-success text-success">
          <CheckCircle2 className="w-3.5 h-3.5" />
          Succeeded
        </span>
      );
    case "failed":
      return (
        <span className="flex items-center gap-1 text-xs text-destructive text-destructive">
          <XCircle className="w-3.5 h-3.5" />
          Failed
        </span>
      );
    default:
      return null;
  }
}

// ── Main Component ───────────────────────────────────────────────

export function ToolManagerPanel({
  packageManagers,
  detecting = false,
  outputLines,
  installStatus,
  errorMessage,
  suggestedFix,
  onInstall,
  onCancel,
}: ToolManagerPanelProps) {
  const [selectedManager, setSelectedManager] = useState("");
  const [toolName, setToolName] = useState("");
  const [useSudo, setUseSudo] = useState(false);
  const [showSudoConfirm, setShowSudoConfirm] = useState(false);

  const handleInstall = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!selectedManager || !toolName.trim()) return;

      if (useSudo) {
        setShowSudoConfirm(true);
      } else {
        onInstall(selectedManager, toolName.trim(), false);
      }
    },
    [selectedManager, toolName, useSudo, onInstall],
  );

  const handleSudoConfirm = useCallback(() => {
    setShowSudoConfirm(false);
    onInstall(selectedManager, toolName.trim(), true);
  }, [selectedManager, toolName, onInstall]);

  const isInstalling = installStatus === "running";

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Tool Manager">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-border border-border">
        <h3 className="text-xs font-semibold text-muted-foreground text-muted-foreground flex items-center gap-1.5">
          <Package className="w-4 h-4" />
          Tool Manager
        </h3>
        <StatusIndicator status={installStatus} />
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-2 space-y-3">
        {/* Package manager detection */}
        <section>
          <h4 className="text-xs font-medium text-muted-foreground text-muted-foreground mb-1.5">
            Package Managers
          </h4>
          {detecting ? (
            <div className="flex items-center gap-2 text-xs text-muted-foreground text-muted-foreground">
              <Loader2 className="w-3.5 h-3.5 animate-spin" />
              Detecting package managers...
            </div>
          ) : packageManagers.length === 0 ? (
            <p className="text-xs text-muted-foreground text-muted-foreground italic">
              No package managers detected.
            </p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {packageManagers.map((pm) => (
                <span
                  key={pm}
                  className="px-2 py-0.5 text-xs bg-card bg-card text-foreground text-muted-foreground rounded font-mono"
                >
                  {pm}
                </span>
              ))}
            </div>
          )}
        </section>

        {/* Install form */}
        <section>
          <h4 className="text-xs font-medium text-muted-foreground text-muted-foreground mb-1.5">
            Install Tool
          </h4>
          <form onSubmit={handleInstall} className="space-y-2">
            <div className="grid grid-cols-2 gap-2">
              <div>
                <label htmlFor="tool-manager-select" className="text-[10px] text-muted-foreground text-muted-foreground">
                  Package Manager
                </label>
                <NativeSelect
                  id="tool-manager-select"
                  value={selectedManager}
                  onChange={(e) => setSelectedManager(e.target.value)}
                  disabled={isInstalling || packageManagers.length === 0}
                  className="w-full px-2 py-1 text-xs border border-border border-border rounded bg-background bg-card disabled:opacity-50"
                >
                  <option value="">Select...</option>
                  {packageManagers.map((pm) => (
                    <option key={pm} value={pm}>
                      {pm}
                    </option>
                  ))}
                </NativeSelect>
              </div>
              <div>
                <label htmlFor="tool-name-input" className="text-[10px] text-muted-foreground text-muted-foreground">
                  Tool Name
                </label>
                <Input
                  id="tool-name-input"
                  type="text"
                  value={toolName}
                  onChange={(e) => setToolName(e.target.value)}
                  placeholder="e.g. ripgrep"
                  disabled={isInstalling}
                  className="w-full px-2 py-1 text-xs border border-border border-border rounded bg-background bg-card disabled:opacity-50"
                />
              </div>
            </div>

            <div className="flex items-center justify-between">
              <label className="flex items-center gap-1.5 text-xs text-muted-foreground text-muted-foreground cursor-pointer">
                <input
                  type="checkbox"
                  checked={useSudo}
                  onChange={(e) => setUseSudo(e.target.checked)}
                  disabled={isInstalling}
                  className="w-3 h-3 rounded border-border border-border text-warning"
                />
                Use sudo
              </label>

              <div className="flex items-center gap-2">
                {isInstalling && (
                  <button
                    type="button"
                    onClick={onCancel}
                    className="flex items-center gap-1 px-2 py-1 text-xs text-destructive text-destructive hover:bg-destructive/10 hover:bg-destructive/20 rounded"
                  >
                    <Square className="w-3 h-3" />
                    Cancel
                  </button>
                )}
                <button
                  type="submit"
                  disabled={isInstalling || !selectedManager || !toolName.trim()}
                  className="flex items-center gap-1 px-3 py-1 text-xs bg-info text-info-foreground rounded hover:bg-info disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  <Play className="w-3 h-3" />
                  Install
                </button>
              </div>
            </div>
          </form>
        </section>

        {/* Sudo confirmation */}
        {showSudoConfirm && (
          <SudoConfirmDialog
            tool={toolName}
            manager={selectedManager}
            onConfirm={handleSudoConfirm}
            onCancel={() => setShowSudoConfirm(false)}
          />
        )}

        {/* Output log */}
        {(isInstalling || outputLines.length > 0) && (
          <section>
            <h4 className="text-xs font-medium text-muted-foreground text-muted-foreground mb-1.5 flex items-center gap-1">
              <Terminal className="w-3.5 h-3.5" />
              Output
            </h4>
            <OutputLog lines={outputLines} />
          </section>
        )}

        {/* Error with suggested fix */}
        {installStatus === "failed" && errorMessage && (
          <section className="border border-destructive border-destructive rounded-md p-2 bg-destructive/10 bg-destructive/10 space-y-1.5">
            <div className="flex items-start gap-2">
              <AlertTriangle className="w-3.5 h-3.5 text-destructive shrink-0 mt-0.5" />
              <p className="text-xs text-destructive text-destructive">{errorMessage}</p>
            </div>
            {suggestedFix && (
              <div className="pl-5">
                <p className="text-[10px] text-muted-foreground text-muted-foreground font-medium">
                  Suggested fix:
                </p>
                <p className="text-xs text-foreground text-muted-foreground font-mono bg-card bg-card px-2 py-1 rounded mt-0.5">
                  {suggestedFix}
                </p>
              </div>
            )}
          </section>
        )}
      </div>
    </div>
  );
}
