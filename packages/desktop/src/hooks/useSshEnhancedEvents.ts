/**
 * Hook that subscribes to all SSH enhanced push event channels from
 * the main process and dispatches the corresponding reducer actions
 * to the sshEnhancedReducer.
 *
 * Mount this hook in any component that manages the SSH enhanced state
 * via useReducer. Pass the dispatch function from useReducer as the
 * parameter.
 *
 * Events handled:
 * - ssh:transfer-progress       → TRANSFER_PROGRESS
 * - ssh:transfer-status-changed → TRANSFER_STATUS_CHANGED
 * - ssh:quality-changed         → QUALITY_UPDATE
 * - ssh:sync-status             → SYNC_UPDATE_PAIR
 * - ssh:sync-conflict           → SYNC_ADD_CONFLICT
 * - ssh:stream-data             → STREAM_DATA
 * - ssh:stream-end              → STREAM_END
 * - ssh:remote-file-changed     → custom event dispatch
 * - ssh:agent-upgrade-available → CAPABILITIES_SET (stores upgrade info)
 * - ssh:scheduled-fired         → TRANSFER_STATUS_CHANGED
 */

import { useEffect } from "react";
import type { SshEnhancedAction } from "../state/ssh-enhanced-state";
import type { TransferStatus, QualityLevel, ConnectionQuality } from "../../electron/ssh-types";

type Dispatch = (action: SshEnhancedAction) => void;

/**
 * Subscribes to all SSH enhanced push events from the main process
 * and dispatches reducer actions for state updates.
 *
 * @param dispatch - The dispatch function from useReducer(sshEnhancedReducer)
 */
export function useSshEnhancedEvents(dispatch: Dispatch): void {
  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh) return;

    // Transfer progress updates (throttled to ≤500ms by main process)
    const offTransferProgress = ssh.onTransferProgress?.(
      (payload: { id: string; bytesTransferred: number; speed: number; eta: number }) => {
        dispatch({
          type: "TRANSFER_PROGRESS",
          id: payload.id,
          bytesTransferred: payload.bytesTransferred,
          speed: payload.speed,
          eta: payload.eta,
        });
      },
    );

    // Transfer status transitions (queued → active → completed/failed)
    const offTransferStatus = ssh.onTransferStatusChanged?.(
      (payload: { transferId: string; status: string; error?: string }) => {
        dispatch({
          type: "TRANSFER_STATUS_CHANGED",
          id: payload.transferId,
          status: payload.status as TransferStatus,
          error: payload.error,
        });
      },
    );

    // Connection quality level changes (green/yellow/red)
    const offQuality = ssh.onQualityChanged?.(
      (payload: {
        profileId: string;
        previous: string;
        current: string;
        quality: { level: string; latencyMs: number; bandwidthMbps: number; packetLossPercent: number; measuredAt: number };
      }) => {
        dispatch({
          type: "QUALITY_UPDATE",
          profileId: payload.profileId,
          quality: {
            level: payload.quality.level as QualityLevel,
            latencyMs: payload.quality.latencyMs,
            bandwidthMbps: payload.quality.bandwidthMbps,
            packetLossPercent: payload.quality.packetLossPercent,
            measuredAt: payload.quality.measuredAt,
          } satisfies ConnectionQuality,
        });
      },
    );

    // Workspace sync status updates (pending changes, last sync time)
    const offSyncStatus = ssh.onSyncStatus?.(
      (payload: { syncPairId: string; pendingChanges: number; lastSyncAt: number }) => {
        dispatch({
          type: "SYNC_UPDATE_PAIR",
          id: payload.syncPairId,
          updates: {
            pendingChanges: payload.pendingChanges,
            lastSyncAt: payload.lastSyncAt,
          },
        });
      },
    );

    // Sync conflict detected
    const offSyncConflict = ssh.onSyncConflict?.(
      (payload: { id: string; syncPairId: string; filePath: string; localMtime: number; remoteMtime: number; detectedAt?: number }) => {
        dispatch({
          type: "SYNC_ADD_CONFLICT",
          conflict: {
            id: payload.id,
            syncPairId: payload.syncPairId,
            filePath: payload.filePath,
            localMtime: payload.localMtime,
            remoteMtime: payload.remoteMtime,
            detectedAt: payload.detectedAt ?? Date.now(),
          },
        });
      },
    );

    // Streaming output data (search, exec, tail, tool install)
    const offStreamData = ssh.onStreamData?.(
      (payload: { streamId: string; data: string }) => {
        dispatch({
          type: "STREAM_DATA",
          streamId: payload.streamId,
          data: payload.data,
        });
      },
    );

    // Stream ended (with optional exit code or error)
    const offStreamEnd = ssh.onStreamEnd?.(
      (payload: { streamId: string; exitCode?: number; error?: string }) => {
        dispatch({
          type: "STREAM_END",
          streamId: payload.streamId,
          exitCode: payload.exitCode,
          error: payload.error,
        });
      },
    );

    // Remote file change events — emit a custom DOM event for
    // components that need to react (e.g., file browser refresh)
    const offRemoteFileChanged = ssh.onRemoteFileChanged?.(
      (payload: { profileId: string; path: string; event: string }) => {
        window.dispatchEvent(
          new CustomEvent("ssh:remote-file-changed", { detail: payload }),
        );
      },
    );

    // Agent upgrade available — store in capabilities state
    const offAgentUpgrade = ssh.onAgentUpgradeAvailable?.(
      (payload: { profileId: string; currentVersion: string; availableVersion: string }) => {
        // Emit a custom event so UI components can show upgrade notifications
        window.dispatchEvent(
          new CustomEvent("ssh:agent-upgrade-available", { detail: payload }),
        );
      },
    );

    // Scheduled transfer fired — maps to a status change
    const offScheduledFired = ssh.onScheduledFired?.(
      (payload: { transferId: string; result: string; error?: string }) => {
        dispatch({
          type: "TRANSFER_STATUS_CHANGED",
          id: payload.transferId,
          status: (payload.result === "completed" ? "completed" : "failed") as TransferStatus,
          error: payload.error,
        });
      },
    );

    return () => {
      offTransferProgress?.();
      offTransferStatus?.();
      offQuality?.();
      offSyncStatus?.();
      offSyncConflict?.();
      offStreamData?.();
      offStreamEnd?.();
      offRemoteFileChanged?.();
      offAgentUpgrade?.();
      offScheduledFired?.();
    };
  }, [dispatch]);
}
