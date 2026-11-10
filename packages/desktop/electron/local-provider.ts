/**
 * Local Provider — RepoProvider implementation for repositories on the
 * user's local filesystem.
 *
 * Delegates RPC calls to the local crab-desktop-agent (stdio JSON-RPC),
 * file watching to Node's built-in recursive `fs.watch`, and file
 * system mutations to `node:fs`. This is the default provider when no
 * SSH prefix is present in the repo_id.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { shell } from "electron";

import { fullCapabilities, noCapabilities, type CapabilitySet } from "./repo-capabilities";
import { Agent, type RepositoryRuntime } from "./agent";
import {
  BaseProvider,
  FsCreateResult,
  FsDeleteResult,
  FsReadResult,
  FsRenameResult,
  FsWriteResult,
} from "./repo-provider";

// ── File Watcher Internals ───────────────────────────────────────
//
// Mirrors the debounce logic from file-watcher.ts: 350ms debounce
// with a 2s max-wait ceiling. Uses Node's built-in `fs.watch` with
// `recursive: true` (FSEvents on macOS, ReadDirectoryChangesW on
// Windows).

const DEBOUNCE_MS = 350;
const MAX_WAIT_MS = 2000;

/** Directories to ignore — changes here don't affect git status. */
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

/** Files inside .git that signal meaningful state changes. */
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
  firstEventAt: number | null;
}

function shouldIgnore(relativePath: string): boolean {
  const normalized = relativePath.replace(/\\/g, "/");

  for (const dir of IGNORED_DIRS) {
    if (normalized.startsWith(dir + "/") || normalized === dir) {
      return true;
    }
  }

  if (normalized.startsWith(".git/")) {
    const basename = path.basename(normalized);
    if (normalized.startsWith(".git/refs/")) return false;
    return !GIT_INTERESTING.has(basename);
  }

  return false;
}

// ── LocalProvider ────────────────────────────────────────────────

export class LocalProvider extends BaseProvider {
  private agent: Agent;
  private watchers = new Map<string, WatcherState>();
  private readonly onRuntime = (event: RepositoryRuntime) => {
    const nativeSessionId = this.agent.repositorySession(event.repoId);
    void this.capabilities(event.repoId).then(capabilities => {
      if (this.agent.repositorySession(event.repoId) !== nativeSessionId) return;
      this.emit("capabilities-changed", { type: "capabilities-changed", repoId: event.repoId, nativeSessionId, capabilities, error: event.error });
    });
  };

  constructor(agent: Agent) {
    super();
    this.agent = agent;
    agent.on("repository-runtime", this.onRuntime);
  }

  // ── Core RPC dispatch ──────────────────────────────────────────

  async request(method: string, params: Record<string, unknown>): Promise<unknown> {
    return this.agent.request(method, params);
  }

  nativeSession(repoId: string): string | null { return this.agent.repositorySession(repoId); }

  // ── File Watching ──────────────────────────────────────────────

  async watchStart(repoId: string): Promise<void> {
    // repoId for local repos is the absolute filesystem path.
    if (this.watchers.has(repoId)) return;

    try {
      const stat = fs.statSync(repoId);
      if (!stat.isDirectory()) return;
    } catch {
      return;
    }

    let debounceTimer: ReturnType<typeof setTimeout> | null = null;
    let firstEventAt: number | null = null;

    const emitChange = () => {
      const state = this.watchers.get(repoId);
      if (state) state.firstEventAt = null;
      this.emit("fs/changed", {
        type: "fs/changed" as const,
        repoPath: repoId,
      });
    };

    const watcher = fs.watch(
      repoId,
      { recursive: true, persistent: false },
      (_eventType, filename) => {
        if (!filename) return;
        if (shouldIgnore(filename)) return;

        const now = Date.now();
        const state = this.watchers.get(repoId);
        if (!state) return;

        if (state.firstEventAt === null) state.firstEventAt = now;

        // Max-wait ceiling: fire at least once every MAX_WAIT_MS.
        if (now - state.firstEventAt >= MAX_WAIT_MS) {
          if (state.debounceTimer) clearTimeout(state.debounceTimer);
          state.debounceTimer = null;
          emitChange();
          return;
        }

        // Debounce: reset timer on each event.
        if (state.debounceTimer) clearTimeout(state.debounceTimer);
        state.debounceTimer = setTimeout(emitChange, DEBOUNCE_MS);
      },
    );

    watcher.on("error", () => {
      this.watchStop(repoId);
    });

    this.watchers.set(repoId, { watcher, debounceTimer, firstEventAt });
  }

