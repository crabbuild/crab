/**
 * Workspace Sync Panel — displays sync pairs with status, pending changes,
 * last sync timestamp, conflict list with resolution options, and controls
 * for adding/removing/starting/pausing sync pairs.
 */

import { useState, useCallback, useMemo } from "react";
import {
  FolderSync,
  Play,
  Pause,
  Trash2,
  Plus,
  AlertTriangle,
  CheckCircle2,
  XCircle,
  Clock,
} from "lucide-react";
import type { SyncPair, SyncConflict } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

interface WorkspaceSyncPanelProps {
  pairs: Map<string, SyncPair>;
  conflicts: Map<string, SyncConflict>;
  onAddPair: (localDir: string, remoteDir: string, ignorePatterns: string[]) => void;
  onRemovePair: (syncPairId: string) => void;
  onStartPair: (syncPairId: string) => void;
  onPausePair: (syncPairId: string) => void;
  onResolveConflict: (conflictId: string, resolution: "local" | "remote" | "both") => void;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatTimestamp(ts?: number): string {
  if (!ts) return "Never";
  const date = new Date(ts);
  const now = Date.now();
  const diffMs = now - ts;

  if (diffMs < 60_000) return "Just now";
  if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
  if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function getStatusIcon(status: SyncPair["status"]) {
  switch (status) {
    case "active":
      return <CheckCircle2 className="w-4 h-4 text-green-500" />;
    case "paused":
      return <Pause className="w-4 h-4 text-yellow-500" />;
    case "error":
      return <XCircle className="w-4 h-4 text-red-500" />;
  }
}

// ── Add Pair Form ────────────────────────────────────────────────

function AddPairForm({
  onAdd,
  onCancel,
}: {
  onAdd: (localDir: string, remoteDir: string, ignorePatterns: string[]) => void;
  onCancel: () => void;
}) {
  const [localDir, setLocalDir] = useState("");
  const [remoteDir, setRemoteDir] = useState("");
  const [ignorePatterns, setIgnorePatterns] = useState("");

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!localDir.trim() || !remoteDir.trim()) return;
      const patterns = ignorePatterns
        .split("\n")
        .map((p) => p.trim())
        .filter(Boolean);
      onAdd(localDir.trim(), remoteDir.trim(), patterns);
    },
    [localDir, remoteDir, ignorePatterns, onAdd],
  );

  return (
    <form
      onSubmit={handleSubmit}
      className="border border-gray-200 dark:border-gray-700 rounded-md p-3 space-y-2"
    >
      <div>
        <label htmlFor="sync-local-dir" className="text-xs text-gray-500 dark:text-gray-400">
          Local Directory
        </label>
        <input
          id="sync-local-dir"
          type="text"
          value={localDir}
          onChange={(e) => setLocalDir(e.target.value)}
          placeholder="/home/user/project"
          className="w-full px-2 py-1 text-sm border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800"
          required
        />
      </div>
      <div>
        <label htmlFor="sync-remote-dir" className="text-xs text-gray-500 dark:text-gray-400">
          Remote Directory
        </label>
        <input
          id="sync-remote-dir"
          type="text"
          value={remoteDir}
          onChange={(e) => setRemoteDir(e.target.value)}
          placeholder="/remote/project"
          className="w-full px-2 py-1 text-sm border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800"
          required
        />
      </div>
      <div>
        <label htmlFor="sync-ignore" className="text-xs text-gray-500 dark:text-gray-400">
          Ignore Patterns (one per line)
        </label>
        <textarea
          id="sync-ignore"
          value={ignorePatterns}
          onChange={(e) => setIgnorePatterns(e.target.value)}
          placeholder={"node_modules\n.git\n*.log"}
          rows={3}
          className="w-full px-2 py-1 text-sm border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800 resize-none"
        />
      </div>
      <div className="flex justify-end gap-2 pt-1">
        <button
          type="button"
          onClick={onCancel}
          className="px-3 py-1 text-xs text-gray-600 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-700 rounded"
        >
          Cancel
        </button>
        <button
          type="submit"
          className="px-3 py-1 text-xs bg-blue-600 text-white rounded hover:bg-blue-700"
        >
          Add Sync Pair
        </button>
      </div>
    </form>
  );
}

// ── Conflict Item ────────────────────────────────────────────────

function ConflictItem({
  conflict,
  onResolve,
}: {
  conflict: SyncConflict;
  onResolve: (conflictId: string, resolution: "local" | "remote" | "both") => void;
}) {
  if (conflict.resolvedAt) return null;

  return (
    <li className="flex items-center justify-between px-2 py-1.5 rounded bg-amber-50 dark:bg-amber-900/20 border border-amber-200 dark:border-amber-800">
      <div className="flex items-center gap-2 min-w-0">
        <AlertTriangle className="w-3.5 h-3.5 text-amber-500 shrink-0" />
        <span className="text-xs text-gray-700 dark:text-gray-300 truncate" title={conflict.filePath}>
          {conflict.filePath}
        </span>
      </div>
      <div className="flex items-center gap-1 shrink-0 ml-2">
        <button
          onClick={() => onResolve(conflict.id, "local")}
          className="px-1.5 py-0.5 text-[10px] font-medium text-blue-600 dark:text-blue-400 hover:bg-blue-50 dark:hover:bg-blue-900/30 rounded"
          title="Keep local version"
        >
          Local
        </button>
        <button
          onClick={() => onResolve(conflict.id, "remote")}
          className="px-1.5 py-0.5 text-[10px] font-medium text-green-600 dark:text-green-400 hover:bg-green-50 dark:hover:bg-green-900/30 rounded"
          title="Keep remote version"
        >
          Remote
        </button>
        <button
          onClick={() => onResolve(conflict.id, "both")}
          className="px-1.5 py-0.5 text-[10px] font-medium text-purple-600 dark:text-purple-400 hover:bg-purple-50 dark:hover:bg-purple-900/30 rounded"
          title="Keep both versions"
        >
          Both
        </button>
      </div>
    </li>
  );
}

