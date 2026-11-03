/**
 * Graceful shutdown sequence for the enhanced SSH subsystem.
 *
 * Call `gracefulShutdown(modules)` on app quit (e.g., in the Electron
 * "before-quit" handler). It performs an ordered teardown:
 *
 * 1. Cancel active transfers and persist resume state
 * 2. Stop file watchers and pause workspace sync
 * 3. Close streaming operations (search, exec, tail)
 * 4. Persist unsaved workspace edits
 * 5. Stop process polling and clear caches
 * 6. Close the SshDatabase connection
 *
 * Each step is wrapped in a try/catch so that a failure in one phase
 * does not prevent subsequent phases from executing.
 */

import type { FileTransferManager } from "./ssh-file-transfer";
import type { TransferHistoryStore } from "./ssh-transfer-history";
import type { TransferScheduler } from "./ssh-transfer-scheduler";
import type { BandwidthThrottler } from "./ssh-bandwidth-throttler";
import type { WorkspaceSyncEngine } from "./ssh-workspace-sync";
import type { RemoteFileWatcher } from "./ssh-remote-file-watcher";
import type { RemoteWorkspaceManager } from "./ssh-remote-workspace";
import type { RemoteProcessManager } from "./ssh-process-manager";
import type { ConnectionQualityMonitor } from "./ssh-connection-quality";
import type { RemoteToolManager } from "./ssh-tool-manager";
import type { RemoteGitManager } from "./ssh-remote-git";
import type { RemoteDebugManager } from "./ssh-remote-debug";
import type { SshDatabase } from "./ssh-database";

// ── Types ────────────────────────────────────────────────────────

export interface ShutdownModules {
  fileTransferManager: FileTransferManager;
  transferHistoryStore: TransferHistoryStore;
  transferScheduler: TransferScheduler;
  bandwidthThrottler: BandwidthThrottler;
  workspaceSyncEngine: WorkspaceSyncEngine;
  remoteFileWatcher: RemoteFileWatcher;
  workspaceManager: RemoteWorkspaceManager;
  processManager: RemoteProcessManager;
  connectionQualityMonitor: ConnectionQualityMonitor;
  toolManager: RemoteToolManager;
  gitManager: RemoteGitManager;
  debugManager: RemoteDebugManager;
  sshDatabase: SshDatabase;
}

// ── Shutdown ─────────────────────────────────────────────────────

/**
 * Execute the graceful shutdown sequence for all enhanced SSH modules.
 *
 * Designed to be called from the Electron app's "before-quit" or
 * "will-quit" handler. Each phase runs independently so that a failure
 * in one does not block the others.
 */
export async function gracefulShutdown(modules: ShutdownModules): Promise<void> {
  const {
    fileTransferManager,
    transferHistoryStore,
    transferScheduler,
    bandwidthThrottler,
    workspaceSyncEngine,
    remoteFileWatcher,
    workspaceManager,
    processManager,
    connectionQualityMonitor,
    toolManager,
    gitManager,
    debugManager,
    sshDatabase,
  } = modules;

  // Phase 1: Cancel active transfers and persist resume state
  await safeExec("cancel transfers", async () => {
    const activeTransfers = fileTransferManager.listTransfers("active");
    const queuedTransfers = fileTransferManager.listTransfers("queued");

    for (const transfer of [...activeTransfers, ...queuedTransfers]) {
      // Persist resume state for active transfers before cancelling
      if (transfer.status === "active" && transfer.bytesTransferred > 0) {
        try {
          transferHistoryStore.saveResumeState({
            transferId: transfer.id,
            profileId: transfer.profileId,
            direction: transfer.direction,
            localPath: transfer.localPath,
            remotePath: transfer.remotePath,
            bytesTransferred: transfer.bytesTransferred,
            totalBytes: transfer.totalBytes,
            remoteMtime: Date.now(),
            remoteSize: transfer.totalBytes,
            partialChecksum: "",
            createdAt: Date.now(),
            expiresAt: Date.now() + 7 * 24 * 60 * 60 * 1000,
          });
        } catch {
          // Best-effort persistence — don't block shutdown
        }
      }
      fileTransferManager.cancel(transfer.id);
    }

    // Dispose the scheduler (clears pending timers)
    transferScheduler.dispose();

    // Destroy the bandwidth throttler (clears refill timer)
    bandwidthThrottler.destroy();
  });

  // Phase 2: Stop file watchers and pause workspace sync
  await safeExec("stop watchers and sync", async () => {
    remoteFileWatcher.dispose();
    workspaceSyncEngine.dispose();
  });

  // Phase 3: Close streaming operations (search, exec, tail, git, debug)
  await safeExec("close streams", async () => {
    workspaceManager.dispose();
    toolManager.disposeAll();
    gitManager.cancelAllStreams();
    await debugManager.dispose();
  });

  // Phase 4: Persist unsaved workspace edits
  // (Already handled by workspaceManager.dispose() which persists edits
  //  to disk before closing. The edits are stored in the local filesystem
  //  and recovered on next launch via recoverUnsavedEdits().)

  // Phase 5: Stop process polling and clear caches
  await safeExec("stop polling", async () => {
    processManager.stopAllPolling();
    connectionQualityMonitor.stopAll();
  });

  // Phase 6: Close the SshDatabase connection
  await safeExec("close database", async () => {
    // Purge expired resume states and old history before closing
    try {
      transferHistoryStore.purgeExpired();
    } catch {
      // Non-critical — don't block shutdown
    }
    sshDatabase.close();
  });
}

// ── Helpers ──────────────────────────────────────────────────────

/**
 * Execute a shutdown phase, catching and logging any errors so that
 * subsequent phases still run.
 */
async function safeExec(phase: string, fn: () => Promise<void> | void): Promise<void> {
  try {
    await fn();
  } catch (err) {
    console.error(`[ssh-shutdown] Error during "${phase}":`, err);
  }
}
