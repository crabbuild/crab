/**
 * IPC handlers for SSH profile management, connection operations,
 * and remote terminal management.
 *
 * Registers `ipcMain.handle()` handlers that bridge the renderer's
 * `window.crab.ssh.*` calls to the SshProfileManager,
 * SshSessionManager, and RemoteTerminalManager instances.
 */

import * as fs from "node:fs";
import { ipcMain, BrowserWindow } from "electron";
import { setPassword } from "./keychain";
import type { SshProfileManager } from "./ssh-profiles";
import type { SshSessionManager, SshProfile } from "./ssh-manager";
import type { RemoteTerminalManager } from "./ssh-terminal";
import type { PortForwardManager } from "./ssh-port-forward";
import type { RemoteFileBrowser } from "./ssh-file-browser";
import type { RemoteFileEditor } from "./ssh-file-editor";
import type { SystemHealthMonitor, HealthAlert } from "./ssh-health";
import type { SshLatencyMonitor } from "./ssh-latency-monitor";
import type { RpcRouter } from "./ssh-router";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { WorkspaceSyncEngine } from "./ssh-workspace-sync";
import type { FileTransferManager } from "./ssh-file-transfer";

/**
 * Register all SSH-related IPC handlers. Call once during app
 * initialization (from `registerIpc()` in main.ts).
 */
