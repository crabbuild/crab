import type { BrowserWindow } from "electron";
import { ipcMain } from "electron";
import { getProviderRegistry } from "./provider-registry";
import { parseRepoId } from "./ssh-types";
import { localFileWatchers } from "./local-file-watcher";

const subscriptions = new Map<string, Promise<() => Promise<void>>>();

/** Route watch requests independently of native repository readiness. */
export function registerFileWatcher(getWindow: () => BrowserWindow | null): void {
  ipcMain.handle("crab:fs-watch-start", async (_event, repoPath: string) => {
    if (parseRepoId(repoPath)) {
      const provider = getProviderRegistry().resolve(repoPath);
      if (!provider) throw new Error("Repository file services are unavailable.");
      await provider.watchStart(repoPath);
    } else {
      let pending = subscriptions.get(repoPath);
      if (!pending) {
        pending = localFileWatchers.watch(repoPath, () => {
          const window = getWindow();
          if (!window || window.isDestroyed() || window.webContents.isDestroyed()) return;
          window.webContents.send("crab:event", { method: "fs/changed", params: { repo_path: repoPath } });
        });
        subscriptions.set(repoPath, pending);
      }
      try { await pending; }
      catch (error) { if (subscriptions.get(repoPath) === pending) subscriptions.delete(repoPath); throw error; }
    }
    return { watching: true, path: repoPath };
  });

  ipcMain.handle("crab:fs-watch-stop", async (_event, repoPath: string) => {
    if (parseRepoId(repoPath)) {
      const provider = getProviderRegistry().resolve(repoPath);
      if (!provider) throw new Error("Repository file services are unavailable.");
      await provider.watchStop(repoPath);
    } else {
      const pending = subscriptions.get(repoPath);
      subscriptions.delete(repoPath);
      if (pending) await (await pending)();
    }
    return { watching: false, path: repoPath };
  });
}

/** Join local watcher owners before the application exits. */
export function stopAllWatchers(): Promise<void> {
  subscriptions.clear();
  return localFileWatchers.stopAll();
}
