/**
 * LSP status indicator widget for the editor footer.
 *
 * Shows the active language server name + status icon:
 * - ✓ running
 * - ⟳ initializing
 * - ✗ error
 * - — not found (offers one-click install when supported)
 *
 * Clicking opens a popover with server details and actions:
 * - Restart button (error state)
 * - Install button (not-found state, when auto-install is supported)
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Check, Download, Loader2, X, Minus, RotateCcw } from "lucide-react";
import type { LspServerStatusInfo, LspInstallProgress } from "../ipc";

type ServerState = LspServerStatusInfo["state"];

interface LspStatusProps {
  /** The language of the currently active file. */
  language: string | null;
}

export function LspStatus({ language }: LspStatusProps) {
  const [servers, setServers] = useState<LspServerStatusInfo[]>([]);
  const [popoverOpen, setPopoverOpen] = useState(false);
  const [canInstall, setCanInstall] = useState(false);
  const [installProgress, setInstallProgress] = useState<LspInstallProgress | null>(null);
  const popoverRef = useRef<HTMLDivElement>(null);

  // Subscribe to status + install progress events
  useEffect(() => {
    const unsubscribeStatus = window.crab.lsp.onStatusChange((payload) => {
      setServers((prev) => {
        const idx = prev.findIndex((s) => s.language === payload.language);
        if (idx >= 0) {
          const updated = [...prev];
          updated[idx] = payload;
          return updated;
        }
        return [...prev, payload];
      });
    });

    const unsubscribeProgress = window.crab.lsp.onInstallProgress((payload) => {
      setInstallProgress(payload);
      if (payload.phase === "done" || payload.phase === "error") {
        // Clear the progress UI after a short delay
        setTimeout(() => setInstallProgress(null), 2000);
      }
    });

    // Initial status fetch
    window.crab.lsp.status().then((statuses) => {
      if (statuses && Array.isArray(statuses)) {
        setServers(statuses);
      }
    }).catch(() => undefined);

    return () => {
      unsubscribeStatus();
      unsubscribeProgress();
    };
  }, []);

  // Check install support for the active language when it changes
  useEffect(() => {
    if (!language || language === "plaintext") {
      setCanInstall(false);
      return;
    }
    window.crab.lsp.installSupported(language).then(setCanInstall).catch(() => setCanInstall(false));
  }, [language]);

  // Close popover on outside click
  useEffect(() => {
    if (!popoverOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (popoverRef.current && !popoverRef.current.contains(e.target as Node)) {
        setPopoverOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [popoverOpen]);

  const activeServer = language
    ? servers.find((s) => s.language === language)
    : null;

  const handleRestart = useCallback(async () => {
    if (!activeServer) return;
    try {
      await window.crab.lsp.stop(activeServer.language);
    } catch {
      // Best-effort
    }
  }, [activeServer]);

  const handleInstall = useCallback(async () => {
    if (!language) return;
    try {
      await window.crab.lsp.install(language);
      // After install, refresh discovery + status will update via events
    } catch (err) {
      // Progress handler already surfaces the error
      console.error("LSP install failed:", err);
    }
  }, [language]);

  if (!language || language === "plaintext") return null;

  const isInstalling = installProgress?.language === language
    && installProgress.phase !== "done"
    && installProgress.phase !== "error";
  const state: ServerState = isInstalling ? "restarting" : (activeServer?.state ?? "discovered");
  const serverName = activeServer?.command ?? language;
  const notFound = !activeServer || state === "discovered" || state === "stopped";

  return (
    <div className="relative" ref={popoverRef}>
      <button
        type="button"
        className="flex items-center gap-1 text-[11px] hover:text-gx-text"
        onClick={() => setPopoverOpen((o) => !o)}
        title={`LSP: ${serverName} (${state})`}
      >
        <StatusIcon state={state} />
        <span className="max-w-[120px] truncate">
          {isInstalling ? installLabel(installProgress) : serverName}
        </span>
      </button>

      {popoverOpen && (
        <div className="absolute bottom-full right-0 mb-1 w-64 rounded border border-gx-border bg-gx-surface p-3 shadow-lg text-xs z-50">
          <div className="mb-2 font-medium text-gx-text">Language Server</div>

          <div className="space-y-1 text-gx-muted">
            <Row label="Language" value={language} />
            <Row label="Command" value={activeServer?.command ?? "—"} />
            <Row label="Status" value={stateLabel(state)} />
            {activeServer?.error && (
              <div className="mt-1 text-gx-error text-[10px] break-all">
                {activeServer.error}
              </div>
            )}
          </div>

          {isInstalling && installProgress && (
            <div className="mt-2">
              <div className="text-[10px] text-gx-muted mb-1">
                {installLabel(installProgress)}
              </div>
              {installProgress.phase === "downloading" && installProgress.percent != null && (
                <div className="h-1 rounded bg-gx-bg-sub overflow-hidden">
                  <div
                    className="h-full bg-gx-accent transition-all"
                    style={{ width: `${Math.round((installProgress.percent ?? 0) * 100)}%` }}
                  />
                </div>
              )}
            </div>
          )}

          {state === "error" && !isInstalling && (
            <button
              type="button"
              onClick={handleRestart}
              className="mt-2 flex items-center gap-1 rounded bg-gx-accent/10 px-2 py-1 text-[11px] text-gx-accent hover:bg-gx-accent/20"
            >
              <RotateCcw size={10} />
              Restart
            </button>
          )}

          {notFound && canInstall && !isInstalling && (
            <button
              type="button"
              onClick={handleInstall}
              className="mt-2 flex items-center gap-1 rounded bg-gx-accent/10 px-2 py-1 text-[11px] text-gx-accent hover:bg-gx-accent/20"
            >
              <Download size={10} />
              Install {language} language server
            </button>
          )}
        </div>
      )}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function StatusIcon({ state }: { state: ServerState }) {
  switch (state) {
    case "running":
      return <Check size={11} className="text-gx-success" />;
    case "restarting":
      return <Loader2 size={11} className="animate-spin text-gx-warn" />;
    case "error":
      return <X size={11} className="text-gx-error" />;
    case "stopped":
    case "discovered":
    default:
      return <Minus size={11} className="text-gx-muted" />;
  }
}

function stateLabel(state: ServerState): string {
  switch (state) {
    case "running":
      return "Running";
    case "restarting":
      return "Initializing…";
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

function installLabel(progress: LspInstallProgress | null): string {
  if (!progress) return "Installing…";
  switch (progress.phase) {
    case "resolving":
      return "Resolving…";
    case "downloading":
      if (progress.percent != null) {
        return `Downloading ${Math.round(progress.percent * 100)}%`;
      }
      return "Downloading…";
    case "extracting":
      return "Installing…";
    case "done":
      return "Installed";
    case "error":
      return `Failed: ${progress.message ?? "unknown error"}`;
    default:
      return "Installing…";
  }
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between">
      <span>{label}</span>
      <span className="text-gx-text max-w-[140px] truncate">{value}</span>
    </div>
  );
}
