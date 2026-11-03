/**
 * SyncManagementPanel — full sync pair management view.
 * Shows all configured sync pairs for a profile with status, controls
 * (start/pause/remove), transfer progress, and conflict resolution.
 */

import { useState, useEffect, useCallback } from "react";
import {
  FolderSync,
  Play,
  Pause,
  Trash2,
  AlertTriangle,
  RefreshCw,
  ArrowUpDown,
} from "lucide-react";
import clsx from "clsx";
import { SyncConflictList } from "./SyncConflictList";

// ── Types ────────────────────────────────────────────────────────

interface SyncPair {
  id: string;
  profileId: string;
  localDir: string;
  remoteDir: string;
  ignorePatterns: string[];
  status: "active" | "paused" | "error";
  lastSyncAt?: number;
  pendingChanges?: number;
}

export interface SyncManagementPanelProps {
  profileId: string;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatTimestamp(ts?: number): string {
  if (!ts) return "Never";
  const diff = Date.now() - ts;
  if (diff < 60_000) return "Just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return new Date(ts).toLocaleDateString();
}

function basename(p: string): string {
  return p.split("/").pop() || p;
}

// ── Component ────────────────────────────────────────────────────

export function SyncManagementPanel({ profileId }: SyncManagementPanelProps) {
  const [pairs, setPairs] = useState<SyncPair[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [expandedPairId, setExpandedPairId] = useState<string | null>(null);

  const fetchPairs = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const ssh = (window as any).crab?.ssh;
      if (!ssh?.syncListPairs) {
        setError("Sync API not available");
        return;
      }
      const result = await ssh.syncListPairs(profileId);
      setPairs(Array.isArray(result) ? result : []);
    } catch (err) {
      setError(err instanceof Error ? err.message : (err as any)?.message ?? "Failed to load sync pairs");
    } finally {
      setLoading(false);
    }
  }, [profileId]);

  useEffect(() => {
    fetchPairs();
  }, [fetchPairs]);

  // Listen for sync status changes to refresh
  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh?.onSyncStatus) return;
    const cleanup = ssh.onSyncStatus(() => {
      fetchPairs();
    });
    return () => { cleanup?.(); };
  }, [fetchPairs]);

  const handleStart = useCallback(async (syncPairId: string) => {
    try {
      await (window as any).crab.ssh.syncStart(syncPairId);
    } catch {
      // Error will be reflected via sync-status event
    }
  }, []);

  const handlePause = useCallback(async (syncPairId: string) => {
    try {
      await (window as any).crab.ssh.syncPause(syncPairId);
    } catch {
      // Error will be reflected via sync-status event
    }
  }, []);

  const handleRemove = useCallback(async (syncPairId: string) => {
    try {
      await (window as any).crab.ssh.syncRemovePair(syncPairId);
      setPairs((prev) => prev.filter((p) => p.id !== syncPairId));
      if (expandedPairId === syncPairId) setExpandedPairId(null);
    } catch {
      // Ignore
    }
  }, [expandedPairId]);

  if (loading) {
    return (
      <div className="flex items-center justify-center py-12 text-gx-muted">
        <RefreshCw className="w-4 h-4 animate-spin mr-2" />
        <span className="text-xs">Loading sync pairs…</span>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center py-12 gap-2">
        <AlertTriangle className="w-5 h-5 text-red-400" />
        <p className="text-xs text-red-400">{error}</p>
        <button onClick={fetchPairs} className="text-xs text-gx-accent hover:underline">
          Retry
        </button>
      </div>
    );
  }

  if (pairs.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-12 gap-3 text-center px-6">
        <div className="flex h-12 w-12 items-center justify-center rounded-full bg-gx-accent/10">
          <FolderSync size={24} className="text-gx-accent" />
        </div>
        <h3 className="text-sm font-medium text-gx-text">No sync pairs configured</h3>
        <p className="text-xs text-gx-muted max-w-xs">
          Right-click a folder in the File Browser and select "Sync this folder locally" to set up bidirectional sync.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Header */}
      <div className="flex items-center justify-between px-4 py-3 border-b border-gx-border">
        <h2 className="text-xs font-semibold text-gx-text flex items-center gap-2">
          <ArrowUpDown className="w-4 h-4 text-gx-accent" />
          Sync Pairs ({pairs.length})
        </h2>
        <button
          onClick={fetchPairs}
          className="p-1 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover rounded transition"
          aria-label="Refresh"
          title="Refresh"
        >
          <RefreshCw className="w-3.5 h-3.5" />
        </button>
      </div>

      {/* Pair list */}
      <div className="flex-1 overflow-y-auto">
        {pairs.map((pair) => (
          <div key={pair.id} className="border-b border-gx-border last:border-b-0">
            {/* Pair row */}
            <div className="flex items-center gap-3 px-4 py-3 hover:bg-gx-surface-hover transition">
              {/* Status indicator */}
              <div
                className={clsx(
                  "w-2 h-2 rounded-full shrink-0",
                  pair.status === "active" && "bg-green-500",
                  pair.status === "paused" && "bg-yellow-500",
                  pair.status === "error" && "bg-red-500",
                )}
                title={pair.status}
              />

              {/* Pair info */}
              <button
                className="flex-1 min-w-0 text-left"
                onClick={() => setExpandedPairId(expandedPairId === pair.id ? null : pair.id)}
              >
                <div className="flex items-center gap-1.5">
                  <span className="text-xs font-medium text-gx-text truncate">
                    {basename(pair.remoteDir)}
                  </span>
                  <span className="text-[10px] text-gx-muted">↔</span>
                  <span className="text-xs text-gx-muted truncate">
                    {basename(pair.localDir)}
                  </span>
                </div>
                <div className="flex items-center gap-2 mt-0.5">
                  <span className="text-[10px] text-gx-muted capitalize">{pair.status}</span>
                  {pair.lastSyncAt && (
                    <span className="text-[10px] text-gx-muted">
                      · Last sync: {formatTimestamp(pair.lastSyncAt)}
                    </span>
                  )}
                  {(pair.pendingChanges ?? 0) > 0 && (
                    <span className="text-[10px] text-blue-400">
                      · {pair.pendingChanges} pending
                    </span>
                  )}
                </div>
              </button>

              {/* Actions */}
              <div className="flex items-center gap-1 shrink-0">
                {pair.status === "active" ? (
                  <button
                    onClick={() => handlePause(pair.id)}
                    className="p-1 text-gx-muted hover:text-yellow-500 hover:bg-gx-surface-hover rounded transition"
                    title="Pause sync"
                    aria-label="Pause sync"
                  >
                    <Pause className="w-3.5 h-3.5" />
                  </button>
                ) : (
                  <button
                    onClick={() => handleStart(pair.id)}
                    className="p-1 text-gx-muted hover:text-green-500 hover:bg-gx-surface-hover rounded transition"
                    title="Start sync"
                    aria-label="Start sync"
                  >
                    <Play className="w-3.5 h-3.5" />
                  </button>
                )}
                <button
                  onClick={() => handleRemove(pair.id)}
                  className="p-1 text-gx-muted hover:text-red-400 hover:bg-gx-surface-hover rounded transition"
                  title="Remove sync pair"
                  aria-label="Remove sync pair"
                >
                  <Trash2 className="w-3.5 h-3.5" />
                </button>
              </div>
            </div>

            {/* Expanded details: paths + conflicts */}
            {expandedPairId === pair.id && (
              <div className="px-4 pb-3 space-y-2">
                <div className="rounded border border-gx-border bg-gx-bg-sub p-2 space-y-1">
                  <div className="flex items-center gap-2 text-[11px]">
                    <span className="text-gx-muted w-14 shrink-0">Remote:</span>
                    <span className="text-gx-text font-mono truncate">{pair.remoteDir}</span>
                  </div>
                  <div className="flex items-center gap-2 text-[11px]">
                    <span className="text-gx-muted w-14 shrink-0">Local:</span>
                    <span className="text-gx-text font-mono truncate">{pair.localDir}</span>
                  </div>
                  {pair.ignorePatterns.length > 0 && (
                    <div className="flex items-start gap-2 text-[11px]">
                      <span className="text-gx-muted w-14 shrink-0">Ignore:</span>
                      <span className="text-gx-muted font-mono">{pair.ignorePatterns.join(", ")}</span>
                    </div>
                  )}
                </div>
                {/* Conflicts for this pair */}
                <div className="max-h-48 overflow-y-auto rounded border border-gx-border">
                  <SyncConflictList syncPairId={pair.id} />
                </div>
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
