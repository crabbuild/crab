import { Button } from "@/components/ui/button";
/**
 * SyncManagementPanel — full sync pair management view.
 * Shows all configured sync pairs for a profile with status, controls
 * (start/pause/remove), transfer progress, and conflict resolution.
 */

import clsx from "clsx";
import {
  AlertTriangle,
  ArrowUpDown,
  FolderSync,
  Pause,
  Play,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
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
      setError(
        err instanceof Error
          ? err.message
          : ((err as any)?.message ?? "Failed to load sync pairs"),
      );
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
      <div className="flex items-center justify-center py-12 text-muted-foreground">
        <RefreshCw className="w-4 h-4 animate-spin mr-2" />
        <span className="text-xs">Loading sync pairs…</span>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center py-12 gap-2">
        <AlertTriangle className="w-5 h-5 text-destructive" />
        <p className="text-xs text-destructive">{error}</p>
        <Button
          variant="ghost"
          size="compact"
          onClick={fetchPairs}
          className="text-xs text-primary hover:underline"
        >
          Retry
        </Button>
      </div>
    );
  }

  if (pairs.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-12 gap-3 text-center px-6">
        <div className="flex h-12 w-12 items-center justify-center rounded-full bg-primary/10">
          <FolderSync size={24} className="text-primary" />
        </div>
        <h3 className="text-sm font-medium text-foreground">
          No sync pairs configured
        </h3>
        <p className="text-xs text-muted-foreground max-w-xs">
          Right-click a folder in the File Browser and select "Sync this folder
          locally" to set up bidirectional sync.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Header */}
      <div className="flex items-center justify-between px-4 py-3 border-b border-border">
        <h2 className="text-xs font-semibold text-foreground flex items-center gap-2">
          <ArrowUpDown className="w-4 h-4 text-primary" />
          Sync Pairs ({pairs.length})
        </h2>
        <Button
          variant="ghost"
          size="compact"
          onClick={fetchPairs}
          className="p-1 text-muted-foreground hover:text-foreground hover:bg-accent rounded transition"
          aria-label="Refresh"
          title="Refresh"
        >
          <RefreshCw className="w-3.5 h-3.5" />
        </Button>
      </div>

      {/* Pair list */}
      <div className="flex-1 overflow-y-auto">
        {pairs.map((pair) => (
          <div key={pair.id} className="border-b border-border last:border-b-0">
            {/* Pair list */}
            <div className="flex items-center gap-3 px-4 py-3 hover:bg-accent transition">
              {/* Pair row */}
              <div
                className={clsx(
                  "w-2 h-2 rounded-full shrink-0",
                  pair.status === "active" && "bg-success",
                  pair.status === "paused" && "bg-warning",
                  pair.status === "error" && "bg-destructive",
                )}
                title={pair.status}
              />

              {/* Status indicator */}
              <Button
                variant="ghost"
                size="compact"
                className="justify-start flex-1 min-w-0 text-left"
                onClick={() => setExpandedPairId(expandedPairId === pair.id ? null : pair.id)}
              >
                <div className="flex items-center gap-1.5">
                  <span className="text-xs font-medium text-foreground truncate">
                    {basename(pair.remoteDir)}
                  </span>
                  <span className="text-[10px] text-muted-foreground">↔</span>
                  <span className="text-xs text-muted-foreground truncate">
                    {basename(pair.localDir)}
                  </span>
                </div>
                <div className="flex items-center gap-2 mt-0.5">
                  <span className="text-[10px] text-muted-foreground capitalize">
                    {pair.status}
                  </span>
                  {pair.lastSyncAt && (
                    <span className="text-[10px] text-muted-foreground">
                      · Last sync: {formatTimestamp(pair.lastSyncAt)}
                    </span>
                  )}
                  {(pair.pendingChanges ?? 0) > 0 && (
                    <span className="text-[10px] text-info">
                      · {pair.pendingChanges} pending
                    </span>
                  )}
                </div>
              </Button>

              {/* Actions */}
              <div className="flex items-center gap-1 shrink-0">
                {pair.status === "active" ? (
                  <Button
                    variant="ghost"
                    size="compact"
                    onClick={() => handlePause(pair.id)}
                    className="p-1 text-muted-foreground hover:text-warning hover:bg-accent rounded transition"
                    title="Pause sync"
                    aria-label="Pause sync"
                  >
                    <Pause className="w-3.5 h-3.5" />
                  </Button>
                ) : (
                  <Button
                    variant="ghost"
                    size="compact"
                    onClick={() => handleStart(pair.id)}
                    className="p-1 text-muted-foreground hover:text-success hover:bg-accent rounded transition"
                    title="Start sync"
                    aria-label="Start sync"
                  >
                    <Play className="w-3.5 h-3.5" />
                  </Button>
                )}
                <Button
                  variant="ghost"
                  size="compact"
                  onClick={() => handleRemove(pair.id)}
                  className="p-1 text-muted-foreground hover:text-destructive hover:bg-accent rounded transition"
                  title="Remove sync pair"
                  aria-label="Remove sync pair"
                >
                  <Trash2 className="w-3.5 h-3.5" />
                </Button>
              </div>
            </div>

            {/* Expanded details: paths + conflicts */}
            {expandedPairId === pair.id && (
              <div className="px-4 pb-3 space-y-2">
                <div className="rounded border border-border bg-background p-2 space-y-1">
                  <div className="flex items-center gap-2 text-[11px]">
                    <span className="text-muted-foreground w-14 shrink-0">
                      Remote:
                    </span>
                    <span className="text-foreground font-mono truncate">
                      {pair.remoteDir}
                    </span>
                  </div>
                  <div className="flex items-center gap-2 text-[11px]">
                    <span className="text-muted-foreground w-14 shrink-0">
                      Local:
                    </span>
                    <span className="text-foreground font-mono truncate">
                      {pair.localDir}
                    </span>
                  </div>
                  {pair.ignorePatterns.length > 0 && (
                    <div className="flex items-start gap-2 text-[11px]">
                      <span className="text-muted-foreground w-14 shrink-0">
                        Ignore:
                      </span>
                      <span className="text-muted-foreground font-mono">
                        {pair.ignorePatterns.join(", ")}
                      </span>
                    </div>
                  )}
                </div>
                {/* Conflicts for this pair */}
                <div className="max-h-48 overflow-y-auto rounded border border-border">
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
