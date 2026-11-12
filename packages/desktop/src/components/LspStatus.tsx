import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
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

import { Check, Download, Loader2, Minus, RotateCcw, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { LspInstallProgress, LspServerStatusInfo } from "../ipc";

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
      <Popover open={popoverOpen} onOpenChange={setPopoverOpen}>
        <PopoverTrigger asChild>
          <Button
            variant="ghost"
            size="compact"
            type="button"
        className="flex items-center gap-1 text-[11px] hover:text-foreground"
        onClick={() => setPopoverOpen((o) => !o)}
        title={`LSP: ${serverName} (${state})`}
      >
        <StatusIcon state={state} />
        <span className="max-w-[120px] truncate">
          {isInstalling ? installLabel(installProgress) : serverName}
        </span>
      </Button>
        </PopoverTrigger>

      {popoverOpen && (
        <PopoverContent
            side="top"
            align="end"
            className=" w-64 rounded border border-border bg-card p-3 text-xs "
          >
          <div className="mb-2 font-medium text-foreground">
              Language Server
            </div>

          <div className="space-y-1 text-muted-foreground">
            <Row label="Language" value={language} />
            <Row label="Command" value={activeServer?.command ?? "—"} />
            <Row label="Status" value={stateLabel(state)} />
            {activeServer?.error && (
              <div className="mt-1 text-destructive text-[10px] break-all">
                {activeServer.error}
              </div>
            )}
          </div>

          {isInstalling && installProgress && (
            <div className="mt-2">
              <div className="text-[10px] text-muted-foreground mb-1">
                {installLabel(installProgress)}
              </div>
              {installProgress.phase === "downloading" && installProgress.percent != null && (
                <div className="h-1 rounded bg-background overflow-hidden">
                  <div
                    className="h-full bg-primary transition-all"
                    style={{ width: `${Math.round((installProgress.percent ?? 0) * 100)}%` }}
                  />
                </div>
              )}
            </div>
          )}

          {state === "error" && !isInstalling && (
            <Button
                onSelect={handleRestart}
              className="mt-2 flex items-center gap-1 rounded bg-primary/10 px-2 py-1 text-[11px] text-primary hover:bg-primary/20"
            >
              <RotateCcw size={10} />
                Restart
              </Button>
          )}

          {notFound && canInstall && !isInstalling && (
            <Button
                onSelect={handleInstall}
              className="mt-2 flex items-center gap-1 rounded bg-primary/10 px-2 py-1 text-[11px] text-primary hover:bg-primary/20"
            >
              <Download size={10} />
                Install {language} language server
              </Button>
            )}
          </PopoverContent>
        )}
      </Popover>
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function StatusIcon({ state }: { state: ServerState }) {
  switch (state) {
    case "running":
      return <Check size={11} className="text-success" />;
    case "restarting":
      return <Loader2 size={11} className="animate-spin text-warning" />;
    case "error":
      return <X size={11} className="text-destructive" />;
    case "stopped":
    case "discovered":
    default:
      return <Minus size={11} className="text-muted-foreground" />;
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
      <span className="text-foreground max-w-[140px] truncate">{value}</span>
    </div>
  );
}
