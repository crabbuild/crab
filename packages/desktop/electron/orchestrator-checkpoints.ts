/**
 * Checkpoint Manager for the Crab Desktop Orchestrator.
 *
 * Provides per-turn git ref-based snapshots of workspace state.
 * Checkpoints are stored on the workspace host under
 * `refs/crab/checkpoints/<task-id>/<turn-number>` and allow
 * reverting a workspace to any previous agent turn.
 *
 * Each checkpoint captures the FULL working tree state — tracked +
 * untracked files, committed or not — by writing a temporary index
 * and creating a snapshot commit that is reachable only via the
 * checkpoint ref. This ensures that per-turn diffs have distinct
 * tree SHAs even when the agent does not commit its changes.
 */

import { randomUUID } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import type { Task } from "./orchestrator";
import { assertWorkspace } from "./orchestrator-worktree";
import { AgentProcessClosureError, runAgentCommand, spawnLocalAgent, type AgentProcessFactory } from "./agent-process";
import { parseRepoId } from "./ssh-types";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface RestoreBackup {
  ref: string;
  createdAt: number;
}

export interface Checkpoint {
  ref: string;
  turnNumber: number;
  sha: string;
}

// ─── CheckpointManager ──────────────────────────────────────────────────────

export class CheckpointManager {
  constructor(private readonly remote?: (task: Task, cwd: string) => AgentProcessFactory, private readonly signal?: AbortSignal) {}

  private readonly refPrefix = "refs/crab/checkpoints";
  private readonly pendingPrefix = "refs/crab/checkpoint-intents";
  private readonly basePrefix = "refs/crab/bases";
  private readonly restorePrefix = "refs/crab/restore-backups";

  private async command(task: Task, binary: string, args: string[], cwd: string, signal?: AbortSignal, timeoutMs = 30_000): Promise<{ stdout: string }> {
    if (!this.remote || !task.remoteProfileId) throw new Error("Remote checkpoints require a connected workspace owner");
    const repository = parseRepoId(task.repoPath);
    if (!repository || repository.profileId !== task.remoteProfileId) throw new Error("Remote checkpoint repository identity changed");
    const directory = cwd === task.repoPath ? repository.remotePath : cwd;
    const factory = this.remote(task, directory);
    return runAgentCommand(factory, binary, args, { cwd: directory, signal, timeoutMs });
  }

  private git(task: Task, args: string[], options: { cwd: string; env?: Record<string, string>; timeout?: number }): Promise<{ stdout: string }> {
    // Local Git keeps Node's existing unbounded default; task cancellation now
    // owns its whole group. Remote commands retain their transport deadline.
    // Metadata reads must not refresh the user's real index as a side effect.
    // Required locks for explicit add/restore/ref writes still apply.
    if (!task.isRemote) {
      const env = { GIT_OPTIONAL_LOCKS: "0", ...options.env };
      return runAgentCommand(spawnLocalAgent, "git", args, { cwd: options.cwd, env, signal: this.signal, timeoutMs: options.timeout });
    }
    const environment = ["GIT_OPTIONAL_LOCKS=0"];
    if (options.env?.GIT_INDEX_FILE) environment.push(`GIT_INDEX_FILE=${options.env.GIT_INDEX_FILE}`);
    if (options.env?.GIT_AUTHOR_NAME) {
      // Snapshot authors are product metadata. Never copy a local login's
      // environment or identity into the remote native process.
      environment.push("GIT_AUTHOR_NAME=crab-orchestrator", "GIT_AUTHOR_EMAIL=orchestrator@crab.local",
        "GIT_COMMITTER_NAME=crab-orchestrator", "GIT_COMMITTER_EMAIL=orchestrator@crab.local");
    }
    const command = [...environment, "git", ...args];
    return this.command(task, "env", command, options.cwd, this.signal, options.timeout);
  }

  /** Commits a completed owned turn on its workspace host. */
  async commitTurn(task: Task, turnNumber: number): Promise<boolean> {
    const options = { cwd: task.worktreePath, timeout: 10_000,
      env: { GIT_AUTHOR_NAME: "crab-agent", GIT_AUTHOR_EMAIL: "agent@crab.local",
        GIT_COMMITTER_NAME: "crab-agent", GIT_COMMITTER_EMAIL: "agent@crab.local" } };
    await this.git(task, ["add", "-A"], options);
    try { await this.git(task, ["diff", "--cached", "--quiet"], options); return false; } catch (error: unknown) {
      if ((error as { code?: unknown }).code !== 1) throw error;
    }
    await this.git(task, ["commit", "-m", `agent checkpoint: turn ${turnNumber}`, "--allow-empty-message"], options);
    return true;
  }