  async watchStop(repoId: string): Promise<void> {
    const state = this.watchers.get(repoId);
    if (!state) return;
    if (state.debounceTimer) clearTimeout(state.debounceTimer);
    try {
      state.watcher.close();
    } catch {
      // Already closed.
    }
    this.watchers.delete(repoId);
  }

  // ── File System Operations ─────────────────────────────────────

  async fsCreate(
    _repoId: string,
    fullPath: string,
    kind: "file" | "directory",
  ): Promise<FsCreateResult> {
    if (kind === "directory") {
      await fs.promises.mkdir(fullPath, { recursive: true });
    } else {
      const dir = path.dirname(fullPath);
      await fs.promises.mkdir(dir, { recursive: true });
      await fs.promises.writeFile(fullPath, "");
    }
    // Emit fs/changed so views refresh even if the file watcher hasn't
    // fired yet (e.g. directory-only creation on some platforms).
    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: _repoId });
    return { path: fullPath, kind };
  }

  async fsRename(
    _repoId: string,
    oldPath: string,
    newPath: string,
  ): Promise<FsRenameResult> {
    const dir = path.dirname(newPath);
    await fs.promises.mkdir(dir, { recursive: true });
    await fs.promises.rename(oldPath, newPath);
    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: _repoId });
    return { oldPath, newPath };
  }

  async fsDelete(
    _repoId: string,
    fullPath: string,
    useTrash?: boolean,
  ): Promise<FsDeleteResult> {
    const shouldTrash = useTrash ?? true;
    if (shouldTrash) {
      await shell.trashItem(fullPath);
    } else {
      const stat = await fs.promises.stat(fullPath);
      if (stat.isDirectory()) {
        await fs.promises.rm(fullPath, { recursive: true, force: true });
      } else {
        await fs.promises.unlink(fullPath);
      }
    }
    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: _repoId });
    return { deleted: fullPath, trash: shouldTrash };
  }

  async fsWrite(
    _repoId: string,
    fullPath: string,
    content: string,
  ): Promise<FsWriteResult> {
    const dir = path.dirname(fullPath);
    await fs.promises.mkdir(dir, { recursive: true });
    await fs.promises.writeFile(fullPath, content, "utf8");
    return { path: fullPath, written: content.length };
  }

  async fsRead(
    _repoId: string,
    fullPath: string,
  ): Promise<FsReadResult> {
    try {
      const stat = await fs.promises.stat(fullPath);
      if (stat.isDirectory()) {
        return { exists: true, content: null };
      }
      const content = await fs.promises.readFile(fullPath, "utf8");
      return { exists: true, content };
    } catch (err: unknown) {
      if (err && typeof err === "object" && "code" in err && err.code === "ENOENT") {
        return { exists: false, content: null };
      }
      throw err;
    }
  }

  // ── Capabilities ───────────────────────────────────────────────

  async capabilities(repoId: string): Promise<CapabilitySet> {
    if (!this.agent.hasRepository(repoId)) return noCapabilities();
    const available = this.agent.repositorySession(repoId) !== null;
    const caps = available ? fullCapabilities() : noCapabilities();
    // Node filesystem services survive sidecar exit, so editor buffers can
    // still save. Native features wait for this repository's recovered session.
    caps.canEdit = true;
    caps.canWatch = true;
    caps.canMaterialize = available && this.agent.supports("file_open", "file_materialize", "file_close");
    caps.canHydrate = available && this.agent.supports("hydrate", "dehydrate");
    // Electron's shell.trashItem works on macOS and Windows but is
    // unreliable on Linux (depends on desktop environment).
    caps.canTrash = process.platform !== "linux";
    return caps;
  }

  // ── Lifecycle ──────────────────────────────────────────────────

  dispose(): void {
    this.agent.removeListener("repository-runtime", this.onRuntime);
    for (const repoId of [...this.watchers.keys()]) {
      this.watchStop(repoId);
    }
  }
}