export function registerSshIpcHandlers(
  profileManager: SshProfileManager,
  sessionManager: SshSessionManager,
  terminalManager?: RemoteTerminalManager,
  portForwardManager?: PortForwardManager,
  fileBrowser?: RemoteFileBrowser,
  fileEditor?: RemoteFileEditor,
): void {
  ipcMain.handle("ssh:list-profiles", async () => {
    return profileManager.list();
  });

  ipcMain.handle("ssh:save-profile", async (_event, profile: Partial<SshProfile> & { host: string; username: string; password?: string }) => {
    const password = profile.password;
    delete profile.password;
    const saved = profileManager.save(profile);
    // Store password in OS keychain if provided
    if (password && saved.authMethod.type === "password") {
      await setPassword("crab-desktop-ssh", saved.id, password);
    }
    return saved;
  });

  ipcMain.handle("ssh:delete-profile", async (_event, profileId: string) => {
    // Disconnect if currently connected before deleting
    const status = sessionManager.getStatus(profileId);
    if (status !== "disconnected") {
      await sessionManager.disconnect(profileId);
    }
    return profileManager.delete(profileId);
  });

  ipcMain.handle("ssh:connect", async (_event, profileId: string) => {
    const profile = profileManager.get(profileId);
    if (!profile) {
      throw new Error(`SSH profile not found: ${profileId}`);
    }
    console.log(`[ssh:connect] profileId=${profileId}, authMethod=${profile.authMethod?.type}, host=${profile.host}:${profile.port}`);
    await sessionManager.connect(profile);
  });

  ipcMain.handle("ssh:disconnect", async (_event, profileId: string) => {
    await sessionManager.disconnect(profileId);
  });

  ipcMain.handle("ssh:test-connection", async (_event, profileOrId: string | Record<string, unknown>) => {
    let profile: SshProfile | undefined;
    if (typeof profileOrId === "string") {
      profile = profileManager.get(profileOrId);
      if (!profile) {
        throw new Error(`SSH profile not found: ${profileOrId}`);
      }
    } else {
      // Accept a profile-like object directly (used by the form before saving)
      profile = profileOrId as unknown as SshProfile;
    }
    // For password auth from the form, pass the password directly
    const password = (profileOrId as any)?.password;
    console.log(`[ssh:test-connection] authMethod=${profile.authMethod?.type}, host=${profile.host}:${profile.port}, user=${profile.username}, hasPassword=${!!password}`);
    const result = await sessionManager.testConnection(profile, password);
    console.log(`[ssh:test-connection] result:`, JSON.stringify(result));
    return result;
  });

  ipcMain.handle("ssh:get-status", async (_event, profileId: string) => {
    return sessionManager.getStatus(profileId);
  });

  // SSH config import (used by the first-run banner and import dialog)
  ipcMain.handle("ssh:import-config", async () => {
    const { parseSshConfig } = require("./ssh-profiles") as typeof import("./ssh-profiles.js");
    return parseSshConfig();
  });

  // ─── Remote File Browsing (SFTP-based) ───────────────────────────

  // Cache a single SFTP session per profile to avoid channel exhaustion.
  const sftpSessions = new Map<string, any>();

  function getSftp(profileId: string): Promise<any> {
    const existing = sftpSessions.get(profileId);
    if (existing) return Promise.resolve(existing);

    const conn = sessionManager.getConnection(profileId);
    if (!conn || !conn.client) {
      return Promise.reject(new Error(`No active SSH connection for profile: ${profileId}`));
    }

    return new Promise((resolve, reject) => {
      conn.client.sftp((err: Error | undefined, sftp: any) => {
        if (err) { reject(err); return; }
        sftpSessions.set(profileId, sftp);
        // Clean up when the connection closes
        conn.client.once("close", () => { sftpSessions.delete(profileId); });
        resolve(sftp);
      });
    });
  }

  ipcMain.handle("ssh:browse-path", async (_event, profileId: string, path: string) => {
    const sftp = await getSftp(profileId);
    return new Promise((resolve, reject) => {
      sftp.readdir(path, (err: Error | null, list: any[]) => {
        if (err) { reject(err); return; }
        const entries = list
          .map((item: any) => ({
            name: item.filename,
            path: path === "/" ? `/${item.filename}` : `${path}/${item.filename}`,
            kind: item.attrs.isDirectory() ? "dir" : "file",
            size: item.attrs.size,
          }))
          .sort((a: any, b: any) => {
            if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
            return a.name.localeCompare(b.name);
          });
        const hasGitDir = entries.some((e: any) => e.name === ".git" && e.kind === "dir");
        resolve({ entries, path, hasGitDir });
      });
    });
  });

  ipcMain.handle("ssh:home-dir", async (_event, profileId: string) => {
    const conn = sessionManager.getConnection(profileId);
    if (!conn || !conn.client) {
      throw new Error(`No active SSH connection for profile: ${profileId}`);
    }
    return new Promise((resolve, reject) => {
      conn.client.exec("echo $HOME", (err: Error | undefined, stream: any) => {
        if (err) { reject(err); return; }
        let data = "";
        stream.on("data", (chunk: Buffer) => { data += chunk.toString(); });
        stream.on("close", () => { resolve({ path: data.trim() || "/root" }); });
        stream.stderr.on("data", () => {});
      });
    });
  });

  // ─── Forward SSH status events to renderer ───────────────────────

  sessionManager.on("ssh:status-changed", (payload: { profileId: string; status: string; error?: string }) => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:status-changed", payload);
    }
  });

  // ─── Remote Terminal IPC ─────────────────────────────────────────

  if (terminalManager) {
    ipcMain.handle(
      "ssh:open-terminal",
      async (_event, profileId: string, cols: number, rows: number, cwd?: string, resumeId?: string, ownerPath?: string) => {
        const terminalId = await terminalManager.openTerminal(profileId, cols, rows, cwd, resumeId, ownerPath);
        return { terminalId };
      },
    );

    ipcMain.handle("ssh:close-terminal", async (_event, terminalId: string) => {
      await terminalManager.closeTerminal(terminalId);
    });
    ipcMain.handle("ssh:detach-terminal", (_event, terminalId: string) => terminalManager.detachTerminal(terminalId));

    ipcMain.handle(
      "ssh:terminal-resize",
      async (_event, terminalId: string, cols: number, rows: number) => {
        terminalManager.resizeTerminal(terminalId, cols, rows);
      },
    );

    ipcMain.handle("ssh:terminal-write", async (_event, terminalId: string, data: string) => {
      terminalManager.writeToTerminal(terminalId, data);
    });

    ipcMain.handle("ssh:terminal-reconnect", async (_event, terminalId: string) => {
      return terminalManager.reconnectTerminal(terminalId);
    });

    // Forward terminal data events to the renderer
    terminalManager.on("ssh:terminal-data", (payload: { terminalId: string; data: string }) => {
      const windows = BrowserWindow.getAllWindows();
      for (const win of windows) {
        win.webContents.send("ssh:terminal-data", payload);
      }
    });

    terminalManager.on("ssh:terminal-closed", (payload: { terminalId: string; profileId: string }) => {
      const windows = BrowserWindow.getAllWindows();
      for (const win of windows) {
        win.webContents.send("ssh:terminal-closed", payload);
      }
    });

    terminalManager.on("ssh:terminal-session-ended", (payload: { terminalId: string; profileId: string }) => {
      const windows = BrowserWindow.getAllWindows();
      for (const win of windows) {
        win.webContents.send("ssh:terminal-session-ended", payload);
      }
    });

    terminalManager.on("ssh:terminal-resumed", (payload: { terminalId: string; profileId: string }) => {
      const windows = BrowserWindow.getAllWindows();
      for (const win of windows) {
        win.webContents.send("ssh:terminal-resumed", payload);
      }
    });
  }

  // ─── Port Forwarding IPC ─────────────────────────────────────────

  if (portForwardManager) {
    ipcMain.handle(
      "ssh:add-forward",
      async (
        _event,
        profileId: string,
        localPort: number,
        remoteHost: string,
        remotePort: number,
        label?: string,
        autoStart?: boolean,
      ) => {
        const forwardId = await portForwardManager.addLocalForward(
          profileId,
          localPort,
          remoteHost,
          remotePort,
          label,
          autoStart,
        );
        return { forwardId };
      },
    );

    ipcMain.handle("ssh:remove-forward", async (_event, forwardId: string) => {
      portForwardManager.removeForward(forwardId);
    });

    ipcMain.handle("ssh:list-forwards", async (_event, profileId?: string) => {
      return portForwardManager.listForwards(profileId);
    });

    ipcMain.handle("ssh:open-forward-browser", async (_event, forwardId: string) => {
      await portForwardManager.openInBrowser(forwardId);
    });

    // Forward port-forward events to the renderer
    portForwardManager.on(
      "ssh:port-forward-detected",
      (payload: { profileId: string; suggestion: unknown }) => {
        const windows = BrowserWindow.getAllWindows();
        for (const win of windows) {
          win.webContents.send("ssh:port-forward-detected", payload);
        }
      },
    );

    portForwardManager.on(
      "ssh:port-forward-status",
      (payload: { forwardId: string; status: string; actualLocalPort?: number; error?: string }) => {
        const windows = BrowserWindow.getAllWindows();
        for (const win of windows) {
          win.webContents.send("ssh:port-forward-status", payload);
        }
      },
    );
  }

  // ─── Remote File Browsing IPC ────────────────────────────────────

  if (fileBrowser) {
    ipcMain.handle(
      "ssh:browse-path",
      async (_event, profileId: string, path: string) => {
        return fileBrowser.browsePath(profileId, path);
      },
    );

    ipcMain.handle("ssh:home-dir", async (_event, profileId: string) => {
      return { path: await fileBrowser.getHomeDir(profileId) };
    });

    ipcMain.handle(
      "ssh:open-as-repo",
      async (_event, profileId: string, path: string) => {
        return fileBrowser.openAsRepo(profileId, path);
      },
    );
  }

  // ─── Remote File Editing IPC ─────────────────────────────────────

  if (fileEditor) {
    ipcMain.handle(
      "ssh:read-file",
      async (_event, profileId: string, path: string) => {
        return fileEditor.readFile(profileId, path);
      },
    );

    ipcMain.handle(
      "ssh:write-file",
      async (_event, profileId: string, path: string, content: string, expectedMtime?: number) => {
        return fileEditor.writeFile(profileId, path, content, expectedMtime);
      },
    );

    ipcMain.handle(
      "ssh:create-file",
      async (_event, profileId: string, path: string) => {
        await fileEditor.createFile(profileId, path);
      },
    );

    ipcMain.handle(
      "ssh:delete-file",
      async (_event, profileId: string, path: string) => {
        await fileEditor.deleteFile(profileId, path);
      },
    );

    ipcMain.handle(
      "ssh:rename-file",
      async (_event, profileId: string, oldPath: string, newPath: string) => {
        await fileEditor.renameFile(profileId, oldPath, newPath);
      },
    );
  }
}


