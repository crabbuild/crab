/**
 * Integration wiring that connects the enhanced SSH IPC handler stubs
 * (registered in ssh-ipc.ts) to their actual main-process module
 * implementations.
 *
 * Call `wireEnhancedIpcHandlers(modules)` once during app initialization,
 * after all module instances are created and the stub handlers have been
 * registered via `registerSshEnhancedIpcHandlers()`.
 *
 * This replaces each stub's "not implemented" throw with a real delegation
 * to the appropriate module method, wrapping errors in the SshIpcError shape.
 */

import { ipcMain } from "electron";
import * as nodeFs from "node:fs";
import type { FileTransferManager } from "./ssh-file-transfer";
import type { BandwidthThrottler } from "./ssh-bandwidth-throttler";
import type { TransferScheduler } from "./ssh-transfer-scheduler";
import type { TransferHistoryStore } from "./ssh-transfer-history";
import type { TransferCompressor } from "./ssh-transfer-compressor";
import type { WorkspaceSyncEngine } from "./ssh-workspace-sync";
import type { RemoteProcessManager } from "./ssh-process-manager";
import type { RemoteEnvironmentManager } from "./ssh-environment-manager";
import type { RemoteToolManager } from "./ssh-tool-manager";
import type { ConnectionQualityMonitor } from "./ssh-connection-quality";
import type { BookmarkManager } from "./ssh-bookmarks";
import type { RemoteWorkspaceManager } from "./ssh-remote-workspace";
import type { RemoteGitManager } from "./ssh-remote-git";
import type { RemoteDebugManager } from "./ssh-remote-debug";
import type { SshSessionManager } from "./ssh-manager";
import type { SshIpcError,TransferDirection } from "./ssh-types";
import { broadcastToRenderer } from "./ssh-ipc";

// ── Types ────────────────────────────────────────────────────────

export interface EnhancedModules {
  fileTransferManager: FileTransferManager;
  bandwidthThrottler: BandwidthThrottler;
  transferScheduler: TransferScheduler;
  transferHistoryStore: TransferHistoryStore;
  transferCompressor: TransferCompressor;
  workspaceSyncEngine: WorkspaceSyncEngine;
  processManager: RemoteProcessManager;
  environmentManager: RemoteEnvironmentManager;
  toolManager: RemoteToolManager;
  connectionQualityMonitor: ConnectionQualityMonitor;
  bookmarkManager: BookmarkManager;
  workspaceManager: RemoteWorkspaceManager;
  gitManager: RemoteGitManager;
  debugManager: RemoteDebugManager;
  sessionManager: SshSessionManager;
}

// ── Error Helpers ────────────────────────────────────────────────

function toIpcError(err: unknown): SshIpcError {
  if (err instanceof Error) {
    return {
      code: "SSH_ERROR",
      message: err.message,
      retryable: isRetryable(err),
    };
  }
  return {
    code: "SSH_UNKNOWN_ERROR",
    message: String(err),
    retryable: false,
  };
}

function isRetryable(err: Error): boolean {
  const msg = err.message.toLowerCase();
  return (
    msg.includes("timeout") ||
    msg.includes("econnreset") ||
    msg.includes("econnrefused") ||
    msg.includes("network") ||
    msg.includes("unreachable")
  );
}

/**
 * Wrap an async handler so that errors are thrown as SshIpcError-shaped
 * objects that the renderer can parse consistently.
 */
function wrapHandler<T>(fn: () => Promise<T>): Promise<T> {
  return fn().catch((err) => {
    throw toIpcError(err);
  });
}

// ── Wiring ───────────────────────────────────────────────────────

/**
 * Replace all enhanced SSH IPC handler stubs with real implementations.
 *
 * This removes the existing stub handlers and re-registers them with
 * actual module method calls. Must be called after both
 * `registerSshEnhancedIpcHandlers()` and module instantiation.
 */