  /** Captures a pinned primary baseline; save its returned SHA as the task's base ref. */
  async captureBase(task: Task): Promise<string> {
    const sha = await this.snapshotHead(task);
    const baseline = { ...task, baseRef: sha };
    return this.pinBase(baseline);
  }

  /** Pins an existing baseline commit shared by conversations. */
  async pinBase(task: Task): Promise<string> {
    const { stdout } = await this.git(task, ["rev-parse", "--verify", `${task.baseRef}^{commit}`], { cwd: task.repoPath });
    const sha = stdout.trim();
    await this.git(task, ["update-ref", "--no-deref", `${this.basePrefix}/${sha}`, sha], { cwd: task.repoPath });
    return sha;
  }

  /**
   * Creates a checkpoint for the given task at the specified turn number.
   *
   * Captures a snapshot of the ENTIRE working tree — committed HEAD
   * plus any staged, unstaged, and untracked changes — as a dedicated
   * git commit reachable only via the checkpoint ref. This guarantees
   * that consecutive checkpoints have distinct tree SHAs whenever the
   * working tree changes, so per-turn `git diff from..to` is meaningful
   * even if the agent never calls `git commit`.
   *
   * The snapshot is built through a temporary index so the user's real
   * index (`.git/index`) stays untouched.
   *
   * Returns the ref name and the snapshot commit SHA.
   */
  async createCheckpoint(task: Task, turnNumber: number): Promise<{ ref: string; sha: string }> {
    const sha = await this.snapshotWorkingTree(
      task,
      `crab-checkpoint: turn ${turnNumber}`,
    );

    return this.recordCheckpoint(task, turnNumber, sha);
  }

  /** Publishes a captured pre-input snapshot after provider acceptance. */
  async recordCheckpoint(task: Task, turnNumber: number, sha: string): Promise<{ ref: string; sha: string }> {
    const ref = `${this.refPrefix}/${task.id}/${turnNumber}`;
    await this.git(task, ["update-ref", "--no-deref", ref, sha], { cwd: task.repoPath });
    return { ref, sha };
  }

  /** Checks whether the exact checkpoint exists, preserving repository errors. */
  async hasCheckpoint(task: Task, turnNumber: number): Promise<boolean> {
    const ref = `${this.refPrefix}/${task.id}/${turnNumber}`;
    const { stdout } = await this.git(task, ["for-each-ref", "--format=%(refname)", ref], { cwd: task.repoPath });
    if (!stdout.split("\n").includes(ref)) return false;
    await this.git(task, ["rev-parse", "--verify", `${ref}^{commit}`], { cwd: task.repoPath });
    return true;
  }

  /** Pins a pre-input snapshot until delivery and bookkeeping are settled. */
  async holdCheckpoint(task: Task, turnNumber: number, sha: string): Promise<void> {
    await this.git(task, ["update-ref", "--no-deref", `${this.pendingPrefix}/${task.id}/${turnNumber}`, sha], { cwd: task.repoPath });
  }

  /** Releases the pin after publication or a definitive input rejection. */
  async releaseCheckpoint(task: Task, turnNumber: number): Promise<void> {
    await this.git(task, ["update-ref", "--no-deref", "-d", `${this.pendingPrefix}/${task.id}/${turnNumber}`], { cwd: task.repoPath });
  }

  /**
   * Creates a lightweight checkpoint using the current HEAD commit directly.
   *
   * Unlike `createCheckpoint()`, this does NOT run `git add -A` or build
   * a temporary index. It simply points the checkpoint ref at HEAD. This
   * is appropriate for the first turn (turn 0) where the worktree was
   * just created from the base ref and is guaranteed to be clean — saving
   * 1-3 seconds on large repos.
   *
   * If the worktree has uncommitted changes (dirty), falls back to the
   * full snapshot path to ensure correctness.
   */
  async createLightweightCheckpoint(task: Task, turnNumber: number): Promise<{ ref: string; sha: string }> {
    const { stdout } = await this.git(task, ["status", "--porcelain", "--untracked-files=all"], { cwd: task.worktreePath });
    if (stdout.trim()) return this.createCheckpoint(task, turnNumber);
    const headSha = await this.headSha(task);
    if (!headSha) return this.createCheckpoint(task, turnNumber);
    return this.recordCheckpoint(task, turnNumber, headSha);
  }

