/**
 * File system watcher for the Changes view.
 *
 * Uses Node's built-in `fs.watch` with `recursive: true` (backed by
 * FSEvents on macOS, ReadDirectoryChangesW on Windows). Debounces
 * rapid bursts of events into a single "fs/changed" notification so
 * the renderer can refresh its git status without hammering the agent.
 *
 * The watcher ignores `.git/` internals (except for index/refs changes
 * which signal staging operations from external tools) and common
 * build artifact directories.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import type { BrowserWindow } from "electron";
import { ipcMain } from "electron";
import { getProviderRegistry } from "./provider-registry";
import { parseRepoId } from "./ssh-types";

// Directories to ignore entirely — changes here don't affect git status.
const IGNORED_DIRS = new Set([
  "node_modules",
  ".git/objects",
  ".git/logs",
  ".git/hooks",
  ".git/info",
  ".crab/staging",
  ".crab/cache",
  ".crab/slatedb",
  "target",
  "__pycache__",
  ".venv",
  ".venv-crab-py",
  "dist",
  "build",
  ".next",
  ".turbo",
]);

// Files inside .git that DO signal meaningful state changes.
const GIT_INTERESTING = new Set([
  "index",
  "HEAD",
  "MERGE_HEAD",
  "REBASE_HEAD",
  "CHERRY_PICK_HEAD",
  "FETCH_HEAD",
  "ORIG_HEAD",
  "COMMIT_EDITMSG",
]);

interface WatcherState {
  watcher: fs.FSWatcher;
  debounceTimer: ReturnType<typeof setTimeout> | null;
  repoPath: string;
}

const watchers = new Map<string, WatcherState>();

// Debounce interval — coalesce rapid FS events into one notification.
// Set high enough that a `git fetch` updating 20 refs produces one
// event, not 20. The renderer adds its own per-view debounce on top.
const DEBOUNCE_MS = 350;

function shouldIgnore(relativePath: string): boolean {
  // Normalize separators for Windows compat.
  const normalized = relativePath.replace(/\\/g, "/");

  // Check if the path is inside an ignored directory.
  for (const dir of IGNORED_DIRS) {
    if (normalized.startsWith(dir + "/") || normalized === dir) {
      return true;
    }
  }

  // Inside .git/ — only pass through interesting files.
  if (normalized.startsWith(".git/")) {
    const basename = path.basename(normalized);
    // refs/heads changes signal branch updates.
    if (normalized.startsWith(".git/refs/")) return false;
    return !GIT_INTERESTING.has(basename);
  }

  return false;
}

function startWatching(repoPath: string, getWindow: () => BrowserWindow | null): void {
  // Don't double-watch.
  if (watchers.has(repoPath)) return;

  // Verify the path exists and is a directory.
  try {
    const stat = fs.statSync(repoPath);
    if (!stat.isDirectory()) return;
  } catch {
    return;
  }

  let debounceTimer: ReturnType<typeof setTimeout> | null = null;
  // Max-wait ceiling: even if events keep arriving, fire at least
  // once every MAX_WAIT_MS. Prevents infinite deferral during
  // continuous writes (e.g. a build tool).
  let firstEventAt: number | null = null;
  const MAX_WAIT_MS = 2000;

  const emitChange = () => {
    firstEventAt = null;
    const win = getWindow();
    if (!win || win.isDestroyed() || win.webContents.isDestroyed()) return;
    win.webContents.send("crab:event", {
      method: "fs/changed",
      params: { repo_path: repoPath },
    });
  };

  const watcher = fs.watch(
    repoPath,
    { recursive: true, persistent: false },
    (_eventType, filename) => {
      if (!filename) return;
      if (shouldIgnore(filename)) return;

      const now = Date.now();
      if (firstEventAt === null) firstEventAt = now;

      // If we've been deferring for too long, fire immediately.
      if (now - firstEventAt >= MAX_WAIT_MS) {
        if (debounceTimer) clearTimeout(debounceTimer);
        debounceTimer = null;
        emitChange();
        return;
      }

      // Debounce: reset the timer on each event, fire once after
      // the burst settles.
      if (debounceTimer) clearTimeout(debounceTimer);
      debounceTimer = setTimeout(emitChange, DEBOUNCE_MS);
    },
  );

  watcher.on("error", () => {
    // Watcher died (e.g. directory deleted). Clean up silently.
    stopWatching(repoPath);
  });

  watchers.set(repoPath, { watcher, debounceTimer, repoPath });
}

function stopWatching(repoPath: string): void {
  const state = watchers.get(repoPath);
  if (!state) return;
  if (state.debounceTimer) clearTimeout(state.debounceTimer);
  try {
    state.watcher.close();
  } catch {
    // Already closed — fine.
  }
  watchers.delete(repoPath);
}

function stopAll(): void {
  for (const repoPath of [...watchers.keys()]) {
    stopWatching(repoPath);
  }
}

/**
 * Register IPC handlers for the file watcher. Call once during app
 * startup.
 */
export function registerFileWatcher(getWindow: () => BrowserWindow | null): void {
  // Renderer tells us which repos to watch.
  ipcMain.handle("crab:fs-watch-start", async (_evt, repoPath: string) => {
    const registry = getProviderRegistry();

    // Require the remote provider before attempting any local watching.
    if (parseRepoId(repoPath)) {
      const provider = registry.resolve(repoPath);
      if (provider) {
        await provider.watchStart(repoPath);
        return { watching: true, path: repoPath };
      }
    }

    // Otherwise, use the local file watcher (existing behavior).
    startWatching(repoPath, getWindow);
    return { watching: true, path: repoPath };
  });

  ipcMain.handle("crab:fs-watch-stop", async (_evt, repoPath: string) => {
    const registry = getProviderRegistry();

    // Require the remote provider before attempting any local watching.
    if (parseRepoId(repoPath)) {
      const provider = registry.resolve(repoPath);
      if (provider) {
        await provider.watchStop(repoPath);
        return { watching: false, path: repoPath };
      }
    }

    // Otherwise, stop the local file watcher (existing behavior).
    stopWatching(repoPath);
    return { watching: false, path: repoPath };
  });

  // Clean up all watchers on app quit.
  process.on("exit", stopAll);
}

export { stopAll as stopAllWatchers };
