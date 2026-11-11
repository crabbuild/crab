import { posix } from "node:path";
import type { SFTPWrapper, FileEntryWithStats } from "ssh2";
import type { SshSessionManager } from "./ssh-manager";
import type { ProviderRegistry } from "./provider-registry";
import type { RepoProvider } from "./repo-provider";

export interface RemoteDirEntry {
  name: string;
  path: string;
  kind: "file" | "dir" | "pointer" | "lfs" | "submodule";
  size: number | null;
  pointer_hash: string | null;
}
export interface BrowseResult { entries: RemoteDirEntry[]; path: string; hasGitDir: boolean }

export class RemoteFileBrowser {
  constructor(
    private readonly sessionManager: SshSessionManager,
    private readonly registry: ProviderRegistry,
    private readonly remoteProvider: RepoProvider,
  ) {}

  private async sftp(profileId: string): Promise<SFTPWrapper> {
    if (this.sessionManager.getStatus(profileId) !== "connected") throw new Error("SSH connection is not ready");
    return this.sessionManager.getSftp(profileId);
  }

  async getHomeDir(profileId: string): Promise<string> {
    const sftp = await this.sftp(profileId);
    return new Promise((resolve, reject) => sftp.realpath(".", (error, result) => error ? reject(error) : resolve(result)));
  }

  async browsePath(profileId: string, remotePath: string): Promise<BrowseResult> {
    if (remotePath.includes("\0")) throw new Error("Invalid remote path");
    const sftp = await this.sftp(profileId);
    const directory = await new Promise<string>((resolve, reject) => sftp.realpath(remotePath, (error, result) => error ? reject(error) : resolve(result)));
    const listing = await new Promise<FileEntryWithStats[]>((resolve, reject) => sftp.readdir(directory, (error, entries) => error ? reject(error) : resolve(entries)));
    const entries: RemoteDirEntry[] = listing.filter(entry => entry.filename !== "." && entry.filename !== "..").map(entry => ({
      name: entry.filename, path: posix.join(directory, entry.filename), kind: entry.attrs.isDirectory() ? "dir" : "file",
      size: Number.isSafeInteger(entry.attrs.size) ? entry.attrs.size : null, pointer_hash: null,
    }));
    // This is a navigation hint; open_local performs the authoritative Git open.
    // Worktrees use a .git file, while bare repositories expose HEAD/objects.
    const hasGitDir = listing.some(entry => entry.filename === ".git") ||
      (listing.some(entry => entry.filename === "HEAD") && listing.some(entry => entry.filename === "objects" && entry.attrs.isDirectory()));
    return { entries, path: directory, hasGitDir };
  }

  async openAsRepo(profileId: string, remotePath: string): Promise<unknown> {
    const result = await this.sessionManager.openRepository(profileId, remotePath);
    const repoId = result.repo_id as string;
    this.registry.register(repoId, this.remoteProvider);
    await this.remoteProvider.watchStart(repoId);
    return { ...result, capabilities: await this.remoteProvider.capabilities(repoId) };
  }
}
