/**
 * SyncConflictList — scrollable panel showing all unresolved sync conflicts
 * for a given sync pair, sorted by detection time (newest first).
 * Each row displays the file path, timestamps, and a "Resolve" button
 * that opens the ConflictResolutionDialog.
 */

import { useState, useEffect, useCallback, useMemo } from "react";
import { AlertTriangle, RefreshCw } from "lucide-react";
import type { SyncConflict } from "../../../electron/ssh-types";
import {
  ConflictResolutionDialog,
  type ConflictResolution,
} from "./ConflictResolutionDialog";

// ── Types ────────────────────────────────────────────────────────

export interface SyncConflictListProps {
  syncPairId: string;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatTimestamp(ts: number): string {
  return new Date(ts).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

// ── Component ────────────────────────────────────────────────────

export function SyncConflictList({ syncPairId }: SyncConflictListProps) {
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [resolvingConflict, setResolvingConflict] = useState<SyncConflict | null>(null);

  const fetchConflicts = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await window.crab.ssh.syncListConflicts(syncPairId);
      setConflicts(result);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load conflicts");
    } finally {
      setLoading(false);
    }
  }, [syncPairId]);

  useEffect(() => {
    fetchConflicts();
  }, [fetchConflicts]);

  // Sort by detectedAt descending (newest first)
  const sortedConflicts = useMemo(() => {
    return [...conflicts]
      .filter((c) => !c.resolvedAt)
      .sort((a, b) => b.detectedAt - a.detectedAt);
  }, [conflicts]);

  const handleResolve = useCallback(
    async (resolution: ConflictResolution) => {
      if (!resolvingConflict) return;
      try {
        await window.crab.ssh.syncResolveConflict(resolvingConflict.id, resolution);
        setResolvingConflict(null);
        // Refresh the list after resolution
        fetchConflicts();
      } catch (err) {
        // Keep dialog open on error so user can retry
        console.error("Failed to resolve conflict:", err);
      }
    },
    [resolvingConflict, fetchConflicts],
  );

  if (loading) {
    return (
      <div className="flex items-center justify-center py-8 text-gx-muted">
        <RefreshCw className="w-4 h-4 animate-spin mr-2" />
        <span className="text-xs">Loading conflicts…</span>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center py-8 gap-2">
        <p className="text-xs text-red-400">{error}</p>
        <button
          onClick={fetchConflicts}
          className="text-xs text-gx-accent hover:underline"
        >
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Sync Conflicts">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-gx-border">
        <h3 className="text-xs font-semibold text-gx-text flex items-center gap-1.5">
          <AlertTriangle className="w-4 h-4 text-orange-400" />
          Conflicts ({sortedConflicts.length})
        </h3>
        <button
          onClick={fetchConflicts}
          className="p-1 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover rounded transition"
          aria-label="Refresh conflicts"
          title="Refresh"
        >
          <RefreshCw className="w-3.5 h-3.5" />
        </button>
      </div>

      {/* List */}
      <div className="flex-1 overflow-y-auto">
        {sortedConflicts.length === 0 ? (
          <p className="text-xs text-gx-muted italic text-center py-6">
            No unresolved conflicts.
          </p>
        ) : (
          <ul className="divide-y divide-gx-border" aria-label="Conflict list">
            {sortedConflicts.map((conflict) => (
              <li
                key={conflict.id}
                className="px-3 py-2.5 hover:bg-gx-surface-hover transition"
              >
                <div className="flex items-start justify-between gap-2">
                  <div className="min-w-0 flex-1">
                    <p
                      className="text-[12px] font-medium text-gx-text truncate"
                      title={conflict.filePath}
                    >
                      {conflict.filePath}
                    </p>
                    <div className="flex items-center gap-3 mt-1 text-[10px] text-gx-muted">
                      <span title="Local modified">
                        Local: {formatTimestamp(conflict.localMtime)}
                      </span>
                      <span title="Remote modified">
                        Remote: {formatTimestamp(conflict.remoteMtime)}
                      </span>
                    </div>
                    <p className="text-[10px] text-gx-muted mt-0.5">
                      Detected: {formatTimestamp(conflict.detectedAt)}
                    </p>
                  </div>
                  <button
                    onClick={() => setResolvingConflict(conflict)}
                    className="shrink-0 px-2.5 py-1 text-[11px] font-medium rounded border border-gx-accent/60 bg-gx-accent/15 text-gx-accent hover:bg-gx-accent/25 transition focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent"
                  >
                    Resolve
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>

      {/* Resolution dialog */}
      {resolvingConflict && (
        <ConflictResolutionDialog
          conflict={resolvingConflict}
          onResolve={handleResolve}
          onDismiss={() => setResolvingConflict(null)}
        />
      )}
    </div>
  );
}