/**
 * Check if `~/.ssh/config` has importable hosts and no profiles exist yet.
 * If so, emit `ssh:config-detected` to the renderer with the host count.
 * Call once during app startup after SSH subsystem initialization.
 */
export function checkFirstRunSshConfig(profileManager: SshProfileManager): void {
  // Only show the banner if no profiles exist yet (first-run scenario)
  const existingProfiles = profileManager.list();
  if (existingProfiles.length > 0) {
    return;
  }

  // Dynamically import parseSshConfig to avoid circular deps at module level
  const { parseSshConfig } = require("./ssh-profiles") as typeof import("./ssh-profiles.js");
  const hosts = parseSshConfig();

  if (hosts.length === 0) {
    return;
  }

  // Emit to all renderer windows after a short delay to ensure the
  // renderer is ready to receive events
  setTimeout(() => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:config-detected", { hostCount: hosts.length });
    }
  }, 1500);
}

/**
 * Wire the SystemHealthMonitor's events to the renderer via IPC.
 * Call once after the health monitor is instantiated.
 *
 * Forwards:
 * - `health:alert` → `ssh:health-alert` (new alerts only, deduplicated)
 * - `health:alerts-snapshot` → `ssh:health-alerts-snapshot` (full alert list for state)
 * - `ssh:health-update` → `ssh:health-update` (health data for state)
 * - Also emits `ssh:bandwidth-update` alongside health updates when a router is provided.
 */
