/**
 * Hook that listens for ssh:sync-conflict push events from the main process,
 * shows a toast notification for each new conflict, and provides state for
 * opening the ConflictResolutionDialog when the user clicks the toast.
 */

import { useCallback, useEffect, useState } from "react";
import { publishToast } from "../lib/toasts";
import type { SyncConflict } from "../../electron/ssh-types";

interface ConflictPayload {
  id: string;
  syncPairId: string;
  filePath: string;
  localMtime: number;
  remoteMtime: number;
}

export interface UseSyncConflictNotificationsResult {
  /** The conflict currently selected for resolution (shown in dialog), or null. */
  activeConflict: SyncConflict | null;
  /** Dismiss the conflict resolution dialog without resolving. */
  dismissConflict: () => void;
  /** Resolve the active conflict and close the dialog. */
  resolveConflict: (resolution: "local" | "remote" | "both") => Promise<void>;
}

/**
 * Subscribes to sync conflict events, publishes toast notifications,
 * and manages the ConflictResolutionDialog open state.
 */
export function useSyncConflictNotifications(): UseSyncConflictNotificationsResult {
  const [activeConflict, setActiveConflict] = useState<SyncConflict | null>(null);

  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh?.onSyncConflict) return;

    const cleanup = ssh.onSyncConflict((payload: ConflictPayload) => {
      const conflict: SyncConflict = {
        id: payload.id,
        syncPairId: payload.syncPairId,
        filePath: payload.filePath,
        localMtime: payload.localMtime,
        remoteMtime: payload.remoteMtime,
        detectedAt: Date.now(),
      };

      // Extract just the filename for a concise toast title
      const fileName = payload.filePath.split("/").pop() || payload.filePath;

      publishToast({
        kind: "warning",
        title: "Sync conflict detected",
        body: fileName,
        duration: 8000,
        action: {
          label: "Resolve",
          onClick: () => setActiveConflict(conflict),
        },
      });
    });

    return () => {
      cleanup?.();
    };
  }, []);

  const dismissConflict = useCallback(() => {
    setActiveConflict(null);
  }, []);

  const resolveConflict = useCallback(async (resolution: "local" | "remote" | "both") => {
    if (!activeConflict) return;
    try {
      const ssh = (window as any).crab?.ssh;
      if (ssh?.syncResolveConflict) {
        await ssh.syncResolveConflict(activeConflict.id, resolution);
      }
    } finally {
      setActiveConflict(null);
    }
  }, [activeConflict]);

  return { activeConflict, dismissConflict, resolveConflict };
}
