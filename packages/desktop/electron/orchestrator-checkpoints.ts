/**
 * Checkpoint Manager for the Crab Desktop Orchestrator.
 *
 * Provides per-turn git ref-based snapshots of workspace state.
 * Checkpoints are stored as local refs under
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
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import type { Task } from "./orchestrator";

const execFile = promisify(execFileCb);

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
  private readonly refPrefix = "refs/crab/checkpoints";
  private readonly pendingPrefix = "refs/crab/checkpoint-intents";
  private readonly basePrefix = "refs/crab/bases";
  private readonly restorePrefix = "refs/crab/restore-backups";

  /** Captures a pinned primary baseline; save its returned SHA as the task's base ref. */
  async captureBase(task: Task): Promise<string> {
    const sha = await this.snapshotHead(task);
    const baseline = { repoPath: task.repoPath, baseRef: sha };
    return this.pinBase(baseline);
  }

  /** Pins an existing baseline commit shared by local conversations. */
  async pinBase(task: Pick<Task, "repoPath" | "baseRef">): Promise<string> {
    const { stdout } = await execFile("git", ["rev-parse", "--verify", `${task.baseRef}^{commit}`], { cwd: task.repoPath });
    const sha = stdout.trim();
    await execFile("git", ["update-ref", `${this.basePrefix}/${sha}`, sha], { cwd: task.repoPath });
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
    await execFile("git", ["update-ref", ref, sha], { cwd: task.repoPath });
    return { ref, sha };
  }

  /** Checks whether the exact checkpoint exists, preserving repository errors. */
  async hasCheckpoint(task: Task, turnNumber: number): Promise<boolean> {
    const ref = `${this.refPrefix}/${task.id}/${turnNumber}`;
    const { stdout } = await execFile("git", ["for-each-ref", "--format=%(refname)", ref], { cwd: task.repoPath });
    if (!stdout.split("\n").includes(ref)) return false;
    await execFile("git", ["rev-parse", "--verify", `${ref}^{commit}`], { cwd: task.repoPath });
    return true;
  }

  /** Pins a pre-input snapshot until delivery and bookkeeping are settled. */
  async holdCheckpoint(task: Task, turnNumber: number, sha: string): Promise<void> {
    await execFile("git", ["update-ref", `${this.pendingPrefix}/${task.id}/${turnNumber}`, sha], { cwd: task.repoPath });
  }

  /** Releases the pin after publication or a definitive input rejection. */
  async releaseCheckpoint(task: Task, turnNumber: number): Promise<void> {
    await execFile("git", ["update-ref", "-d", `${this.pendingPrefix}/${task.id}/${turnNumber}`], { cwd: task.repoPath });
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
    const worktree = task.worktreePath;

    // Check if the worktree is clean (no staged, unstaged, or untracked changes).
    let isClean = false;
    try {
      // `git status --porcelain` outputs nothing when the tree is clean.
      const { stdout } = await execFile(
        "git",
        ["status", "--porcelain", "--untracked-files=all"],
        { cwd: worktree },
      );
      isClean = stdout.trim().length === 0;
    } catch {
      isClean = false;
    }

    if (!isClean) {
      // Worktree is dirty — fall back to the full snapshot for correctness.
      return this.createCheckpoint(task, turnNumber);
    }

    // Worktree is clean — use HEAD directly.
    let headSha = "";
    try {
      const { stdout } = await execFile(
        "git",
        ["rev-parse", "HEAD"],
        { cwd: worktree },
      );
      headSha = stdout.trim();
    } catch {
      // No HEAD (empty repo) — fall back to full snapshot.
      return this.createCheckpoint(task, turnNumber);
    }

    if (!headSha) {
      return this.createCheckpoint(task, turnNumber);
    }

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

  private async snapshotWorkingTree(
    task: Task,
    message: string,
  ): Promise<string> {
    const worktree = task.worktreePath;

    // Resolve current HEAD — used as the parent of the snapshot commit.
    let headSha = "";
    try {
      const { stdout } = await execFile(
        "git",
        ["rev-parse", "HEAD"],
        { cwd: worktree },
      );
      headSha = stdout.trim();
    } catch (error: unknown) {
      // An unborn branch has no parent commit. Other HEAD failures cannot
      // silently turn a damaged repository into a new root snapshot.
      const { stdout: symbolic } = await execFile("git", ["symbolic-ref", "HEAD"], { cwd: worktree });
      const ref = symbolic.trim();
      const { stdout: refs } = await execFile("git", ["for-each-ref", "--format=%(refname)", ref], { cwd: worktree });
      if (refs.split("\n").includes(ref)) throw error;
    }

    // Build a temp index that mirrors the current working tree.
    const tmpIndex = path.join(
      os.tmpdir(),
      `crab-ckpt-index-${task.id}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    );

    try {
      // The real index defines tracked files, including staged additions that
      // now match ignore rules. Seed from it without changing its contents.
      const { stdout: indexPath } = await execFile("git", ["rev-parse", "--git-path", "index"], { cwd: worktree });
      try { fs.copyFileSync(path.resolve(worktree, indexPath.trim()), tmpIndex); } catch (error: unknown) {
        // Git treats an absent index as empty, including on an unborn branch.
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }

      // Stage everything currently in the working tree (tracked + untracked).
      // `-A` picks up deletes too. Honours `.gitignore` by default, matching
      // how `git add .` behaves, so generated files we don't care about
      // (build outputs, node_modules) stay out of the snapshot.
      await execFile(
        "git",
        ["add", "-A", "--", "."],
        { cwd: worktree, env: { ...process.env, GIT_INDEX_FILE: tmpIndex } },
      );

      // Write the index as a tree object.
      const { stdout: treeOut } = await execFile(
        "git",
        ["write-tree"],
        { cwd: worktree, env: { ...process.env, GIT_INDEX_FILE: tmpIndex } },
      );
      const tree = treeOut.trim();

      // Commit the tree. Parent is HEAD if we have one; otherwise root commit.
      const commitArgs = ["commit-tree", tree, "-m", message];
      if (headSha) {
        commitArgs.push("-p", headSha);
      }
      const { stdout: commitOut } = await execFile(
        "git",
        commitArgs,
        {
          cwd: worktree,
          env: {
            ...process.env,
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
    } finally {
      // Temp index lives outside the repo — remove it regardless of outcome.
      try { fs.unlinkSync(tmpIndex); } catch { /* ignore */ }
    }
  }

  /** Validates snapshot ownership and resolves its commit. */
  async snapshotSha(task: Task, sourceRef: string): Promise<string> {
    const sources = [...await this.listCheckpoints(task), ...await this.listRestoreBackups(task)];
    if (!sources.some(source => source.ref === sourceRef)) throw new Error("Snapshot does not belong to this conversation");
    await this.assertWorkspace(task);
    const { stdout } = await execFile("git", ["rev-parse", "--verify", `${sourceRef}^{commit}`], { cwd: task.repoPath });
    return stdout.trim();
  }

  private async assertWorkspace(task: Task): Promise<void> {
    const [root, common, repository, gitDir, repositoryGitDir] = await Promise.all([
      this.gitDirectory(task.worktreePath, "--show-toplevel"),
      this.gitDirectory(task.worktreePath, "--git-common-dir"),
      this.gitDirectory(task.repoPath, "--git-common-dir"),
      this.gitDirectory(task.worktreePath, "--absolute-git-dir"),
      this.gitDirectory(task.repoPath, "--absolute-git-dir"),
    ]);
    // Git searches parents when a checkout loses its .git pointer. A matching
    // directory name alone must never authorize replacing the primary index.
    if (root !== fs.realpathSync(task.worktreePath)) throw new Error("Task directory is not its Git worktree root");
    if (common !== repository) throw new Error("Workspace belongs to a different Git repository");
    if (root !== fs.realpathSync(task.repoPath) && gitDir === repositoryGitDir) throw new Error("Workspace shares the primary Git index");
  }

  private async gitDirectory(cwd: string, option: "--show-toplevel" | "--git-common-dir" | "--absolute-git-dir"): Promise<string> {
    const { stdout } = await execFile("git", ["rev-parse", option], { cwd });
    return fs.realpathSync(path.resolve(cwd, stdout.replace(/\r?\n$/, "")));
  }

  /** Restores an owned snapshot's files after its caller drains workspace writers. */
  async restoreFiles(task: Task, sourceRef: string): Promise<void> {
    const sha = await this.snapshotSha(task, sourceRef);
    const { stdout: files } = await execFile("git", ["ls-tree", "-r", "--name-only", "-z", sha], { cwd: task.repoPath });
    const targets = files.split("\0").filter(Boolean);
    await this.assertNoIgnoredCollisions(task, targets);
    const before = await this.backupFiles(task);
    // Track the captured untracked files before replacing the index/tree, so
    // later additions are removed too. HEAD and conversation history stay intact.
    await execFile("git", ["read-tree", before], { cwd: task.worktreePath });
    await execFile("git", ["read-tree", "--reset", "-u", sha], { cwd: task.worktreePath });
  }

  /** Restores a changed file (and its rename source) after workspace writers close. */
  async revertFile(task: Task, filePath: string, oldPath?: string): Promise<void> {
    await this.assertWorkspace(task);
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
    const { stdout: source } = await execFile("git", ["rev-parse", "--verify", "--end-of-options", `${task.baseRef}^{commit}`], { cwd: task.repoPath });
    const sha = source.trim();
    const restore: string[] = [];
    const remove: string[] = [];
    for (const target of targets) {
      const { stdout: baseline } = await execFile("git", ["--literal-pathspecs", "ls-tree", "-z", sha, "--", target], { cwd: task.repoPath });
      if (baseline && !/^(100644|100755|120000) /.test(baseline)) throw new Error(`Revert requires a regular file or symbolic link: "${target}"`);
      const { stdout: tracked } = await execFile("git", ["--literal-pathspecs", "ls-files", "-z", "--", target], { cwd: task.worktreePath });
      if (baseline || tracked) restore.push(target);
      else remove.push(path.join(root, target));
    }
    await this.backupFiles(task);
    // Restore updates only these index entries; unrelated staging and HEAD
    // remain intact. Git cannot remove additions absent from both source/index.
    if (restore.length) await execFile("git", ["--literal-pathspecs", "restore", `--source=${sha}`, "--staged", "--worktree", "--no-recurse-submodules", "--", ...restore], { cwd: task.worktreePath });
    for (const target of remove) {
      try { fs.unlinkSync(target); } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    }
  }

  private async assertNoIgnoredCollisions(task: Task, targets: string[]): Promise<void> {
    const { stdout: excluded } = await execFile("git", ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "--no-empty-directory", "-z"], { cwd: task.worktreePath });
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
    await execFile("git", ["update-ref", backup, before], { cwd: task.repoPath });
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

  /**
   * Deletes all checkpoint refs for a task. Errors on individual
   * ref deletions are silently ignored (ref may already be gone).
   */
  async cleanup(task: Task, keepBase = false): Promise<void> {
    const prefix = `${this.refPrefix}/${task.id}/`;
    const refs = [...await this.getRefs(task, prefix), ...await this.getRefs(task, `${this.pendingPrefix}/${task.id}/`),
      ...await this.getRefs(task, `${this.restorePrefix}/${task.id}/`),
      ...(!keepBase ? await this.getRefs(task, `${this.basePrefix}/${task.baseRef}`) : [])];

    for (const { ref } of refs) {
      try {
        await execFile(
          "git",
          ["update-ref", "-d", ref],
          { cwd: task.repoPath },
        );
      } catch {
        // Ref may already be deleted — ignore.
      }
    }
  }

  /**
   * Lists all refs matching a given prefix using `git for-each-ref`.
   */
  private async getRefs(
    task: Task,
    prefix: string,
  ): Promise<Array<{ ref: string; sha: string }>> {
    const { stdout } = await execFile(
      "git",
      ["for-each-ref", "--format=%(refname) %(objectname)", prefix],
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
