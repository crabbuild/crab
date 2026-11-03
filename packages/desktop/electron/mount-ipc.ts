/**
 * Electron IPC handlers for VFS mount operations.
 *
 * Registers IPC handlers that the renderer can invoke to manage
 * FUSE mounts via the coordinator. Each handler delegates to the
 * mount IPC client which communicates over the Unix socket.
 *
 * Channels:
 *   Invoke (renderer → main):
 *     - mount:list     — list all active mounts
 *     - mount:mount    — mount a repository
 *     - mount:unmount  — unmount a mountpoint
 *     - mount:status   — get status of a specific mount
 *     - mount:refresh  — refresh a mount (re-fetch)
 *     - mount:switch   — switch a mount to a different ref
 */

import { ipcMain } from "electron";
import {
  mountRepo,
  unmountRepo,
  listMounts,
  getMountStatus,
  refreshMount,
  switchMountRef,
  MountIpcError,
  MountIpcErrorCode,
  type MountOptions,
} from "./mount-ipc-client";

/** Result shape returned to the renderer for all mount IPC calls. */
interface MountIpcResult {
  ok: boolean;
  error?: string;
  errorCode?: MountIpcErrorCode;
  data?: unknown;
}

/**
 * Wrap a mount IPC operation, catching errors and returning a
 * consistent result shape to the renderer.
 */
async function wrapMountOp<T>(fn: () => Promise<T>): Promise<MountIpcResult> {
  try {
    const data = await fn();
    return { ok: true, data };
  } catch (err) {
    if (err instanceof MountIpcError) {
      return { ok: false, error: err.message, errorCode: err.code };
    }
    const message = err instanceof Error ? err.message : String(err);
    return { ok: false, error: message };
  }
}

/**
 * Register all mount-related IPC handlers.
 *
 * Call once during app initialization (in main.ts).
 */
export function registerMountIpc(): void {
  // List all active mounts
  ipcMain.handle("mount:list", async () => {
    return wrapMountOp(() => listMounts());
  });

  // Mount a repository
  ipcMain.handle(
    "mount:mount",
    async (_event, params: MountOptions) => {
      return wrapMountOp(() => mountRepo(params));
    },
  );

  // Unmount a mountpoint
  ipcMain.handle(
    "mount:unmount",
    async (_event, params: { mountpoint: string }) => {
      return wrapMountOp(() => unmountRepo(params.mountpoint));
    },
  );

  // Get status of a specific mount
  ipcMain.handle(
    "mount:status",
    async (_event, params: { mountpoint: string }) => {
      return wrapMountOp(() => getMountStatus(params.mountpoint));
    },
  );

  // Refresh a mount (re-fetch from remote)
  ipcMain.handle(
    "mount:refresh",
    async (_event, params: { mountpoint: string }) => {
      return wrapMountOp(() => refreshMount(params.mountpoint));
    },
  );

  // Switch a mount to a different ref/branch
  ipcMain.handle(
    "mount:switch",
    async (_event, params: { mountpoint: string; ref: string }) => {
      return wrapMountOp(() => switchMountRef(params.mountpoint, params.ref));
    },
  );
}
