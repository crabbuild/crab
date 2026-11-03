import { autoUpdater, UpdateInfo, ProgressInfo } from "electron-updater";
import { BrowserWindow, ipcMain } from "electron";
import * as log from "electron-log";

// Configure electron-updater to use electron-log for diagnostics.
autoUpdater.logger = log;

/**
 * Auto-update lifecycle manager.
 *
 * Design choices:
 * - autoDownload disabled: the user decides when to download.
 * - autoInstallOnAppQuit enabled: once downloaded, install on next quit.
 * - All state changes are pushed to the renderer via IPC events so the
 *   UI can show a non-intrusive update banner.
 * - Check interval: once on launch + every 4 hours while running.
 */

let mainWindow: BrowserWindow | null = null;
let checkInterval: ReturnType<typeof setInterval> | null = null;

const CHECK_INTERVAL_MS = 4 * 60 * 60 * 1000; // 4 hours

function send(channel: string, payload: unknown) {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send(channel, payload);
  }
}

export function initAutoUpdater(win: BrowserWindow) {
  mainWindow = win;

  // Configuration
  autoUpdater.autoDownload = false;
  autoUpdater.autoInstallOnAppQuit = true;
  autoUpdater.allowDowngrade = false;

  // --- Events → renderer ---

  autoUpdater.on("checking-for-update", () => {
    send("updater:checking", {});
  });

  autoUpdater.on("update-available", (info: UpdateInfo) => {
    send("updater:available", {
      version: info.version,
      releaseDate: info.releaseDate,
      releaseNotes: info.releaseNotes,
    });
  });

  autoUpdater.on("update-not-available", (info: UpdateInfo) => {
    send("updater:not-available", { version: info.version });
  });

  autoUpdater.on("download-progress", (progress: ProgressInfo) => {
    send("updater:progress", {
      percent: progress.percent,
      bytesPerSecond: progress.bytesPerSecond,
      transferred: progress.transferred,
      total: progress.total,
    });
  });

  autoUpdater.on("update-downloaded", (info: UpdateInfo) => {
    send("updater:downloaded", {
      version: info.version,
      releaseDate: info.releaseDate,
      releaseNotes: info.releaseNotes,
    });
  });

  autoUpdater.on("error", (err: Error) => {
    send("updater:error", { message: err.message });
  });

  // --- IPC handlers (renderer → main) ---

  ipcMain.handle("updater:check", async () => {
    try {
      const result = await autoUpdater.checkForUpdates();
      return { success: true, version: result?.updateInfo?.version ?? null };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("updater:download", async () => {
    try {
      await autoUpdater.downloadUpdate();
      return { success: true };
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("updater:install", () => {
    // Quit and install immediately. This does not return.
    autoUpdater.quitAndInstall(/* isSilent */ false, /* isForceRunAfter */ true);
  });

  ipcMain.handle("updater:get-version", () => {
    const { app } = require("electron") as typeof import("electron");
    return app.getVersion();
  });

  // --- Periodic check ---

  // Initial check after a short delay (let the app finish loading).
  setTimeout(() => {
    autoUpdater.checkForUpdates().catch(() => {
      // Swallow — network may be unavailable at launch.
    });
  }, 10_000);

  checkInterval = setInterval(() => {
    autoUpdater.checkForUpdates().catch(() => {});
  }, CHECK_INTERVAL_MS);
}

/**
 * Cleanup — call on app quit to clear the interval.
 */
export function stopAutoUpdater() {
  if (checkInterval) {
    clearInterval(checkInterval);
    checkInterval = null;
  }
}