export function bridgeHealthMonitorToRenderer(healthMonitor: SystemHealthMonitor, router?: RpcRouter): void {
  healthMonitor.on("health:alert", (payload: { profileId: string; alert: HealthAlert }) => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:health-alert", payload);
    }
  });

  healthMonitor.on("health:alerts-snapshot", (payload: { profileId: string; alerts: HealthAlert[] }) => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:health-alerts-snapshot", payload);
    }
  });

  healthMonitor.on("ssh:health-update", (payload: { profileId: string; health: unknown }) => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:health-update", payload);
    }

    // Emit bandwidth stats alongside health updates
    if (router) {
      const bandwidth = router.getBandwidth(payload.profileId);
      for (const win of windows) {
        win.webContents.send("ssh:bandwidth-update", {
          profileId: payload.profileId,
          ...bandwidth,
        });
      }
    }
  });
}

/**
 * Wire the SshLatencyMonitor's events to the renderer via IPC.
 * Call once after the latency monitor is instantiated.
 *
 * Forwards `ssh:latency-update` and `ssh:high-latency-mode` events
 * to all renderer windows.
 */
export function bridgeLatencyMonitorToRenderer(latencyMonitor: SshLatencyMonitor): void {
  latencyMonitor.on("ssh:latency-update", (payload: { profileId: string; latencyMs: number }) => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:latency-update", payload);
    }
  });

  latencyMonitor.on("ssh:high-latency-mode", (payload: { profileId: string; enabled: boolean }) => {
    const windows = BrowserWindow.getAllWindows();
    for (const win of windows) {
      win.webContents.send("ssh:high-latency-mode", payload);
    }
  });
}

// ─── Enhanced SSH IPC Handlers ─────────────────────────────────────

/**
 * All new IPC push event channels for the enhanced SSH subsystem.
 * These are used by `BrowserWindow.webContents.send()` to push
 * real-time updates to the renderer.
 */
export const SSH_ENHANCED_PUSH_CHANNELS = [
  "ssh:transfer-progress",
  "ssh:transfer-status-changed",
  "ssh:transfer-batch-result",
  "ssh:sync-status",
  "ssh:sync-file-status",
  "ssh:sync-conflict",
  "ssh:sync-error",
  "ssh:sync-progress",
  "ssh:quality-changed",
  "ssh:stream-data",
  "ssh:stream-end",
  "ssh:remote-file-changed",
  "ssh:remote-file-external-mod",
  "ssh:agent-upgrade-available",
  "ssh:scheduled-fired",
  "ssh:deploy-progress",
  "ssh:agent-capabilities",
  "ssh:agent-unavailable",
] as const;

