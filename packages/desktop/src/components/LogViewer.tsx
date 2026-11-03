/**
 * LogViewer — read-only xterm.js replay of saved ANSI session logs.
 *
 * Renders a saved terminal session log into a read-only xterm.js instance,
 * allowing users to browse past sessions and scroll through replayed content.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { FileText, X, RefreshCcw } from "lucide-react";
import clsx from "clsx";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { resolveTheme, getStoredSchemeId } from "../lib/terminal-themes";

interface LogEntry {
  sessionId: string;
  path: string;
  sizeBytes: number;
  mtimeMs: number;
}

interface Props {
  onClose: () => void;
}

export function LogViewer({ onClose }: Props) {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [selectedLog, setSelectedLog] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const theme = useEffectiveTheme();

  // Load available logs on mount
  useEffect(() => {
    void loadLogs();
  }, []);

  async function loadLogs() {
    try {
      const entries = await (window.crab as any).termLogList();
      setLogs(entries ?? []);
    } catch {
      setLogs([]);
    }
  }

  // Clean up xterm on unmount
  useEffect(() => {
    return () => {
      xtermRef.current?.dispose();
      xtermRef.current = null;
      fitRef.current = null;
    };
  }, []);

  const loadLog = useCallback(async (sessionId: string) => {
    setSelectedLog(sessionId);
    setLoading(true);

    try {
      const result = await (window.crab as any).termLogRead(sessionId);
      if (!result || !result.content) {
        setLoading(false);
        return;
      }

      // Dispose previous instance
      if (xtermRef.current) {
        xtermRef.current.dispose();
        xtermRef.current = null;
        fitRef.current = null;
      }

      // Wait for container to be available
      await new Promise((r) => setTimeout(r, 50));

      if (!containerRef.current) {
        setLoading(false);
        return;
      }

      // Create read-only xterm instance
      const xterm = new XTerm({
        cursorBlink: false,
        fontSize: 13,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
        scrollback: 100000,
        disableStdin: true,
        theme: resolveTheme(getStoredSchemeId(), theme),
      });

      const fit = new FitAddon();
      xterm.loadAddon(fit);

      // Clear container and mount
      containerRef.current.innerHTML = "";
      xterm.open(containerRef.current);

      // Write the log content
      xterm.write(result.content);

      // Fit to container
      requestAnimationFrame(() => {
        fit.fit();
      });

      xtermRef.current = xterm;
      fitRef.current = fit;
    } catch {
      // Failed to load log
    } finally {
      setLoading(false);
    }
  }, [theme]);

  // Resize handler
  useEffect(() => {
    const observer = new ResizeObserver(() => {
      if (fitRef.current && xtermRef.current) {
        fitRef.current.fit();
      }
    });
    if (containerRef.current) {
      observer.observe(containerRef.current);
    }
    return () => observer.disconnect();
  }, [selectedLog]);

  function formatDate(mtimeMs: number): string {
    const d = new Date(mtimeMs);
    return d.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  return (
    <div className="flex flex-col h-full bg-[#0b0d12] text-[#e7eaf1]">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-[#2a2e38]">
        <div className="flex items-center gap-2 text-sm font-medium">
          <FileText size={14} className="text-[#8b8fa7]" />
          Session Logs
        </div>
        <div className="flex items-center gap-1">
          <button
            onClick={() => void loadLogs()}
            className="p-1 rounded hover:bg-[#2a2e38] text-[#8b8fa7] hover:text-[#e7eaf1]"
            title="Refresh"
          >
            <RefreshCcw size={14} />
          </button>
          <button
            onClick={onClose}
            className="p-1 rounded hover:bg-[#2a2e38] text-[#8b8fa7] hover:text-[#e7eaf1]"
            title="Close"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      <div className="flex flex-1 min-h-0">
        {/* Sidebar: log list */}
        <div className="w-56 border-r border-[#2a2e38] overflow-y-auto flex-shrink-0">
          {logs.length === 0 ? (
            <div className="p-3 text-xs text-[#8b8fa7]">No session logs found.</div>
          ) : (
            logs.map((log) => (
              <button
                key={log.sessionId}
                onClick={() => void loadLog(log.sessionId)}
                className={clsx(
                  "w-full text-left px-3 py-2 text-xs border-b border-[#1a1d23] hover:bg-[#1a1d23] transition-colors",
                  selectedLog === log.sessionId && "bg-[#1a1d23] border-l-2 border-l-[#4f8cff]",
                )}
              >
                <div className="font-mono text-[#c8ccd8] truncate">{log.sessionId}</div>
                <div className="flex justify-between mt-0.5 text-[#8b8fa7]">
                  <span>{formatDate(log.mtimeMs)}</span>
                  <span>{formatSize(log.sizeBytes)}</span>
                </div>
              </button>
            ))
          )}
        </div>

        {/* Main: xterm replay area */}
        <div className="flex-1 min-w-0 relative">
          {!selectedLog && (
            <div className="absolute inset-0 flex items-center justify-center text-sm text-[#8b8fa7]">
              Select a session log to view
            </div>
          )}
          {loading && (
            <div className="absolute inset-0 flex items-center justify-center text-sm text-[#8b8fa7] z-10">
              Loading...
            </div>
          )}
          <div
            ref={containerRef}
            className={clsx("h-full w-full", !selectedLog && "hidden")}
          />
        </div>
      </div>
    </div>
  );
}
