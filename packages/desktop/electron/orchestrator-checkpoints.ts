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

import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import type { Task } from "./orchestrator";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface Checkpoint {
  ref: string;
  turnNumber: number;
  sha: string;
}

// ─── CheckpointManager ──────────────────────────────────────────────────────

export class CheckpointManager {
  private readonly refPrefix = "refs/crab/checkpoints";

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
    const refName = `${this.refPrefix}/${task.id}/${turnNumber}`;
    const { sha: snapshotSha, headSha } = await this.snapshotWorkingTree(
      task,
      `crab-checkpoint: turn ${turnNumber}`,
    );

    // Fall back to HEAD if the snapshot commit couldn't be produced
    // (e.g. no git identity configured and we somehow skipped the env
    // overrides below). This preserves prior behaviour rather than
    // losing the checkpoint entirely.
    const refSha = snapshotSha || headSha;
    if (!refSha) {
      throw new Error("Cannot create checkpoint: no HEAD and no snapshot commit produced");
    }

    await execFile(
      "git",
      ["update-ref", refName, refSha],
      { cwd: task.repoPath },
    );

    return { ref: refName, sha: refSha };
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
    const refName = `${this.refPrefix}/${task.id}/${turnNumber}`;
    const worktree = task.worktreePath;

    // Check if the worktree is clean (no staged, unstaged, or untracked changes).
    let isClean = false;
    try {
      // `git status --porcelain` outputs nothing when the tree is clean.
      const { stdout } = await execFile(
        "git",
        ["status", "--porcelain"],
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

    await execFile(
      "git",
      ["update-ref", refName, headSha],
      { cwd: task.repoPath },
    );

    return { ref: refName, sha: headSha };
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
    const { sha, headSha } = await this.snapshotWorkingTree(
      task,
      "crab-checkpoint: live snapshot",
    );
    return sha || headSha;
  }

  private async snapshotWorkingTree(
    task: Task,
    message: string,
  ): Promise<{ sha: string; headSha: string }> {
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
    } catch {
      headSha = "";
    }

    // Build a temp index that mirrors the current working tree.
    const tmpIndex = path.join(
      os.tmpdir(),
      `crab-ckpt-index-${task.id}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    );

    let snapshotSha = "";
    try {
      // Seed the temp index from HEAD when available, otherwise start empty.
      if (headSha) {
        await execFile(
          "git",
          ["read-tree", headSha],
          { cwd: worktree, env: { ...process.env, GIT_INDEX_FILE: tmpIndex } },
        );
      }

      // Stage everything currently in the working tree (tracked + untracked).
      // `-A` picks up deletes too. Honours `.gitignore` by default, matching
      // how `git add .` behaves, so generated files we don't care about
      // (build outputs, node_modules) stay out of the snapshot.
      try {
        await execFile(
          "git",
          ["add", "-A", "--", "."],
          { cwd: worktree, env: { ...process.env, GIT_INDEX_FILE: tmpIndex } },
        );
      } catch {
        // If `git add -A` fails with the temp index, try an alternative
        // approach: use `git ls-files` + `git update-index` to build the
        // index manually. This handles edge cases where the temp index
        // confuses git's working tree detection.
        try {
          // Remove the potentially corrupted temp index and start fresh.
          try { fs.unlinkSync(tmpIndex); } catch { /* ignore */ }
          if (headSha) {
            await execFile(
              "git",
              ["read-tree", headSha],
              { cwd: worktree, env: { ...process.env, GIT_INDEX_FILE: tmpIndex } },
            );
          }
          // Use --work-tree to explicitly tell git where the working tree is.
          await execFile(
            "git",
            ["--work-tree", worktree, "add", "-A", "--", "."],
            { cwd: worktree, env: { ...process.env, GIT_INDEX_FILE: tmpIndex } },
          );
        } catch {
          // Last resort: just use HEAD's tree as-is.
        }
      }

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
      snapshotSha = commitOut.trim();
    } finally {
      // Temp index lives outside the repo — remove it regardless of outcome.
      try { fs.unlinkSync(tmpIndex); } catch { /* ignore */ }
    }

    return { sha: snapshotSha, headSha };
  }

  /**
   * Reverts the workspace to the checkpoint at the given turn number.
   * Resets the worktree to the checkpoint SHA, deletes all later
   * checkpoint refs, and trims the task's event history.
   */
  async revertToCheckpoint(task: Task, turnNumber: number): Promise<void> {
    const refName = `${this.refPrefix}/${task.id}/${turnNumber}`;

    const { stdout: sha } = await execFile(
      "git",
      ["rev-parse", refName],
      { cwd: task.repoPath },
    );

    // Reset worktree to the checkpoint commit.
    await execFile(
      "git",
      ["reset", "--hard", sha.trim()],
      { cwd: task.worktreePath },
    );

    // Delete checkpoint refs that come after this turn.
    const laterRefs = await this.getCheckpointsAfter(task, turnNumber);
    for (const ref of laterRefs) {
      await execFile(
        "git",
        ["update-ref", "-d", ref],
        { cwd: task.repoPath },
      );
    }

    // Trim event history to only include events up to the reverted turn.
    task.events = task.events.filter(
      (_e, idx) => idx < turnNumber,
    );
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
  async cleanup(task: Task): Promise<void> {
    const prefix = `${this.refPrefix}/${task.id}/`;
    const refs = await this.getRefs(task, prefix);

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

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Returns all checkpoint refs after the given turn number for a task.
   */
  private async getCheckpointsAfter(task: Task, turnNumber: number): Promise<string[]> {
    const prefix = `${this.refPrefix}/${task.id}/`;
    const refs = await this.getRefs(task, prefix);

    return refs
      .filter((entry) => {
        const turn = parseInt(entry.ref.slice(prefix.length), 10);
        return turn > turnNumber;
      })
      .map((entry) => entry.ref);
  }

  /**
   * Lists all refs matching a given prefix using `git for-each-ref`.
   */
  private async getRefs(
    task: Task,
    prefix: string,
  ): Promise<Array<{ ref: string; sha: string }>> {
    try {
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
    } catch {
      // No refs found or git error — return empty.
      return [];
    }
  }
}
