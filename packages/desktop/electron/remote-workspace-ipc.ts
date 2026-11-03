/**
 * IPC bridge for the RemoteWorkspaceOrchestrator.
 *
 * Registers IPC handlers for remote workspace operations (renderer → main)
 * and relays orchestrator events to the renderer (main → renderer).
 *
 * Channels:
 *   Invoke (renderer → main):
 *     - remote-workspace:discover
 *     - remote-workspace:open
 *     - remote-workspace:close
 *     - remote-workspace:refresh
 *
 *   Push (main → renderer):
 *     - remote-workspace:discovered
 *     - remote-workspace:status-update
 */

import { ipcMain, BrowserWindow } from "electron";
import type {
  RemoteWorkspaceOrchestrator,
  DiscoveryOptions,
} from "./remote-workspace-orchestrator";

/**
 * Register all remote-workspace IPC handlers and wire orchestrator
 * events to the renderer. Call once during app initialization.
 */
export function registerRemoteWorkspaceIpc(
  orchestrator: RemoteWorkspaceOrchestrator,
): void {
  // ─── Command Handlers (Renderer → Main) ──────────────────────────

  ipcMain.handle(
    "remote-workspace:discover",
    async (
      _event,
      params: { profileId: string; rootPath: string; options?: DiscoveryOptions },
    ) => {
      try {
        const repos = await orchestrator.discoverRepos(
          params.profileId,
          params.rootPath,
          params.options,
        );
        return { ok: true, repos };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { ok: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "remote-workspace:open",
    async (
      _event,
      params: { profileId: string; rootPath: string; options?: DiscoveryOptions },
    ) => {
      try {
        const workspace = await orchestrator.openWorkspace(
          params.profileId,
          params.rootPath,
          params.options,
        );
        return { ok: true, workspace };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { ok: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "remote-workspace:close",
    async (_event, params: { profileId: string }) => {
      try {
        orchestrator.closeWorkspace(params.profileId);
        return { ok: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { ok: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "remote-workspace:refresh",
    async (
      _event,
      params: { profileId: string; rootPath: string; options?: DiscoveryOptions },
    ) => {
      try {
        // Re-discover repos (same as discover but updates the open workspace)
        const repos = await orchestrator.discoverRepos(
          params.profileId,
          params.rootPath,
          params.options,
        );

        // Update the open workspace's repo list if one exists
        const workspace = orchestrator.getWorkspace(params.profileId);
        if (workspace) {
          workspace.repos = repos;
          // Restart polling with the updated repo list
          const repoPaths = repos.map((r) => r.absolute_path);
          orchestrator.startPolling(params.profileId, repoPaths);
        }

        return { ok: true, repos };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { ok: false, error: message };
      }
    },
  );

  // ─── Event Relay (Main → Renderer) ───────────────────────────────

  orchestrator.on(
    "discovered",
    (payload: { profileId: string; rootPath: string; repos: unknown[] }) => {
      const windows = BrowserWindow.getAllWindows();
      for (const win of windows) {
        if (!win.isDestroyed()) {
          win.webContents.send("remote-workspace:discovered", payload);
        }
      }
    },
  );

  orchestrator.on(
    "status-update",
    (payload: { profileId: string; statuses: unknown[]; poll_timestamp: number }) => {
      const windows = BrowserWindow.getAllWindows();
      for (const win of windows) {
        if (!win.isDestroyed()) {
          win.webContents.send("remote-workspace:status-update", payload);
        }
      }
    },
  );
}