  /**
   * Captures the current working tree (tracked + untracked, honouring
   * `.gitignore`) as a dangling commit object and returns its SHA.
   *
   * The commit is built through a temporary index so the user's real
   * `.git/index` is untouched, and no ref points to it — callers decide
   * whether to publish a ref or just use the SHA for an ephemeral diff.
   *
   * This is the shared primitive behind both `createCheckpoint` (which
   * publishes a ref) and `snapshotHead` (which returns the SHA only
   * for time-travel diffs against the live worktree).
   */
  async snapshotHead(task: Task): Promise<string> {
    return this.snapshotWorkingTree(
      task,
      "crab-checkpoint: live snapshot",
    );
  }

  private async headSha(task: Task): Promise<string | undefined> {
    try {
      const { stdout } = await this.git(task, ["rev-parse", "HEAD"], { cwd: task.worktreePath });
      const sha = stdout.trim();
      if (!sha) throw new Error("Git did not return a HEAD commit");
      return sha;
    } catch (error: unknown) {
      if (error instanceof AgentProcessClosureError || this.signal?.aborted) throw error;
      // An unborn branch has no parent commit. Other HEAD failures cannot
      // silently turn a damaged repository into a new root snapshot.
      const { stdout: symbolic } = await this.git(task, ["symbolic-ref", "HEAD"], { cwd: task.worktreePath });
      const ref = symbolic.trim();
      const { stdout: refs } = await this.git(task, ["for-each-ref", "--format=%(refname)", ref], { cwd: task.worktreePath });
      if (refs.split("\n").includes(ref)) throw error;
      return undefined;
    }
  }

  private async snapshotWorkingTree(task: Task, message: string): Promise<string> {
    const worktree = task.worktreePath;
    const headSha = await this.headSha(task);
    // Build a temp index that mirrors the current working tree.
    const filename = `crab-ckpt-index-${randomUUID()}`;
    const directory = task.isRemote
      ? (await this.git(task, ["rev-parse", "--absolute-git-dir"], { cwd: worktree })).stdout.trimEnd()
      : os.tmpdir();
    const paths = task.isRemote ? path.posix : path;
    const tmpIndex = paths.join(directory, filename);
    let uncertain = false;
    let failure: unknown;

    try {
      // The real index defines tracked files, including staged additions that
      // now match ignore rules. Seed from it without changing its contents.
      const { stdout: indexPath } = await this.git(task, ["rev-parse", "--git-path", "index"], { cwd: worktree });
      try {
        const original = paths.resolve(worktree, indexPath.trimEnd());
        if (task.isRemote) await this.command(task, "sh", ["-c", 'if [ -f "$1" ]; then cp "$1" "$2"; elif [ -e "$1" ]; then exit 1; fi', "crab-index", original, tmpIndex], worktree, this.signal);
        else fs.copyFileSync(original, tmpIndex);
      } catch (error: unknown) {
        // Git treats an absent index as empty, including on an unborn branch.
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }

      // Stage everything currently in the working tree (tracked + untracked).
      // `-A` picks up deletes too. Honours `.gitignore` by default, matching
      // how `git add .` behaves, so generated files we don't care about
      // (build outputs, node_modules) stay out of the snapshot.
      await this.git(task, ["add", "-A", "--", "."],
        { cwd: worktree, env: { GIT_INDEX_FILE: tmpIndex } },
      );

      // Write the index as a tree object.
      const { stdout: treeOut } = await this.git(task, ["write-tree"],
        { cwd: worktree, env: { GIT_INDEX_FILE: tmpIndex } },
      );
      const tree = treeOut.trim();

      // Commit the tree. Parent is HEAD if we have one; otherwise root commit.
      const commitArgs = ["commit-tree", tree, "-m", message];
      if (headSha) {
        commitArgs.push("-p", headSha);
      }
      const { stdout: commitOut } = await this.git(task, commitArgs,
        {
          cwd: worktree,
          env: {
            GIT_INDEX_FILE: tmpIndex,
            GIT_AUTHOR_NAME: process.env.GIT_AUTHOR_NAME ?? "crab-orchestrator",
            GIT_AUTHOR_EMAIL: process.env.GIT_AUTHOR_EMAIL ?? "orchestrator@crab.local",
            GIT_COMMITTER_NAME: process.env.GIT_COMMITTER_NAME ?? "crab-orchestrator",
            GIT_COMMITTER_EMAIL: process.env.GIT_COMMITTER_EMAIL ?? "orchestrator@crab.local",
          },
        },
      );
      const sha = commitOut.trim();
      if (!sha) throw new Error("Git did not return a checkpoint commit");
      return sha;
    } catch (error: unknown) {
      failure = error;
      uncertain = error instanceof AgentProcessClosureError;
      throw error;
    } finally {
      // An uncertain process can still use its index. Retain that file
      // until recovery establishes closure; never unlink beneath an owner.
      if (!uncertain) {
        try {
          // Cleanup deliberately has no cancelled signal: it belongs to this
          // operation and must finish before its physical owner is released.
          if (task.isRemote) await this.command(task, "rm", ["-f", tmpIndex, `${tmpIndex}.lock`], worktree);
          else for (const file of [tmpIndex, `${tmpIndex}.lock`]) fs.rmSync(file, { force: true });
        } catch (cleanup: unknown) {
          if (!failure) throw cleanup;
          const error = new AggregateError([failure, cleanup], "Snapshot failed and its private index needs cleanup");
          if (cleanup instanceof AgentProcessClosureError) throw new AgentProcessClosureError(error);
          throw error;
        }
      }
    }
  }