// ── Main Component ───────────────────────────────────────────────

export function WorkspaceSyncPanel({
  pairs,
  conflicts,
  onAddPair,
  onRemovePair,
  onStartPair,
  onPausePair,
  onResolveConflict,
}: WorkspaceSyncPanelProps) {
  const [showAddForm, setShowAddForm] = useState(false);

  const handleAdd = useCallback(
    (localDir: string, remoteDir: string, ignorePatterns: string[]) => {
      onAddPair(localDir, remoteDir, ignorePatterns);
      setShowAddForm(false);
    },
    [onAddPair],
  );

  // Get unresolved conflicts grouped by sync pair
  const unresolvedConflicts = useMemo(() => {
    const result: SyncConflict[] = [];
    for (const conflict of conflicts.values()) {
      if (!conflict.resolvedAt) result.push(conflict);
    }
    return result;
  }, [conflicts]);

  const pairList = useMemo(() => Array.from(pairs.values()), [pairs]);

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Workspace Sync">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-gray-100 dark:border-gray-700">
        <h3 className="text-xs font-semibold text-gray-600 dark:text-gray-300 flex items-center gap-1.5">
          <FolderSync className="w-4 h-4" />
          Workspace Sync
        </h3>
        {!showAddForm && (
          <button
            onClick={() => setShowAddForm(true)}
            className="flex items-center gap-1 px-2 py-1 text-xs text-blue-600 dark:text-blue-400 hover:bg-blue-50 dark:hover:bg-blue-900/20 rounded"
            aria-label="Add sync pair"
          >
            <Plus className="w-3 h-3" />
            Add
          </button>
        )}
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-2 space-y-3">
        {/* Add form */}
        {showAddForm && (
          <AddPairForm onAdd={handleAdd} onCancel={() => setShowAddForm(false)} />
        )}

        {/* Sync pairs list */}
        {pairList.length === 0 && !showAddForm && (
          <p className="text-xs text-gray-500 dark:text-gray-400 italic text-center py-4">
            No sync pairs configured.
          </p>
        )}

        {pairList.map((pair) => (
          <div
            key={pair.id}
            className="border border-gray-200 dark:border-gray-700 rounded-md p-2 space-y-1.5"
          >
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2 min-w-0">
                {getStatusIcon(pair.status)}
                <div className="min-w-0">
                  <p className="text-xs font-medium text-gray-700 dark:text-gray-300 truncate">
                    {pair.localDir}
                  </p>
                  <p className="text-[10px] text-gray-500 dark:text-gray-400 truncate">
                    ↔ {pair.remoteDir}
                  </p>
                </div>
              </div>
              <div className="flex items-center gap-1 shrink-0">
                {pair.status === "paused" || pair.status === "error" ? (
                  <button
                    onClick={() => onStartPair(pair.id)}
                    className="p-1 text-green-600 dark:text-green-400 hover:bg-green-50 dark:hover:bg-green-900/20 rounded"
                    aria-label="Start sync"
                    title="Start sync"
                  >
                    <Play className="w-3.5 h-3.5" />
                  </button>
                ) : (
                  <button
                    onClick={() => onPausePair(pair.id)}
                    className="p-1 text-yellow-600 dark:text-yellow-400 hover:bg-yellow-50 dark:hover:bg-yellow-900/20 rounded"
                    aria-label="Pause sync"
                    title="Pause sync"
                  >
                    <Pause className="w-3.5 h-3.5" />
                  </button>
                )}
                <button
                  onClick={() => onRemovePair(pair.id)}
                  className="p-1 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/20 rounded"
                  aria-label="Remove sync pair"
                  title="Remove sync pair"
                >
                  <Trash2 className="w-3.5 h-3.5" />
                </button>
              </div>
            </div>

            {/* Status details */}
            <div className="flex items-center gap-3 text-[10px] text-gray-500 dark:text-gray-400">
              <span className="flex items-center gap-1">
                <Clock className="w-3 h-3" />
                {formatTimestamp(pair.lastSyncAt)}
              </span>
              {pair.pendingChanges > 0 && (
                <span className="text-amber-600 dark:text-amber-400 font-medium">
                  {pair.pendingChanges} pending
                </span>
              )}
            </div>
          </div>
        ))}

        {/* Conflicts section */}
        {unresolvedConflicts.length > 0 && (
          <div className="space-y-1.5">
            <h4 className="text-xs font-semibold text-amber-600 dark:text-amber-400 flex items-center gap-1">
              <AlertTriangle className="w-3.5 h-3.5" />
              Conflicts ({unresolvedConflicts.length})
            </h4>
            <ul className="space-y-1" aria-label="Sync conflicts">
              {unresolvedConflicts.map((conflict) => (
                <ConflictItem
                  key={conflict.id}
                  conflict={conflict}
                  onResolve={onResolveConflict}
                />
              ))}
            </ul>
          </div>
        )}
      </div>
    </div>
  );
}