export function wireEnhancedIpcHandlers(modules: EnhancedModules): void {
  const {
    fileTransferManager,
    bandwidthThrottler,
    transferScheduler,
    transferHistoryStore,
    workspaceSyncEngine,
    processManager,
    environmentManager,
    toolManager,
    connectionQualityMonitor,
    bookmarkManager,
    workspaceManager,
    gitManager,
    debugManager,
    sessionManager,
  } = modules;

  // Remove all existing stub handlers before re-registering.
  const channels = [
    "ssh:transfer-list",
    "ssh:transfer-cancel",
    "ssh:transfer-cancel-batch",
    "ssh:transfer-retry",
    "ssh:transfer-retry-batch",
    "ssh:transfer-clear",
    "ssh:transfer-clear-completed",
    "ssh:transfer-set-bandwidth",
    "ssh:transfer-get-bandwidth",
    "ssh:transfer-set-compression",
    "ssh:transfer-set-adaptive",
    "ssh:transfer-schedule",
    "ssh:transfer-cancel-schedule",
    "ssh:transfer-list-scheduled",
    "ssh:transfer-history",
    "ssh:transfer-stats",
    "ssh:sync-add-pair",
    "ssh:sync-remove-pair",
    "ssh:sync-start",
    "ssh:sync-pause",
    "ssh:sync-list-pairs",
    "ssh:sync-list-conflicts",
    "ssh:sync-resolve-conflict",
    "ssh:sync-validate-local-dir",
    "ssh:process-list",
    "ssh:process-signal",
    "ssh:env-list",
    "ssh:env-set",
    "ssh:env-delete",
    "ssh:path-list",
    "ssh:path-update",
    "ssh:tool-detect-managers",
    "ssh:tool-install",
    "ssh:tool-install-cancel",
    "ssh:quality-get",
    "ssh:bookmark-add",
    "ssh:bookmark-remove",
    "ssh:bookmark-list",
    "ssh:recent-list",
    "ssh:transfer-security",
    "ssh:workspace-open",
    "ssh:workspace-search",
    "ssh:workspace-exec",
    "ssh:workspace-exec-kill",
    "ssh:workspace-tail",
    "ssh:workspace-tail-stop",
    "ssh:git-detect",
    "ssh:git-status",
    "ssh:git-operation",
    "ssh:debug-start",
    "ssh:debug-stop",
    "ssh:queue-drop-upload",
  ];

  for (const channel of channels) {
    ipcMain.removeHandler(channel);
  }

  // ─── Transfer Panel ──────────────────────────────────────────

  ipcMain.handle("ssh:transfer-list", async () => {
    return wrapHandler(async () => fileTransferManager.listTransfers());
  });

  ipcMain.handle("ssh:transfer-cancel", async (_event, transferId: string) => {
    return wrapHandler(async () => fileTransferManager.cancel(transferId));
  });

  ipcMain.handle("ssh:transfer-cancel-batch", async (_event, transferIds: string[]) => {
    return wrapHandler(async () => {
      let succeeded = 0;
      let failed = 0;
      for (const id of transferIds) {
        if (fileTransferManager.cancel(id)) {
          succeeded++;
        } else {
          failed++;
        }
      }
      return { succeeded, failed };
    });
  });

  ipcMain.handle("ssh:transfer-retry", async (_event, transferId: string) => {
    return wrapHandler(async () => {
      const transfer = fileTransferManager.getTransfer(transferId);
      if (!transfer) {
        throw new Error(`Transfer not found: ${transferId}`);
      }
      if (transfer.direction === "download") {
        return fileTransferManager.download(transfer.profileId, transfer.remotePath, transfer.localPath);
      }
      return fileTransferManager.upload(transfer.profileId, transfer.localPath, transfer.remotePath);
    });
  });

  ipcMain.handle("ssh:transfer-retry-batch", async (_event, transferIds: string[]) => {
    return wrapHandler(async () => {
      const newIds: string[] = [];
      for (const id of transferIds) {
        const transfer = fileTransferManager.getTransfer(id);
        if (!transfer) continue;
        if (transfer.direction === "download") {
          newIds.push(fileTransferManager.download(transfer.profileId, transfer.remotePath, transfer.localPath));
        } else {
          newIds.push(fileTransferManager.upload(transfer.profileId, transfer.localPath, transfer.remotePath));
        }
      }
      return newIds;
    });
  });

  ipcMain.handle("ssh:transfer-clear", async (_event, transferIds: string[]) => {
    return wrapHandler(async () => {
      for (const id of transferIds) {
        fileTransferManager.cancel(id);
      }
    });
  });

  ipcMain.handle("ssh:transfer-clear-completed", async () => {
    return wrapHandler(async () => {
      const completed = fileTransferManager.listTransfers("completed");
      const failed = fileTransferManager.listTransfers("failed");
      for (const t of [...completed, ...failed]) {
        fileTransferManager.cancel(t.id);
      }
    });
  });

  // ─── Transfer Settings ───────────────────────────────────────

  ipcMain.handle("ssh:transfer-set-bandwidth", async (_event, limitKBps: number) => {
    return wrapHandler(async () => {
      bandwidthThrottler.setLimit(limitKBps * 1024);
    });
  });

  ipcMain.handle("ssh:transfer-get-bandwidth", async () => {
    return wrapHandler(async () => ({
      limit: Math.round(bandwidthThrottler.getLimit() / 1024),
      actual: Math.round(bandwidthThrottler.getCurrentThroughput() / 1024),
    }));
  });

  ipcMain.handle("ssh:transfer-set-compression", async (_event, enabled: boolean) => {
    return wrapHandler(async () => {
      // Compression is a per-transfer decision based on file type;
      // this toggles the global preference stored in the throttler config.
      // The actual compression logic is in TransferCompressor.shouldCompress().
      // Store the preference so the FTM can check it before each transfer.
      (fileTransferManager as any)._compressionEnabled = enabled;
    });
  });

  ipcMain.handle("ssh:transfer-set-adaptive", async (_event, enabled: boolean) => {
    return wrapHandler(async () => {
      bandwidthThrottler.setAdaptiveEnabled(enabled);
    });
  });

  // ─── Transfer Scheduling ─────────────────────────────────────

  ipcMain.handle(
    "ssh:transfer-schedule",
    async (
      _event,
      opts: {
        profileId: string;
        direction: string;
        localPath: string;
        remotePath: string;
        scheduledAt: number;
      },
    ) => {
      return wrapHandler(async () => {
        return transferScheduler.schedule({
          profileId: opts.profileId,
          direction: opts.direction as TransferDirection,
          localPath: opts.localPath,
          remotePath: opts.remotePath,
          scheduledAt: opts.scheduledAt,
        });
      });
    },
  );

  ipcMain.handle("ssh:transfer-cancel-schedule", async (_event, transferId: string) => {
    return wrapHandler(async () => transferScheduler.cancel(transferId));
  });

  ipcMain.handle("ssh:transfer-list-scheduled", async (_event, profileId: string) => {
    return wrapHandler(async () => transferScheduler.listScheduled(profileId));
  });

  // ─── Transfer History ────────────────────────────────────────

  ipcMain.handle(
    "ssh:transfer-history",
    async (
      _event,
      opts: {
        page: number;
        pageSize: number;
        search?: string;
        sortBy?: string;
        sortDir?: "asc" | "desc";
        profileId?: string;
      },
    ) => {
      return wrapHandler(async () => transferHistoryStore.query({
        ...opts,
        sortBy: opts.sortBy as "timestamp" | "file_size" | "duration" | "average_speed" | undefined,
      }));
    },
  );

  ipcMain.handle("ssh:transfer-stats", async (_event, profileId: string, period: string) => {
    return wrapHandler(async () => {
      return transferHistoryStore.getStats(profileId, period as "day" | "week" | "month");
    });
  });

  // ─── Workspace Sync ──────────────────────────────────────────

  ipcMain.handle(
    "ssh:sync-add-pair",
    async (
      _event,
      opts: {
        profileId: string;
        localDir: string;
        remoteDir: string;
        ignorePatterns: string[];
      },
    ) => {
      return wrapHandler(async () => {
        const syncPairId = workspaceSyncEngine.addSyncPair(opts);
        return { syncPairId };
      });
    },
  );

  ipcMain.handle("ssh:sync-remove-pair", async (_event, syncPairId: string) => {
    return wrapHandler(async () => workspaceSyncEngine.removeSyncPair(syncPairId));
  });

  ipcMain.handle("ssh:sync-start", async (_event, syncPairId: string) => {
    return wrapHandler(async () => workspaceSyncEngine.start(syncPairId));
  });

  ipcMain.handle("ssh:sync-pause", async (_event, syncPairId: string) => {
    return wrapHandler(async () => workspaceSyncEngine.pause(syncPairId));
  });

  ipcMain.handle("ssh:sync-list-pairs", async (_event, profileId: string) => {
    return wrapHandler(async () => workspaceSyncEngine.listSyncPairs(profileId));
  });

  ipcMain.handle("ssh:sync-list-conflicts", async (_event, syncPairId: string) => {
    return wrapHandler(async () => workspaceSyncEngine.listConflicts(syncPairId));
  });

  ipcMain.handle("ssh:sync-resolve-conflict", async (_event, conflictId: string, resolution: string) => {
    return wrapHandler(async () => {
      workspaceSyncEngine.resolveConflict(conflictId, resolution as "local" | "remote" | "both");
    });
  });

  ipcMain.handle(
    "ssh:sync-validate-local-dir",
    async (_event, opts: { localDir: string; profileId: string }) => {
      const { localDir, profileId } = opts;

      // Check if the directory exists
      if (!nodeFs.existsSync(localDir)) {
        return { valid: false, error: "Directory does not exist" };
      }

      // Check if the directory is writable
      try {
        nodeFs.accessSync(localDir, nodeFs.constants.W_OK);
      } catch {
        return { valid: false, error: "Directory is not writable" };
      }

      // Check if the directory is already part of an existing sync pair
      const existingPairs = workspaceSyncEngine.listSyncPairs(profileId);
      const duplicate = existingPairs.find((pair) => pair.localDir === localDir);
      if (duplicate) {
        return { valid: false, error: "Directory is already part of an existing sync pair" };
      }

      return { valid: true };
    },
  );

  // ─── Process Management ──────────────────────────────────────

  ipcMain.handle("ssh:process-list", async (_event, profileId: string) => {
    return wrapHandler(async () => processManager.listProcesses(profileId));
  });

  ipcMain.handle("ssh:process-signal", async (_event, profileId: string, pid: number, signal: string) => {
    return wrapHandler(async () => {
      await processManager.signalProcess(profileId, pid, signal as "SIGTERM" | "SIGKILL");
    });
  });

  // ─── Environment Management ──────────────────────────────────

  ipcMain.handle("ssh:env-list", async (_event, profileId: string) => {
    return wrapHandler(async () => environmentManager.listEnvironment(profileId));
  });

  ipcMain.handle(
    "ssh:env-set",
    async (_event, profileId: string, name: string, value: string, persist: boolean) => {
      return wrapHandler(async () => environmentManager.setVariable(profileId, name, value, persist));
    },
  );

  ipcMain.handle("ssh:env-delete", async (_event, profileId: string, name: string) => {
    return wrapHandler(async () => environmentManager.deleteVariable(profileId, name));
  });

  ipcMain.handle("ssh:path-list", async (_event, profileId: string) => {
    return wrapHandler(async () => environmentManager.listPath(profileId));
  });

  ipcMain.handle("ssh:path-update", async (_event, profileId: string, entries: string[]) => {
    return wrapHandler(async () => environmentManager.updatePath(profileId, entries));
  });

  // ─── Tool Management ─────────────────────────────────────────

  ipcMain.handle("ssh:tool-detect-managers", async (_event, profileId: string) => {
    return wrapHandler(async () => toolManager.detectPackageManagers(profileId));
  });

  ipcMain.handle(
    "ssh:tool-install",
    async (_event, profileId: string, manager: string, tool: string, sudo: boolean) => {
      return wrapHandler(async () => {
        const result = await toolManager.installTool(
          profileId,
          manager as "apt" | "yum" | "brew" | "cargo" | "pip" | "npm",
          tool,
          sudo,
        );
        return result.streamId;
      });
    },
  );

  ipcMain.handle("ssh:tool-install-cancel", async (_event, streamId: string) => {
    return wrapHandler(async () => toolManager.cancelInstall(streamId));
  });

  // ─── Connection Quality ──────────────────────────────────────

  ipcMain.handle("ssh:quality-get", async (_event, profileId: string) => {
    return wrapHandler(async () => connectionQualityMonitor.getQuality(profileId));
  });

  // ─── Bookmarks & Recents ─────────────────────────────────────

  ipcMain.handle("ssh:bookmark-add", async (_event, profileId: string, path: string, label: string) => {
    return wrapHandler(async () => bookmarkManager.addBookmark(profileId, path, label));
  });

  ipcMain.handle("ssh:bookmark-remove", async (_event, bookmarkId: string) => {
    return wrapHandler(async () => bookmarkManager.removeBookmark(bookmarkId));
  });

  ipcMain.handle("ssh:bookmark-list", async (_event, profileId: string) => {
    return wrapHandler(async () => bookmarkManager.listBookmarks(profileId));
  });

  ipcMain.handle("ssh:recent-list", async (_event, profileId: string) => {
    return wrapHandler(async () => bookmarkManager.getRecent(profileId));
  });

  // ─── Drag-and-Drop Upload ────────────────────────────────────

  ipcMain.handle(
    "ssh:queue-drop-upload",
    async (
      _event,
      params: {
        profileId: string;
        remoteDir: string;
        files: Array<{ localPath: string; relativePath: string }>;
      },
    ) => {
      return wrapHandler(async () => {
        const { profileId, remoteDir, files } = params;

        // Map dropped files to upload entries: local path → remote path
        const uploadEntries = files.map((f) => ({
          localPath: f.localPath,
          remotePath: `${remoteDir}/${f.relativePath}`,
        }));

        fileTransferManager.uploadMultiple(profileId, uploadEntries);
        return undefined;
      });
    },
  );

  // ─── Security ────────────────────────────────────────────────

  ipcMain.handle("ssh:transfer-security", async (_event, profileId: string) => {
    return wrapHandler(async () => {
      const conn = sessionManager.getConnection(profileId);
      if (!conn || !conn.client) {
        throw new Error(`No active connection for profile: ${profileId}`);
      }
      // Extract cipher info from the ssh2 client's negotiated algorithms
      const algorithms = (conn.client as any)._sock?.getAlgorithms?.() ?? {};
      return {
        cipher: algorithms.cipher ?? "unknown",
        mac: algorithms.mac ?? "unknown",
        kex: algorithms.kex ?? "unknown",
      };
    });
  });

  // ─── Capabilities ────────────────────────────────────────────


  // ─── Agent Deployment ────────────────────────────────────────


  // ─── Remote Workspace IDE ────────────────────────────────────

  ipcMain.handle("ssh:workspace-open", async (_event, profileId: string, remotePath: string) => {
    return wrapHandler(async () => workspaceManager.openWorkspace(profileId, remotePath));
  });

  ipcMain.handle(
    "ssh:workspace-search",
    async (
      _event,
      profileId: string,
      query: string,
      opts: { regex?: boolean; caseSensitive?: boolean; maxResults?: number },
    ) => {
      return wrapHandler(async () => workspaceManager.searchWorkspace(profileId, query, opts));
    },
  );

  ipcMain.handle("ssh:workspace-exec", async (_event, profileId: string, command: string, cwd: string) => {
    return wrapHandler(async () => workspaceManager.executeCommand(profileId, command, cwd));
  });

  ipcMain.handle("ssh:workspace-exec-kill", async (_event, streamId: string) => {
    return wrapHandler(async () => workspaceManager.killCommand(streamId));
  });

  ipcMain.handle("ssh:workspace-tail", async (_event, profileId: string, filePath: string) => {
    return wrapHandler(async () => workspaceManager.tailFile(profileId, filePath));
  });

  ipcMain.handle("ssh:workspace-tail-stop", async (_event, streamId: string) => {
    return wrapHandler(async () => workspaceManager.stopTail(streamId));
  });

  // ─── Remote Git ──────────────────────────────────────────────

  ipcMain.handle("ssh:git-detect", async (_event, profileId: string, path: string) => {
    return wrapHandler(async () => gitManager.detectGitRepo(profileId, path));
  });

  ipcMain.handle("ssh:git-status", async (_event, profileId: string, repoPath: string) => {
    return wrapHandler(async () => gitManager.getStatus(profileId, repoPath));
  });

  ipcMain.handle(
    "ssh:git-operation",
    async (_event, profileId: string, op: { type: string; args?: Record<string, unknown> }) => {
      return wrapHandler(async () => {
        return gitManager.executeOperation(profileId, {
          type: op.type as any,
          repoPath: (op.args?.repoPath as string) ?? "",
          message: op.args?.message as string | undefined,
          branch: op.args?.branch as string | undefined,
          stashAction: op.args?.stashAction as any,
        });
      });
    },
  );

  // ─── Remote Debugging ────────────────────────────────────────

  ipcMain.handle(
    "ssh:debug-start",
    async (_event, profileId: string, opts: { runtime: string; command: string; cwd: string; port: number }) => {
      return wrapHandler(async () => debugManager.startSession(profileId, opts as any));
    },
  );

  ipcMain.handle("ssh:debug-stop", async (_event, sessionId: string) => {
    return wrapHandler(async () => debugManager.stopSession(sessionId));
  });

  // ─── Event Forwarding ────────────────────────────────────────

  wireEventForwarding(modules);
}