export type SshEnhancedPushChannel = (typeof SSH_ENHANCED_PUSH_CHANNELS)[number];

/**
 * Register all enhanced SSH IPC handlers.
 *
 * Agent deployment and capability handlers are wired to real module
 * instances when provided. Remaining handlers throw "not implemented"
 * until wired via `wireEnhancedIpcHandlers()`.
 *
 * Call once during app initialization alongside `registerSshIpcHandlers()`.
 */
export function registerSshEnhancedIpcHandlers(opts?: {
  capabilityNegotiator?: CapabilityNegotiator;
  rpcRouter?: RpcRouter;
  sessionManager?: SshSessionManager;
  workspaceSyncEngine?: WorkspaceSyncEngine;
}): void {
  const capabilityNegotiator = opts?.capabilityNegotiator;
  const sessionManager = opts?.sessionManager;
  const workspaceSyncEngine = opts?.workspaceSyncEngine;
  // ─── Transfer Panel ────────────────────────────────────────────

  ipcMain.handle("ssh:transfer-list", async () => {
    throw new Error("ssh:transfer-list not implemented");
  });

  ipcMain.handle("ssh:transfer-cancel", async (_event, _transferId: string) => {
    throw new Error("ssh:transfer-cancel not implemented");
  });

  ipcMain.handle("ssh:transfer-cancel-batch", async (_event, _transferIds: string[]) => {
    throw new Error("ssh:transfer-cancel-batch not implemented");
  });

  ipcMain.handle("ssh:transfer-retry", async (_event, _transferId: string) => {
    throw new Error("ssh:transfer-retry not implemented");
  });

  ipcMain.handle("ssh:transfer-retry-batch", async (_event, _transferIds: string[]) => {
    throw new Error("ssh:transfer-retry-batch not implemented");
  });

  ipcMain.handle("ssh:transfer-clear", async (_event, _transferIds: string[]) => {
    throw new Error("ssh:transfer-clear not implemented");
  });

  ipcMain.handle("ssh:transfer-clear-completed", async () => {
    throw new Error("ssh:transfer-clear-completed not implemented");
  });

  // ─── Transfer Settings ─────────────────────────────────────────

  ipcMain.handle("ssh:transfer-set-bandwidth", async (_event, _limitKBps: number) => {
    throw new Error("ssh:transfer-set-bandwidth not implemented");
  });

  ipcMain.handle("ssh:transfer-get-bandwidth", async () => {
    throw new Error("ssh:transfer-get-bandwidth not implemented");
  });

  ipcMain.handle("ssh:transfer-set-compression", async (_event, _enabled: boolean) => {
    throw new Error("ssh:transfer-set-compression not implemented");
  });

  ipcMain.handle("ssh:transfer-set-adaptive", async (_event, _enabled: boolean) => {
    throw new Error("ssh:transfer-set-adaptive not implemented");
  });

  // ─── Transfer Scheduling ───────────────────────────────────────

  ipcMain.handle(
    "ssh:transfer-schedule",
    async (
      _event,
      _opts: {
        profileId: string;
        direction: string;
        localPath: string;
        remotePath: string;
        scheduledAt: number;
      },
    ) => {
      throw new Error("ssh:transfer-schedule not implemented");
    },
  );

  ipcMain.handle("ssh:transfer-cancel-schedule", async (_event, _transferId: string) => {
    throw new Error("ssh:transfer-cancel-schedule not implemented");
  });

  ipcMain.handle("ssh:transfer-list-scheduled", async (_event, _profileId: string) => {
    throw new Error("ssh:transfer-list-scheduled not implemented");
  });

  // ─── Transfer History ──────────────────────────────────────────

  ipcMain.handle(
    "ssh:transfer-history",
    async (
      _event,
      _opts: {
        page: number;
        pageSize: number;
        search?: string;
        sortBy?: string;
        sortDir?: "asc" | "desc";
        profileId?: string;
      },
    ) => {
      throw new Error("ssh:transfer-history not implemented");
    },
  );

  ipcMain.handle("ssh:transfer-stats", async (_event, _profileId: string, _period: string) => {
    throw new Error("ssh:transfer-stats not implemented");
  });

  // ─── Workspace Sync ────────────────────────────────────────────

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
      if (!workspaceSyncEngine) {
        throw new Error("WorkspaceSyncEngine not available");
      }
      const syncPairId = workspaceSyncEngine.addSyncPair({
        profileId: opts.profileId,
        localDir: opts.localDir,
        remoteDir: opts.remoteDir,
        ignorePatterns: opts.ignorePatterns,
      });
      return { syncPairId };
    },
  );

  ipcMain.handle("ssh:sync-remove-pair", async (_event, syncPairId: string) => {
    if (!workspaceSyncEngine) {
      throw new Error("WorkspaceSyncEngine not available");
    }
    await workspaceSyncEngine.removeSyncPair(syncPairId);
  });

  ipcMain.handle("ssh:sync-start", async (_event, syncPairId: string) => {
    if (!workspaceSyncEngine) {
      throw new Error("WorkspaceSyncEngine not available");
    }
    await workspaceSyncEngine.start(syncPairId);
  });

  ipcMain.handle("ssh:sync-pause", async (_event, syncPairId: string) => {
    if (!workspaceSyncEngine) {
      throw new Error("WorkspaceSyncEngine not available");
    }
    workspaceSyncEngine.pause(syncPairId);
  });

  ipcMain.handle("ssh:sync-list-pairs", async (_event, profileId: string) => {
    if (!workspaceSyncEngine) {
      throw new Error("WorkspaceSyncEngine not available");
    }
    return workspaceSyncEngine.listSyncPairs(profileId);
  });

  ipcMain.handle("ssh:sync-list-conflicts", async (_event, syncPairId: string) => {
    if (!workspaceSyncEngine) {
      throw new Error("WorkspaceSyncEngine not available");
    }
    return workspaceSyncEngine.listConflicts(syncPairId);
  });

  ipcMain.handle("ssh:sync-resolve-conflict", async (_event, conflictId: string, resolution: string) => {
    if (!workspaceSyncEngine) {
      throw new Error("WorkspaceSyncEngine not available");
    }
    workspaceSyncEngine.resolveConflict(conflictId, resolution as "local" | "remote" | "both");
  });

  ipcMain.handle(
    "ssh:sync-validate-local-dir",
    async (_event, opts: { localDir: string; profileId: string }) => {
      const { localDir, profileId } = opts;

      // Check if the directory exists
      if (!fs.existsSync(localDir)) {
        return { valid: false, error: "Directory does not exist" };
      }

      // Check if the directory is writable
      try {
        fs.accessSync(localDir, fs.constants.W_OK);
      } catch {
        return { valid: false, error: "Directory is not writable" };
      }

      // Check if the directory is already part of an existing sync pair
      if (workspaceSyncEngine) {
        const existingPairs = workspaceSyncEngine.listSyncPairs(profileId);
        const duplicate = existingPairs.find((pair) => pair.localDir === localDir);
        if (duplicate) {
          return { valid: false, error: "Directory is already part of an existing sync pair" };
        }
      }

      return { valid: true };
    },
  );

  // ─── Process Management ────────────────────────────────────────

  ipcMain.handle("ssh:process-list", async (_event, _profileId: string) => {
    throw new Error("ssh:process-list not implemented");
  });

  ipcMain.handle("ssh:process-signal", async (_event, _profileId: string, _pid: number, _signal: string) => {
    throw new Error("ssh:process-signal not implemented");
  });

  // ─── Environment Management ────────────────────────────────────

  ipcMain.handle("ssh:env-list", async (_event, _profileId: string) => {
    throw new Error("ssh:env-list not implemented");
  });

  ipcMain.handle(
    "ssh:env-set",
    async (_event, _profileId: string, _name: string, _value: string, _persist: boolean) => {
      throw new Error("ssh:env-set not implemented");
    },
  );

  ipcMain.handle("ssh:env-delete", async (_event, _profileId: string, _name: string) => {
    throw new Error("ssh:env-delete not implemented");
  });

  ipcMain.handle("ssh:path-list", async (_event, _profileId: string) => {
    throw new Error("ssh:path-list not implemented");
  });

  ipcMain.handle("ssh:path-update", async (_event, _profileId: string, _entries: string[]) => {
    throw new Error("ssh:path-update not implemented");
  });

  // ─── Tool Management ───────────────────────────────────────────

  ipcMain.handle("ssh:tool-detect-managers", async (_event, _profileId: string) => {
    throw new Error("ssh:tool-detect-managers not implemented");
  });

  ipcMain.handle(
    "ssh:tool-install",
    async (_event, _profileId: string, _manager: string, _tool: string, _sudo: boolean) => {
      throw new Error("ssh:tool-install not implemented");
    },
  );

  ipcMain.handle("ssh:tool-install-cancel", async (_event, _streamId: string) => {
    throw new Error("ssh:tool-install-cancel not implemented");
  });

  // ─── Connection Quality ────────────────────────────────────────

  ipcMain.handle("ssh:quality-get", async (_event, _profileId: string) => {
    throw new Error("ssh:quality-get not implemented");
  });

  // ─── Bookmarks & Recents ───────────────────────────────────────

  ipcMain.handle("ssh:bookmark-add", async (_event, _profileId: string, _path: string, _label: string) => {
    throw new Error("ssh:bookmark-add not implemented");
  });

  ipcMain.handle("ssh:bookmark-remove", async (_event, _bookmarkId: string) => {
    throw new Error("ssh:bookmark-remove not implemented");
  });

  ipcMain.handle("ssh:bookmark-list", async (_event, _profileId: string) => {
    throw new Error("ssh:bookmark-list not implemented");
  });

  ipcMain.handle("ssh:recent-list", async (_event, _profileId: string) => {
    throw new Error("ssh:recent-list not implemented");
  });

  // ─── Security ──────────────────────────────────────────────────

  ipcMain.handle("ssh:transfer-security", async (_event, _profileId: string) => {
    throw new Error("ssh:transfer-security not implemented");
  });

  // ─── Capabilities ──────────────────────────────────────────────

  ipcMain.handle("ssh:capabilities", async (_event, profileId: string) => {
    if (!capabilityNegotiator) {
      throw new Error("ssh:capabilities not available — CapabilityNegotiator not provided");
    }
    return capabilityNegotiator.negotiate(profileId);
  });

  // ─── Agent Deployment ──────────────────────────────────────────

  ipcMain.handle("ssh:deploy-agent", async (_event, profileId: string) => {
    if (!sessionManager) throw new Error("ssh:deploy-agent not available — SessionManager not provided");
    await sessionManager.deployAgent(profileId);
  });

  ipcMain.handle("ssh:agent-status", async (_event, profileId: string) => {
    if (!capabilityNegotiator) {
      throw new Error("ssh:agent-status not available — CapabilityNegotiator not provided");
    }
    const cached = capabilityNegotiator.getCached(profileId);
    if (!cached) {
      return { deployed: false };
    }
    return {
      deployed: true,
      version: cached.version,
      capabilities: cached,
    };
  });

  // ─── Remote Workspace IDE ──────────────────────────────────────

  ipcMain.handle("ssh:workspace-open", async (_event, _profileId: string, _remotePath: string) => {
    throw new Error("ssh:workspace-open not implemented");
  });

  ipcMain.handle(
    "ssh:workspace-search",
    async (_event, _profileId: string, _query: string, _opts: { regex?: boolean; caseSensitive?: boolean; maxResults?: number }) => {
      throw new Error("ssh:workspace-search not implemented");
    },
  );

  ipcMain.handle("ssh:workspace-exec", async (_event, _profileId: string, _command: string, _cwd: string) => {
    throw new Error("ssh:workspace-exec not implemented");
  });

  ipcMain.handle("ssh:workspace-exec-kill", async (_event, _streamId: string) => {
    throw new Error("ssh:workspace-exec-kill not implemented");
  });

  ipcMain.handle("ssh:workspace-tail", async (_event, _profileId: string, _filePath: string) => {
    throw new Error("ssh:workspace-tail not wired — see RemoteWorkspaceManager.tailFile()");
  });

  ipcMain.handle("ssh:workspace-tail-stop", async (_event, _streamId: string) => {
    throw new Error("ssh:workspace-tail-stop not wired — see RemoteWorkspaceManager.stopTail()");
  });

  // ─── Remote Git ────────────────────────────────────────────────

  ipcMain.handle("ssh:git-detect", async (_event, _profileId: string, _path: string) => {
    throw new Error("ssh:git-detect not implemented");
  });

  ipcMain.handle("ssh:git-status", async (_event, _profileId: string, _repoPath: string) => {
    throw new Error("ssh:git-status not implemented");
  });

  ipcMain.handle(
    "ssh:git-operation",
    async (_event, _profileId: string, _op: { type: string; args?: Record<string, unknown> }) => {
      throw new Error("ssh:git-operation not implemented");
    },
  );

  // ─── Remote Debugging ──────────────────────────────────────────

  ipcMain.handle(
    "ssh:debug-start",
    async (_event, _profileId: string, _opts: { runtime: string; command: string; cwd: string; port: number }) => {
      throw new Error("ssh:debug-start not implemented");
    },
  );

  ipcMain.handle("ssh:debug-stop", async (_event, _sessionId: string) => {
    throw new Error("ssh:debug-stop not implemented");
  });
}

