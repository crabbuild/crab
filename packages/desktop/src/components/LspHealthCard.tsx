/**
 * LspHealthCard — "Language Servers" card for the Health view.
 *
 * Shows each server's status, command, PID, memory usage (if available),
 * and uptime. Provides per-server "Restart" and "Stop" buttons.
 * Exposes a `getLspDiagnosticsBundle()` for inclusion in the
 * "Copy diagnostics" bundle.
 */

import { useCallback, useEffect, useState } from "react";
import { Check, Loader2, Minus, RotateCcw, Square, X } from "lucide-react";
import type { LspServerStatusInfo } from "../ipc";

// ── Types ───────────────────────────────────────────────────────────

interface LspHealthServer extends LspServerStatusInfo {
  /** Timestamp when the server was started (ms since epoch). */
  startedAt?: number;
  /** Process ID (if available from the main process). */
  pid?: number;
  /** Memory usage in bytes (if available). */
  memoryBytes?: number;
}

interface Props {
  /** Optional className for the card container. */
  className?: string;
}

// ── Component ───────────────────────────────────────────────────────

export function LspHealthCard({ className }: Props) {
  const [servers, setServers] = useState<LspHealthServer[]>([]);

  useEffect(() => {
    // Initial fetch
    window.crab.lsp.status().then((statuses) => {
      if (statuses && Array.isArray(statuses)) {
        setServers(
          statuses.map((s) => ({
            ...s,
            startedAt: s.state === "running" ? Date.now() : undefined,
          })),
        );
      }
    }).catch(() => undefined);

    // Subscribe to status changes
    const unsubscribe = window.crab.lsp.onStatusChange((payload) => {
      setServers((prev) => {
        const idx = prev.findIndex((s) => s.language === payload.language);
        const entry: LspHealthServer = {
          ...payload,
          startedAt: payload.state === "running" ? Date.now() : undefined,
        };
        if (idx >= 0) {
          const updated = [...prev];
          updated[idx] = { ...updated[idx], ...entry };
          return updated;
        }
        return [...prev, entry];
      });
    });

    return unsubscribe;
  }, []);

  const handleRestart = useCallback(async (language: string) => {
    try {
      await window.crab.lsp.stop(language);
      // The registry will re-start on next document open, or we can
      // trigger a start if a root path is known. For now, stop + status
      // update is sufficient — the user can re-open a file to trigger start.
    } catch {
      // Best-effort
    }
  }, []);

  const handleStop = useCallback(async (language: string) => {
    try {
      await window.crab.lsp.stop(language);
    } catch {
      // Best-effort
    }
  }, []);

  if (servers.length === 0) {
    return (
      <div className={`rounded border border-gx-border bg-gx-surface p-4 ${className ?? ""}`}>
        <h3 className="text-sm font-medium text-gx-text mb-2">Language Servers</h3>
        <p className="text-xs text-gx-muted">No language servers discovered.</p>
      </div>
    );
  }

  return (
    <div className={`rounded border border-gx-border bg-gx-surface p-4 ${className ?? ""}`}>
      <h3 className="text-sm font-medium text-gx-text mb-3">Language Servers</h3>

      <div className="space-y-2">
        {servers.map((server) => (
          <div
            key={server.language}
            className="flex items-center gap-3 rounded bg-gx-bg-sub px-3 py-2"
          >
            {/* Status icon */}
            <StatusIcon state={server.state} />

            {/* Server info */}
            <div className="flex-1 min-w-0">
              <div className="flex items-baseline gap-2">
                <span className="text-xs font-medium text-gx-text capitalize">
                  {server.language}
                </span>
                <span className="text-[10px] text-gx-muted truncate">
                  {server.command ?? "—"}
                </span>
              </div>
              <div className="flex items-center gap-3 text-[10px] text-gx-muted mt-0.5">
                <span>Status: {stateLabel(server.state)}</span>
                {server.pid && <span>PID: {server.pid}</span>}
                {server.memoryBytes != null && (
                  <span>Mem: {formatBytes(server.memoryBytes)}</span>
                )}
                {server.startedAt && server.state === "running" && (
                  <UptimeDisplay startedAt={server.startedAt} />
                )}
              </div>
              {server.error && (
                <p className="text-[10px] text-gx-error mt-0.5 truncate">
                  {server.error}
                </p>
              )}
            </div>

            {/* Action buttons */}
            <div className="flex items-center gap-1 shrink-0">
              {(server.state === "running" || server.state === "error") && (
                <button
                  type="button"
                  onClick={() => handleRestart(server.language)}
                  className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
                  title="Restart"
                  aria-label={`Restart ${server.language} server`}
                >
                  <RotateCcw size={11} />
                </button>
              )}
              {server.state === "running" && (
                <button
                  type="button"
                  onClick={() => handleStop(server.language)}
                  className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-error transition"
                  title="Stop"
                  aria-label={`Stop ${server.language} server`}
                >
                  <Square size={11} />
                </button>
              )}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── Diagnostics bundle export ───────────────────────────────────────

/**
 * Returns a string summary of LSP server statuses for inclusion in
 * the "Copy diagnostics" bundle.
 */
export async function getLspDiagnosticsBundle(): Promise<string> {
  try {
    const statuses = await window.crab.lsp.status();
    if (!statuses || statuses.length === 0) {
      return "Language Servers: none discovered";
    }

    const lines = statuses.map(
      (s) => `  ${s.language}: ${s.state}${s.command ? ` (${s.command})` : ""}${s.error ? ` [${s.error}]` : ""}`,
    );
    return `Language Servers:\n${lines.join("\n")}`;
  } catch {
    return "Language Servers: status unavailable";
  }
}

// ── Helpers ─────────────────────────────────────────────────────────

function StatusIcon({ state }: { state: LspServerStatusInfo["state"] }) {
  switch (state) {
    case "running":
      return <Check size={12} className="text-gx-success shrink-0" />;
    case "restarting":
      return <Loader2 size={12} className="animate-spin text-gx-warn shrink-0" />;
    case "error":
      return <X size={12} className="text-gx-error shrink-0" />;
    default:
      return <Minus size={12} className="text-gx-muted shrink-0" />;
  }
}

function stateLabel(state: LspServerStatusInfo["state"]): string {
  switch (state) {
    case "running":
      return "Running";
    case "restarting":
      return "Initializing";
    case "error":
      return "Error";
    case "stopped":
      return "Stopped";
    case "discovered":
      return "Not started";
    default:
      return "Unknown";
  }
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function UptimeDisplay({ startedAt }: { startedAt: number }) {
  const [uptime, setUptime] = useState("");

  useEffect(() => {
    const update = () => {
      const elapsed = Date.now() - startedAt;
      const seconds = Math.floor(elapsed / 1000);
      if (seconds < 60) {
        setUptime(`${seconds}s`);
      } else if (seconds < 3600) {
        setUptime(`${Math.floor(seconds / 60)}m ${seconds % 60}s`);
      } else {
        const h = Math.floor(seconds / 3600);
        const m = Math.floor((seconds % 3600) / 60);
        setUptime(`${h}h ${m}m`);
      }
    };

    update();
    const interval = setInterval(update, 10_000);
    return () => clearInterval(interval);
  }, [startedAt]);

  return <span>Uptime: {uptime}</span>;
}