  /** Validates snapshot ownership and resolves its commit. */
  async snapshotSha(task: Task, sourceRef: string): Promise<string> {
    if (task.isRemote) throw new Error("Remote file restoration requires host workspace support");
    const sources = [...await this.listCheckpoints(task), ...await this.listRestoreBackups(task)];
    if (!sources.some(source => source.ref === sourceRef)) throw new Error("Snapshot does not belong to this conversation");
    await assertWorkspace(task);
    const { stdout } = await this.git(task, ["rev-parse", "--verify", `${sourceRef}^{commit}`], { cwd: task.repoPath });
    return stdout.trim();
  }

  /** Restores an owned snapshot's files after its caller drains workspace writers. */
  async restoreFiles(task: Task, sourceRef: string): Promise<void> {
    const sha = await this.snapshotSha(task, sourceRef);
    const { stdout: files } = await this.git(task, ["ls-tree", "-r", "--name-only", "-z", sha], { cwd: task.repoPath });
    const targets = files.split("\0").filter(Boolean);
    await this.assertNoIgnoredCollisions(task, targets);
    const before = await this.backupFiles(task);
    // Track the captured untracked files before replacing the index/tree, so
    // later additions are removed too. HEAD and conversation history stay intact.
    await this.git(task, ["read-tree", before], { cwd: task.worktreePath });
    await this.git(task, ["read-tree", "--reset", "-u", sha], { cwd: task.worktreePath });
  }

  /** Restores a changed file (and its rename source) after workspace writers close. */
  async revertFile(task: Task, filePath: string, oldPath?: string): Promise<void> {
    if (task.isRemote) throw new Error("Remote file restoration requires host workspace support");
    await assertWorkspace(task);
    const targets = [...new Set([filePath, ...(oldPath ? [oldPath] : [])])];
    const root = fs.realpathSync(task.worktreePath);
    for (const target of targets) {
      const relative = path.relative(root, path.resolve(root, target));
      if (!target || target.includes("\0") || path.isAbsolute(target) || relative === ".." || relative.startsWith(`..${path.sep}`)
        || !relative || target.split(/[\\/]/).some(part => part.toLowerCase() === ".git")) throw new Error("Revert requires a file inside the task worktree");
      // Git pathspecs also match directories. Never let a stale file row remove
      // a replacement directory or traverse a symbolic-link parent.
      const parts = relative.split(path.sep);
      for (let index = 0; index < parts.length; index++) {
        let entry: fs.Stats;
        try { entry = fs.lstatSync(path.join(root, ...parts.slice(0, index + 1))); } catch (error: unknown) {
          if ((error as NodeJS.ErrnoException).code === "ENOENT") break;
          throw error;
        }
        const leaf = index === parts.length - 1;
        if ((!leaf && !entry.isDirectory()) || (leaf && !entry.isFile() && !entry.isSymbolicLink())) throw new Error(`Cannot revert replaced file "${target}"`);
      }
    }
    await this.assertNoIgnoredCollisions(task, targets);
    const { stdout: source } = await this.git(task, ["rev-parse", "--verify", "--end-of-options", `${task.baseRef}^{commit}`], { cwd: task.repoPath });
    const sha = source.trim();
    const restore: string[] = [];
    const remove: string[] = [];
    for (const target of targets) {
      const { stdout: baseline } = await this.git(task, ["--literal-pathspecs", "ls-tree", "-z", sha, "--", target], { cwd: task.repoPath });
      if (baseline && !/^(100644|100755|120000) /.test(baseline)) throw new Error(`Revert requires a regular file or symbolic link: "${target}"`);
      const { stdout: tracked } = await this.git(task, ["--literal-pathspecs", "ls-files", "-z", "--", target], { cwd: task.worktreePath });
      if (baseline || tracked) restore.push(target);
      else remove.push(path.join(root, target));
    }
    await this.backupFiles(task);
    // Restore updates only these index entries; unrelated staging and HEAD
    // remain intact. Git cannot remove additions absent from both source/index.
    if (restore.length) await this.git(task, ["--literal-pathspecs", "restore", `--source=${sha}`, "--staged", "--worktree", "--no-recurse-submodules", "--", ...restore], { cwd: task.worktreePath });
    for (const target of remove) {
      try { fs.unlinkSync(target); } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    }
  }

