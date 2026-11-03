import { ipcMain, BrowserWindow } from "electron";
import { LspManager } from "./lsp-manager";
import { discoverServers } from "./lsp-discovery";
import { lspInstaller, type InstallProgress } from "./lsp-installer";

/**
 * Register LSP-related IPC handlers. Wires the renderer to the
 * LspManager for starting/stopping servers, sending messages, and
 * receiving server events.
 */
export function registerLspIpc(
  manager: LspManager,
  getMainWindow: () => BrowserWindow | null,
): void {
  // Start a language server for a given language + workspace root
  ipcMain.handle("lsp:start", async (_evt, language: string, rootPath: string) => {
    return manager.start(language, rootPath);
  });

  // Send a raw JSON message from renderer to a running server
  ipcMain.handle("lsp:send", async (_evt, language: string, message: string) => {
    manager.sendToServer(language, message);
  });

  // Stop a specific language server
  ipcMain.handle("lsp:stop", async (_evt, language: string) => {
    await manager.stop(language);
  });

  // Get discovery results (available servers on the system)
  ipcMain.handle("lsp:discover", async () => {
    return discoverServers();
  });

  // Get status of all configured/running servers
  ipcMain.handle("lsp:status", async () => {
    return manager.status();
  });

  // Install a language server on-demand (downloads to ~/.crab/lsp/)
  ipcMain.handle("lsp:install", async (_evt, language: string) => {
    return manager.installLanguage(language);
  });

  // Check whether auto-install is supported for a language
  ipcMain.handle("lsp:install-supported", async (_evt, language: string) => {
    return lspInstaller.isSupported(language);
  });

  // Wire server → renderer message relay
  manager.on("message", (payload: { language: string; message: string }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("lsp:message", payload);
    }
  });

  // Wire status change events to renderer
  manager.on("status", (status: unknown) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("lsp:status", status);
    }
  });

  // Wire installer progress events to renderer
  lspInstaller.on("progress", (progress: InstallProgress) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("lsp:install-progress", progress);
    }
  });
}
