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
    <div className="border border-amber-200 dark:border-amber-700 rounded-md p-3 bg-amber-50 dark:bg-amber-900/20 space-y-2">
      <div className="flex items-center gap-2">
        <ShieldAlert className="w-4 h-4 text-amber-600 dark:text-amber-400 shrink-0" />
        <p className="text-xs font-medium text-amber-700 dark:text-amber-300">
          Elevated Privileges Required
        </p>
      </div>
      <p className="text-xs text-gray-600 dark:text-gray-400">
        Installing <span className="font-mono font-medium">{tool}</span> via{" "}
        <span className="font-mono">{manager}</span> requires sudo. The command will run with
        elevated privileges on the remote server.
      </p>
      <div className="flex justify-end gap-2 pt-1">
        <button
          onClick={onCancel}
          className="px-3 py-1 text-xs text-gray-600 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-700 rounded"
        >
          Cancel
        </button>
        <button
          onClick={onConfirm}
          className="px-3 py-1 text-xs bg-amber-600 text-white rounded hover:bg-amber-700"
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
      className="bg-gray-900 rounded-md p-2 font-mono text-[11px] text-gray-300 overflow-y-auto max-h-64 min-h-[100px]"
      role="log"
      aria-label="Installation output"
    >
      {displayLines.length === 0 ? (
        <span className="text-gray-500 italic">Waiting for output...</span>
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
        <span className="flex items-center gap-1 text-xs text-blue-600 dark:text-blue-400">
          <Loader2 className="w-3.5 h-3.5 animate-spin" />
          Running
        </span>
      );
    case "succeeded":
      return (
        <span className="flex items-center gap-1 text-xs text-green-600 dark:text-green-400">
          <CheckCircle2 className="w-3.5 h-3.5" />
          Succeeded
        </span>
      );
    case "failed":
      return (
        <span className="flex items-center gap-1 text-xs text-red-600 dark:text-red-400">
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
      <div className="flex items-center justify-between px-3 py-2 border-b border-gray-100 dark:border-gray-700">
        <h3 className="text-xs font-semibold text-gray-600 dark:text-gray-300 flex items-center gap-1.5">
          <Package className="w-4 h-4" />
          Tool Manager
        </h3>
        <StatusIndicator status={installStatus} />
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-2 space-y-3">
        {/* Package manager detection */}
        <section>
          <h4 className="text-xs font-medium text-gray-600 dark:text-gray-300 mb-1.5">
            Package Managers
          </h4>
          {detecting ? (
            <div className="flex items-center gap-2 text-xs text-gray-500 dark:text-gray-400">
              <Loader2 className="w-3.5 h-3.5 animate-spin" />
              Detecting package managers...
            </div>
          ) : packageManagers.length === 0 ? (
            <p className="text-xs text-gray-500 dark:text-gray-400 italic">
              No package managers detected.
            </p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {packageManagers.map((pm) => (
                <span
                  key={pm}
                  className="px-2 py-0.5 text-xs bg-gray-100 dark:bg-gray-700 text-gray-700 dark:text-gray-300 rounded font-mono"
                >
                  {pm}
                </span>
              ))}
            </div>
          )}
        </section>

        {/* Install form */}
        <section>
          <h4 className="text-xs font-medium text-gray-600 dark:text-gray-300 mb-1.5">
            Install Tool
          </h4>
          <form onSubmit={handleInstall} className="space-y-2">
            <div className="grid grid-cols-2 gap-2">
              <div>
                <label htmlFor="tool-manager-select" className="text-[10px] text-gray-500 dark:text-gray-400">
                  Package Manager
                </label>
                <select
                  id="tool-manager-select"
                  value={selectedManager}
                  onChange={(e) => setSelectedManager(e.target.value)}
                  disabled={isInstalling || packageManagers.length === 0}
                  className="w-full px-2 py-1 text-xs border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800 disabled:opacity-50"
                >
                  <option value="">Select...</option>
                  {packageManagers.map((pm) => (
                    <option key={pm} value={pm}>
                      {pm}
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <label htmlFor="tool-name-input" className="text-[10px] text-gray-500 dark:text-gray-400">
                  Tool Name
                </label>
                <input
                  id="tool-name-input"
                  type="text"
                  value={toolName}
                  onChange={(e) => setToolName(e.target.value)}
                  placeholder="e.g. ripgrep"
                  disabled={isInstalling}
                  className="w-full px-2 py-1 text-xs border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800 disabled:opacity-50"
                />
              </div>
            </div>

            <div className="flex items-center justify-between">
              <label className="flex items-center gap-1.5 text-xs text-gray-600 dark:text-gray-400 cursor-pointer">
                <input
                  type="checkbox"
                  checked={useSudo}
                  onChange={(e) => setUseSudo(e.target.checked)}
                  disabled={isInstalling}
                  className="w-3 h-3 rounded border-gray-300 dark:border-gray-600 text-amber-600"
                />
                Use sudo
              </label>

              <div className="flex items-center gap-2">
                {isInstalling && (
                  <button
                    type="button"
                    onClick={onCancel}
                    className="flex items-center gap-1 px-2 py-1 text-xs text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/20 rounded"
                  >
                    <Square className="w-3 h-3" />
                    Cancel
                  </button>
                )}
                <button
                  type="submit"
                  disabled={isInstalling || !selectedManager || !toolName.trim()}
                  className="flex items-center gap-1 px-3 py-1 text-xs bg-blue-600 text-white rounded hover:bg-blue-700 disabled:opacity-50 disabled:cursor-not-allowed"
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
            <h4 className="text-xs font-medium text-gray-600 dark:text-gray-300 mb-1.5 flex items-center gap-1">
              <Terminal className="w-3.5 h-3.5" />
              Output
            </h4>
            <OutputLog lines={outputLines} />
          </section>
        )}

        {/* Error with suggested fix */}
        {installStatus === "failed" && errorMessage && (
          <section className="border border-red-200 dark:border-red-800 rounded-md p-2 bg-red-50 dark:bg-red-900/10 space-y-1.5">
            <div className="flex items-start gap-2">
              <AlertTriangle className="w-3.5 h-3.5 text-red-500 shrink-0 mt-0.5" />
              <p className="text-xs text-red-700 dark:text-red-300">{errorMessage}</p>
            </div>
            {suggestedFix && (
              <div className="pl-5">
                <p className="text-[10px] text-gray-600 dark:text-gray-400 font-medium">
                  Suggested fix:
                </p>
                <p className="text-xs text-gray-700 dark:text-gray-300 font-mono bg-gray-100 dark:bg-gray-800 px-2 py-1 rounded mt-0.5">
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