  private async assertNoIgnoredCollisions(task: Task, targets: string[]): Promise<void> {
    const { stdout: excluded } = await this.git(task, ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "--no-empty-directory", "-z"], { cwd: task.worktreePath });
    // Ignored files are outside snapshot ownership. Git overwrites them during
    // checkout, so reject file/directory collisions before changing the index.
    for (const entry of excluded.split("\0").filter(Boolean)) {
      const ignored = entry.replace(/\/$/, "");
      if (targets.some(target => target === ignored || target.startsWith(`${ignored}/`) || ignored.startsWith(`${target}/`))) {
        throw new Error(`Move ignored path "${entry}" before restoring these files`);
      }
    }
  }

  private async backupFiles(task: Task): Promise<string> {
    const before = await this.snapshotHead(task);
    const backup = `${this.restorePrefix}/${task.id}/${Date.now()}-${randomUUID()}`;
    await this.git(task, ["update-ref", "--no-deref", backup, before], { cwd: task.repoPath });
    return before;
  }

  /** Lists retained file states from before each explicit restore. */
  async listRestoreBackups(task: Task): Promise<RestoreBackup[]> {
    const prefix = `${this.restorePrefix}/${task.id}/`;
    const refs = await this.getRefs(task, prefix);
    return refs.map(entry => ({ ref: entry.ref, createdAt: Number(entry.ref.slice(prefix.length).split("-")[0]) }))
      .sort((a, b) => b.createdAt - a.createdAt);
  }

  /**
   * Lists all checkpoints for a task, sorted by turn number ascending.
   */
  async listCheckpoints(task: Task): Promise<Checkpoint[]> {
    const prefix = `${this.refPrefix}/${task.id}/`;
    const refs = await this.getRefs(task, prefix);

    return refs
      .map((ref) => {
        const turnStr = ref.ref.slice(prefix.length);
        return {
          ref: ref.ref,
          turnNumber: parseInt(turnStr, 10),
          sha: ref.sha,
        };
      })
      .sort((a, b) => a.turnNumber - b.turnNumber);
  }

  /** Deletes captured checkpoint refs after workspace removal, preserving concurrent updates. */
  async cleanup(task: Task, keepBase = false): Promise<void> {
    const prefix = `${this.refPrefix}/${task.id}/`;
    const refs = [...await this.getRefs(task, prefix), ...await this.getRefs(task, `${this.pendingPrefix}/${task.id}/`),
      ...await this.getRefs(task, `${this.restorePrefix}/${task.id}/`),
      ...(!keepBase ? await this.getRefs(task, `${this.basePrefix}/${task.baseRef}`) : [])];
    for (const { ref, sha } of refs) {
      await this.git(task, ["update-ref", "--no-deref", "-d", ref, sha], { cwd: task.repoPath });
    }
  }

  /**
   * Lists all refs matching a given prefix using `git for-each-ref`.
   */
  private async getRefs(
    task: Task,
    prefix: string,
  ): Promise<Array<{ ref: string; sha: string }>> {
    const { stdout } = await this.git(task, ["for-each-ref", "--format=%(refname) %(objectname)", prefix],
      { cwd: task.repoPath },
    );

    if (!stdout.trim()) return [];

    return stdout
      .trim()
      .split("\n")
      .map((line) => {
        const [ref, sha] = line.split(" ");
        return { ref, sha };
      });
  }
}