/**
 * Wire module EventEmitter events to the renderer via IPC push channels.
 * This connects the real-time event flow from modules to the UI.
 */
function wireEventForwarding(modules: EnhancedModules): void {
  const {
    connectionQualityMonitor,
    workspaceSyncEngine,
    gitManager,
    workspaceManager,
    toolManager,
  } = modules;

  // Connection quality transitions
  connectionQualityMonitor.on("quality-changed", (event) => {
    broadcastToRenderer("ssh:quality-changed", event);
  });

  // Workspace sync events
  workspaceSyncEngine.on("sync-status", (payload) => {
    broadcastToRenderer("ssh:sync-status", payload);
  });

  workspaceSyncEngine.on("sync-conflict", (payload) => {
    broadcastToRenderer("ssh:sync-conflict", payload);
  });

  workspaceSyncEngine.on("sync-error", (payload) => {
    broadcastToRenderer("ssh:sync-error", payload);
  });

  // Streaming output from tool installs, search, exec, tail
  toolManager.on("stream-data", (payload) => {
    broadcastToRenderer("ssh:stream-data", payload);
  });

  toolManager.on("stream-end", (payload) => {
    broadcastToRenderer("ssh:stream-end", payload);
  });

  gitManager.on("stream-data", (payload) => {
    broadcastToRenderer("ssh:stream-data", payload);
  });

  gitManager.on("stream-end", (payload) => {
    broadcastToRenderer("ssh:stream-end", payload);
  });

  workspaceManager.on("stream-data", (payload) => {
    broadcastToRenderer("ssh:stream-data", payload);
  });

  workspaceManager.on("stream-end", (payload) => {
    broadcastToRenderer("ssh:stream-end", payload);
  });

  workspaceManager.on("remote-file-changed", (payload) => {
    broadcastToRenderer("ssh:remote-file-changed", payload);
  });

  workspaceManager.on("remote-file-external-mod", (payload) => {
    broadcastToRenderer("ssh:remote-file-external-mod", payload);
  });
}
