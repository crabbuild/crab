/**
 * Local Provider — RepoProvider implementation for repositories on the
 * user's local filesystem.
 *
 * Delegates RPC calls to the local crab-desktop-agent (stdio JSON-RPC),
 * file change monitoring to the shared metadata-worker owner, and file
 * system mutations to `node:fs`. This is the default provider when no
 * SSH prefix is present in the repo_id.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { shell } from "electron";

import { fullCapabilities, noCapabilities, type CapabilitySet, type RepositoryRuntime } from "./repo-capabilities";
import { Agent } from "./agent";
import { captureFilePublications, writeTextAtomically } from "./file-write";
import { readTextFile } from "./file-read";
import { localFileWatchers } from "./local-file-watcher";
import {
  BaseProvider,
  FsCreateResult,
  FsDeleteResult,
  FsReadResult,
  FsRenameResult,
  FsWriteResult,
} from "./repo-provider";

export class LocalProvider extends BaseProvider {
  private agent: Agent;
  private watchers = new Map<string, Promise<() => Promise<void>>>();
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
    const root = method === "git_status_porcelain" ? this.agent.worktreePath(String(params.repo_id)) : undefined;
    if (!root) return this.agent.request(method, params);
    const physical = await fs.promises.realpath(root);
    const snapshot = captureFilePublications(physical);
    let owned: Set<string>;
    let result: { entries: Array<{ path: string; code: string }> };
    try { result = await this.agent.request(method, params) as typeof result; }
    finally { owned = await snapshot.finish(); }
    return { ...result, entries: result.entries.filter(entry => entry.code !== "??" || !owned.has(path.join(physical, entry.path))) };
  }

  nativeSession(repoId: string): string | null { return this.agent.repositorySession(repoId); }

  // ── File Watching ──────────────────────────────────────────────

  async watchStart(repoId: string): Promise<void> {
    let pending = this.watchers.get(repoId);
    if (!pending) {
      const root = this.agent.worktreePath(repoId);
      if (!root) throw new Error("The repository has no physical working tree.");
      pending = localFileWatchers.watch(root, () => this.emit("fs/changed", { type: "fs/changed", repoPath: repoId }));
      this.watchers.set(repoId, pending);
    }
    try { await pending; }
    catch (error) { if (this.watchers.get(repoId) === pending) this.watchers.delete(repoId); throw error; }
  }

  async watchStop(repoId: string): Promise<void> {
    const pending = this.watchers.get(repoId);
    if (!pending) return;
    this.watchers.delete(repoId);
    await (await pending)();
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
    const written = await writeTextAtomically(fullPath, content);
    return { path: fullPath, written };
  }

  async fsRead(
    _repoId: string,
    fullPath: string,
    maxBytes?: number,
  ): Promise<FsReadResult> {
    return readTextFile(fullPath, maxBytes);
  }

  // ── Capabilities ───────────────────────────────────────────────

  async capabilities(repoId: string): Promise<CapabilitySet> {
    // Direct SDK snapshots have no physical workspace for Node filesystem or
    // CLI mutation services, even while their native readers remain available.
    if (!this.agent.hasWorktree(repoId)) return noCapabilities();
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

  async dispose(): Promise<void> {
    this.agent.removeListener("repository-runtime", this.onRuntime);
    await Promise.all([...this.watchers.keys()].map(repoId => this.watchStop(repoId)));
  }
}
