/**
 * Displays the sync pair status (active/paused/error) in the file browser
 * toolbar when the current directory is within an active sync pair.
 * Also shows a conflict badge count when unresolved conflicts exist.
 * When in error state, displays the error message and a Retry button.
 */

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Circle, Pause, RotateCw } from "lucide-react";
import clsx from "clsx";

export type SyncPairStatus = "active" | "paused" | "error";

interface SyncPairInfo {
  syncPairId: string;
  status: SyncPairStatus;
  remoteDir: string;
  pendingChanges: number;
  lastSyncAt?: number;
}

interface SyncPairStatusBadgeProps {
  profileId: string;
  currentPath: string;
}

const STATUS_CONFIG: Record<SyncPairStatus, { colorClass: string; dotClass: string; label: string }> = {
  active: {
    colorClass: "text-success bg-success/10 border-success/20",
    dotClass: "bg-success",
    label: "Sync active",
  },
  paused: {
    colorClass: "text-warning bg-warning/10 border-warning/20",
    dotClass: "bg-warning",
    label: "Sync paused",
  },
  error: {
    colorClass: "text-destructive bg-destructive/10 border-destructive/20",
    dotClass: "bg-destructive",
    label: "Sync error",
  },
};

export function SyncPairStatusBadge({ profileId, currentPath }: SyncPairStatusBadgeProps) {
  const [pairInfo, setPairInfo] = useState<SyncPairInfo | null>(null);
  const [conflictCount, setConflictCount] = useState(0);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [retrying, setRetrying] = useState(false);

  // Check if the current path is within any active sync pair
  const findMatchingPair = useCallback(async () => {
    try {
      const ssh = (window as any).crab?.ssh;
      if (!ssh?.syncListPairs) return;

      const pairs = await ssh.syncListPairs(profileId);
      if (!Array.isArray(pairs)) return;

      // Find a sync pair whose remoteDir is a prefix of (or equal to) currentPath
      const match = pairs.find(
        (p: { remoteDir: string }) =>
          currentPath === p.remoteDir || currentPath.startsWith(p.remoteDir + "/"),
      );

      if (match) {
        setPairInfo({
          syncPairId: match.id,
          status: match.status as SyncPairStatus,
          remoteDir: match.remoteDir,
          pendingChanges: match.pendingChanges ?? 0,
          lastSyncAt: match.lastSyncAt,
        });

        // Fetch conflict count for this pair
        if (ssh.syncListConflicts) {
          try {
            const conflicts = await ssh.syncListConflicts(match.id);
            const unresolved = Array.isArray(conflicts)
              ? conflicts.filter((c: { resolvedAt?: number }) => !c.resolvedAt).length
              : 0;
            setConflictCount(unresolved);
          } catch {
            setConflictCount(0);
          }
        }
      } else {
        setPairInfo(null);
        setConflictCount(0);
        setErrorMessage(null);
      }
    } catch {
      setPairInfo(null);
      setConflictCount(0);
      setErrorMessage(null);
    }
  }, [profileId, currentPath]);

  // Initial load and path change
  useEffect(() => {
    findMatchingPair();
  }, [findMatchingPair]);

  // Listen for ssh:sync-status events to update pair-level status
  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh?.onSyncStatus) return;

    const cleanup = ssh.onSyncStatus(
      (payload: { syncPairId: string; status?: string; pendingChanges: number; lastSyncAt: number }) => {
        if (pairInfo && payload.syncPairId === pairInfo.syncPairId) {
          setPairInfo((prev) =>
            prev
              ? {
                  ...prev,
                  status: (payload.status as SyncPairStatus) ?? prev.status,
                  pendingChanges: payload.pendingChanges,
                  lastSyncAt: payload.lastSyncAt,
                }
              : prev,
          );
          // Clear error message when status transitions away from error
          if (payload.status && payload.status !== "error") {
            setErrorMessage(null);
          }
        }
      },
    );

    return () => {
      cleanup?.();
    };
  }, [pairInfo?.syncPairId]);

  // Listen for ssh:sync-error events to capture error messages
  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh?.onSyncError || !pairInfo) return;

    const cleanup = ssh.onSyncError(
      (payload: { syncPairId: string; error: string }) => {
        if (payload.syncPairId === pairInfo.syncPairId) {
          setErrorMessage(payload.error);
          setPairInfo((prev) =>
            prev ? { ...prev, status: "error" } : prev,
          );
        }
      },
    );

    return () => {
      cleanup?.();
    };
  }, [pairInfo?.syncPairId]);

  // Listen for conflict events to update badge count
  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh?.onSyncConflict || !pairInfo) return;

    const cleanup = ssh.onSyncConflict(
      (payload: { syncPairId: string }) => {
        if (payload.syncPairId === pairInfo.syncPairId) {
          setConflictCount((prev) => prev + 1);
        }
      },
    );

    return () => {
      cleanup?.();
    };
  }, [pairInfo?.syncPairId]);

  // Retry handler: calls syncStart to restart the sync pair
  const handleRetry = useCallback(async () => {
    if (!pairInfo) return;

    const ssh = (window as any).crab?.ssh;
    if (!ssh?.syncStart) return;

    setRetrying(true);
    try {
      await ssh.syncStart(pairInfo.syncPairId);
      setErrorMessage(null);
    } catch {
      // If retry itself fails, the sync-error event will update the message
    } finally {
      setRetrying(false);
    }
  }, [pairInfo?.syncPairId]);

  if (!pairInfo) return null;

  const config = STATUS_CONFIG[pairInfo.status];

  return (
    <div className="flex flex-col gap-1">
      <div
        className={clsx(
          "flex items-center gap-1.5 px-2 py-0.5 rounded-full border text-xs",
          config.colorClass,
        )}
        title={config.label}
        aria-label={`${config.label}${conflictCount > 0 ? `, ${conflictCount} conflicts` : ""}`}
      >
        {/* Status dot/icon */}
        {pairInfo.status === "active" && (
          <Circle className="w-2 h-2 fill-success text-success" aria-hidden="true" />
        )}
        {pairInfo.status === "paused" && (
          <Pause className="w-2.5 h-2.5" aria-hidden="true" />
        )}
        {pairInfo.status === "error" && (
          <AlertTriangle className="w-2.5 h-2.5" aria-hidden="true" />
        )}

        {/* Status label */}
        <span className="font-medium capitalize">{pairInfo.status}</span>

        {/* Retry button for error state */}
        {pairInfo.status === "error" && (
          <button
            type="button"
            onClick={handleRetry}
            disabled={retrying}
            className={clsx(
              "ml-1 flex items-center gap-0.5 px-1.5 py-0.5 rounded text-[10px] font-medium",
              "bg-destructive/20 hover:bg-destructive/30 text-destructive border border-destructive/30",
              "transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
            )}
            aria-label="Retry sync"
          >
            <RotateCw
              className={clsx("w-2.5 h-2.5", retrying && "animate-spin")}
              aria-hidden="true"
            />
            {retrying ? "Retrying…" : "Retry"}
          </button>
        )}

        {/* Conflict badge */}
        {conflictCount > 0 && (
          <span
            className="flex items-center justify-center min-w-[16px] h-4 px-1 rounded-full bg-warning text-warning-foreground text-[10px] font-bold leading-none"
            title={`${conflictCount} unresolved conflict${conflictCount !== 1 ? "s" : ""}`}
            aria-label={`${conflictCount} unresolved conflicts`}
          >
            {conflictCount}
          </span>
        )}
      </div>

      {/* Error message display */}
      {pairInfo.status === "error" && errorMessage && (
        <div
          className="px-2 py-1 text-[11px] text-destructive bg-destructive/5 border border-destructive/10 rounded"
          role="alert"
          aria-live="polite"
        >
          {errorMessage}
        </div>
      )}
    </div>
  );
}