/**
 * Helper to broadcast a push event to all renderer windows.
 * Used by main-process modules to emit real-time updates.
 */
export function broadcastToRenderer(channel: SshEnhancedPushChannel, payload: unknown): void {
  const windows = BrowserWindow.getAllWindows();
  for (const win of windows) {
    win.webContents.send(channel, payload);
  }
}

/**
 * Wire the SshSessionManager's agent deployment events to the renderer.
 * Forwards deploy progress, agent capabilities, and agent unavailable
 * events to all renderer windows.
 *
 * Call once during app initialization after the session manager is created.
 */
export function bridgeAgentDeployToRenderer(sessionManager: SshSessionManager): void {
  sessionManager.on("ssh:deploy-progress", (payload: unknown) => {
    broadcastToRenderer("ssh:deploy-progress", payload);
  });

  sessionManager.on("ssh:agent-capabilities", (payload: unknown) => {
    broadcastToRenderer("ssh:agent-capabilities", payload);
  });

  sessionManager.on("ssh:agent-unavailable", (payload: unknown) => {
    broadcastToRenderer("ssh:agent-unavailable", payload);
  });
}

/**
 * Wire the WorkspaceSyncEngine and FileTransferManager events to the renderer.
 * Forwards sync status, conflict, error, and transfer progress events
 * to all renderer windows.
 *
 * Call once during app initialization after the sync engine and
 * file transfer manager are instantiated.
 */
export function bridgeSyncEventsToRenderer(
  syncEngine: WorkspaceSyncEngine,
  ftm: FileTransferManager,
): void {
  syncEngine.on("sync-status", (payload: unknown) => {
    broadcastToRenderer("ssh:sync-status", payload);
  });

  syncEngine.on("sync-conflict", (payload: unknown) => {
    broadcastToRenderer("ssh:sync-conflict", payload);
  });

  syncEngine.on("sync-error", (payload: unknown) => {
    broadcastToRenderer("ssh:sync-error", payload);
  });

  ftm.on("transfer:progress", (progress: { transferId: string; bytesTransferred: number; totalBytes: number; speed: number }) => {
    const transfer = ftm.getTransfer(progress.transferId);
    if (!transfer) return;

    broadcastToRenderer("ssh:sync-progress", {
      syncPairId: transfer.profileId,
      file: transfer.direction === "upload" ? transfer.localPath : transfer.remotePath,
      bytesTransferred: progress.bytesTransferred,
      totalBytes: progress.totalBytes,
      speed: progress.speed,
    });
  });
}
