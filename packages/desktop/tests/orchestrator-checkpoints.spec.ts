/**
 * Integration tests for the CheckpointManager.
 *
 * Creates a temporary git repo, exercises the checkpoint lifecycle
 * (create → make changes → revert → verify state), and validates
 * listing and cleanup behaviour.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import type { Task } from "../electron/orchestrator";

const execFile = promisify(execFileCb);

// ─── Helpers ─────────────────────────────────────────────────────────────────

/** Creates a temporary git repo with an initial commit. */
async function createTempRepo(): Promise<string> {
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-cp-test-"));
  await execFile("git", ["init", "--initial-branch=main"], { cwd: repoDir });
  await execFile("git", ["config", "user.email", "test@crab.dev"], { cwd: repoDir });
  await execFile("git", ["config", "user.name", "Test"], { cwd: repoDir });
  await execFile("git", ["config", "core.autocrlf", "false"], { cwd: repoDir });

  // Create an initial commit so we have a valid HEAD.
  const readmePath = path.join(repoDir, "README.md");
  fs.writeFileSync(readmePath, "# Test Repo\n");
  await execFile("git", ["add", "."], { cwd: repoDir });
  await execFile("git", ["commit", "-m", "Initial commit"], { cwd: repoDir });

  return repoDir;
}

/** Cleans up a temporary repo directory. */
function cleanupRepo(repoDir: string): void {
  try {
    fs.rmSync(repoDir, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup.
  }
}

/** Creates a mock Task object pointing at the given repo. */
function createMockTask(repoDir: string): Task {
  return {
    id: `test-task-${Date.now()}`,
    prompt: "test prompt",
    agentType: "claude-code",
    status: "running",
    branch: "main",
    worktreePath: repoDir,
    baseRef: "main",
    repoPath: repoDir,
    createdAt: Date.now(),
    events: [
      { type: "text", timestamp: 0, content: "turn 0" },
      { type: "text", timestamp: 1, content: "turn 1" },
      { type: "text", timestamp: 2, content: "turn 2" },
      { type: "text", timestamp: 3, content: "turn 3" },
    ],
    feedback: [],
    todos: [],
  } as Task;
}

/** Writes a file, stages, and commits it. */
async function commitFile(
  repoDir: string,
  filename: string,
  content: string,
  message: string,
): Promise<string> {
  fs.writeFileSync(path.join(repoDir, filename), content);
  await execFile("git", ["add", filename], { cwd: repoDir });
  await execFile("git", ["commit", "-m", message], { cwd: repoDir });
  const { stdout } = await execFile("git", ["rev-parse", "HEAD"], { cwd: repoDir });
  return stdout.trim();
}

/** Checks if a git ref exists. */
async function refExists(repoDir: string, ref: string): Promise<boolean> {
  try {
    await execFile("git", ["rev-parse", "--verify", `${ref}^{object}`], { cwd: repoDir });
    return true;
  } catch {
    return false;
  }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("orchestrator-checkpoints", () => {
  let repoDir: string;
  let checkpointManager: CheckpointManager;

  beforeEach(async () => {
    repoDir = await createTempRepo();
    checkpointManager = new CheckpointManager();
  });

  afterEach(() => {
    cleanupRepo(repoDir);
  });

  describe("single-file revert", () => {
    it.each(["modified", "deleted", "untracked addition", "staged addition", "rename", "literal path"])("restores a %s without changing HEAD or unrelated staging", async kind => {
      const task = createMockTask(repoDir);
      const original = kind === "literal path" ? `file[1]${process.platform === "win32" ? " " : "\n"}雪.txt` : "source.txt";
      const unrelated = kind === "literal path" ? original.replace("[1]", "1") : "file-other.txt";
      const selected = kind === "rename" ? "renamed.txt" : original;
      const addition = kind.endsWith("addition");
      if (!addition) await commitFile(repoDir, original, "before\n", "Baseline");
      task.baseRef = (await execFile("git", ["rev-parse", "HEAD"], { cwd: repoDir })).stdout.trim();
      if (kind === "rename") fs.renameSync(path.join(repoDir, original), path.join(repoDir, selected));
      if (kind === "deleted") fs.unlinkSync(path.join(repoDir, selected));
      else fs.writeFileSync(path.join(repoDir, selected), "after\n");
      if (kind === "staged addition") await execFile("git", ["add", "--", selected], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "README.md"), "staged bytes\n");
      await execFile("git", ["add", "README.md"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "README.md"), "unstaged bytes\n");
      fs.writeFileSync(path.join(repoDir, unrelated), "untracked bytes\n");
      const index = (await execFile("git", ["ls-files", "--stage", "--", "README.md"], { cwd: repoDir })).stdout;
      await checkpointManager.revertFile(task, selected, kind === "rename" ? original : undefined);
      const backups = await checkpointManager.listRestoreBackups(task);
      expect({
        file: fs.existsSync(path.join(repoDir, original)) ? fs.readFileSync(path.join(repoDir, original), "utf8") : null,
        renamed: kind === "rename" && fs.existsSync(path.join(repoDir, selected)),
        head: (await execFile("git", ["rev-parse", "HEAD"], { cwd: repoDir })).stdout.trim(),
        index: (await execFile("git", ["ls-files", "--stage", "--", "README.md"], { cwd: repoDir })).stdout,
        unrelated: fs.readFileSync(path.join(repoDir, "README.md"), "utf8"),
        untracked: fs.readFileSync(path.join(repoDir, unrelated), "utf8"),
        backup: kind === "deleted" ? (await execFile("git", ["ls-tree", backups[0].ref, "--", selected], { cwd: repoDir })).stdout
          : (await execFile("git", ["show", `${backups[0].ref}:${selected}`], { cwd: repoDir })).stdout,
      }).toEqual({ file: addition ? null : "before\n", renamed: false, head: task.baseRef, index, unrelated: "unstaged bytes\n", untracked: "untracked bytes\n", backup: kind === "deleted" ? "" : "after\n" });
    });

    it("leaves bytes and index intact when its backup cannot be retained", async () => {
      const task = createMockTask(repoDir);
      fs.writeFileSync(path.join(repoDir, "README.md"), "user bytes");
      fs.writeFileSync(path.join(repoDir, ".git/refs/crab"), "blocked ref directory");
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      await expect(checkpointManager.revertFile(task, "README.md")).rejects.toThrow();
      expect({ bytes: fs.readFileSync(path.join(repoDir, "README.md"), "utf8"), index: fs.readFileSync(path.join(repoDir, ".git/index")) }).toEqual({ bytes: "user bytes", index });
    });

    it.each(["../outside.txt", ".git/config", ".", "replacement"])("rejects unsafe file target %s before changing bytes", async target => {
      const task = createMockTask(repoDir);
      fs.mkdirSync(path.join(repoDir, "replacement"));
      fs.writeFileSync(path.join(repoDir, "replacement/keep.txt"), "user bytes");
      await expect(checkpointManager.revertFile(task, target)).rejects.toThrow();
      expect(fs.readFileSync(path.join(repoDir, "replacement/keep.txt"), "utf8")).toBe("user bytes");
    });

    it("rejects a symbolic-link parent instead of writing outside the workspace", async () => {
      const task = createMockTask(repoDir);
      const outside = fs.mkdtempSync(path.join(os.tmpdir(), "crab-revert-outside-"));
      try {
        fs.writeFileSync(path.join(outside, "keep.txt"), "user bytes");
        fs.symlinkSync(outside, path.join(repoDir, "linked"), process.platform === "win32" ? "junction" : "dir");
        await expect(checkpointManager.revertFile(task, "linked/keep.txt")).rejects.toThrow("replaced file");
        expect(fs.readFileSync(path.join(outside, "keep.txt"), "utf8")).toBe("user bytes");
      } finally { cleanupRepo(outside); }
    });

    it("rejects an ignored replacement that its backup cannot capture", async () => {
      const task = createMockTask(repoDir);
      fs.writeFileSync(path.join(repoDir, ".gitignore"), "README.md\n");
      await execFile("git", ["rm", "--cached", "README.md"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "README.md"), "ignored user bytes");
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      await expect(checkpointManager.revertFile(task, "README.md")).rejects.toThrow("Move ignored path");
      expect({ bytes: fs.readFileSync(path.join(repoDir, "README.md"), "utf8"), index: fs.readFileSync(path.join(repoDir, ".git/index")) }).toEqual({ bytes: "ignored user bytes", index });
    });
  });

  describe("createCheckpoint", () => {
    it("captures untracked files even when user status configuration hides them", async () => {
      const task = createMockTask(repoDir);
      await execFile("git", ["config", "status.showUntrackedFiles", "no"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "untracked.txt"), "user bytes");
      const { sha } = await checkpointManager.createLightweightCheckpoint(task, 0);
      const { stdout } = await execFile("git", ["show", `${sha}:untracked.txt`], { cwd: repoDir });
      expect(stdout).toBe("user bytes");
    });

    it("pins a primary baseline against pruning until task cleanup", async () => {
      const task = createMockTask(repoDir);
      fs.writeFileSync(path.join(repoDir, "README.md"), "user baseline");
      const sha = await checkpointManager.captureBase(task);
      task.baseRef = sha;
      fs.writeFileSync(path.join(repoDir, "README.md"), "later changes");
      await execFile("git", ["prune", "--expire=now"], { cwd: repoDir });
      const { stdout } = await execFile("git", ["show", `${sha}:README.md`], { cwd: repoDir });
      await checkpointManager.cleanup(task);
      await execFile("git", ["prune", "--expire=now"], { cwd: repoDir });
      expect({ baseline: stdout, retained: await refExists(repoDir, sha) }).toEqual({ baseline: "user baseline", retained: false });
    });

    it("captures an unborn repository as a root snapshot without moving HEAD or the real index", async () => {
      const empty = fs.mkdtempSync(path.join(os.tmpdir(), "crab-unborn-checkpoint-"));
      try {
        await execFile("git", ["init", "--initial-branch=main", empty]);
        fs.writeFileSync(path.join(empty, "first.txt"), "first bytes");
        const sha = await checkpointManager.snapshotHead(createMockTask(empty));
        const { stdout: content } = await execFile("git", ["show", `${sha}:first.txt`], { cwd: empty });
        const { stdout: parents } = await execFile("git", ["rev-list", "--parents", "-n", "1", sha], { cwd: empty });
        expect({ content, root: parents.trim() === sha, head: await refExists(empty, "HEAD"), index: fs.existsSync(path.join(empty, ".git/index")) }).toEqual({ content: "first bytes", root: true, head: false, index: false });
      } finally { cleanupRepo(empty); }
    });

    it("fails on a required clean-filter error instead of publishing an incomplete working tree", async () => {
      const task = createMockTask(repoDir);
      fs.writeFileSync(path.join(repoDir, "payload.bin"), "before");
      await execFile("git", ["add", "payload.bin"], { cwd: repoDir });
      await execFile("git", ["commit", "-m", "Saved payload"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, ".gitattributes"), "*.bin filter=checkpoint-fixture\n");
      await execFile("git", ["config", "filter.checkpoint-fixture.clean", "exit 17"], { cwd: repoDir });
      await execFile("git", ["config", "filter.checkpoint-fixture.required", "true"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "payload.bin"), "after");
      await expect(checkpointManager.createCheckpoint(task, 0)).rejects.toThrow();
      expect({ refs: await checkpointManager.listCheckpoints(task), file: fs.readFileSync(path.join(repoDir, "payload.bin"), "utf8") }).toEqual({ refs: [], file: "after" });
    });

    it("distinguishes an absent checkpoint from an exact saved baseline", async () => {
      const task = createMockTask(repoDir);
      await checkpointManager.createCheckpoint(task, 1);
      expect(await Promise.all([0, 1, 10].map(turn => checkpointManager.hasCheckpoint(task, turn)))).toEqual([false, true, false]);
    });

    it("rejects a checkpoint ref that no longer points to a commit", async () => {
      const task = createMockTask(repoDir);
      await execFile("git", ["update-ref", `refs/crab/checkpoints/${task.id}/0`, "HEAD:README.md"], { cwd: repoDir });
      await expect(checkpointManager.hasCheckpoint(task, 0)).rejects.toThrow();
    });

    it("does not treat a failed checkpoint lookup as an absent baseline", async () => {
      const task = { ...createMockTask(repoDir), repoPath: path.join(repoDir, "missing") };
      await expect(checkpointManager.hasCheckpoint(task, 0)).rejects.toThrow();
    });

    it("publishes the captured pre-input state even after native work changes the workspace", async () => {
      const task = createMockTask(repoDir);
      fs.writeFileSync(path.join(repoDir, "README.md"), "Before input\n");
      const sha = await checkpointManager.snapshotHead(task);
      await checkpointManager.holdCheckpoint(task, 1, sha);
      await execFile("git", ["prune", "--expire=now"], { cwd: repoDir });
      const beforePublication = await checkpointManager.listCheckpoints(task);
      fs.writeFileSync(path.join(repoDir, "README.md"), "After native work\n");
      const { ref } = await checkpointManager.recordCheckpoint(task, 1, sha);
      await checkpointManager.releaseCheckpoint(task, 1);
      const { stdout } = await execFile("git", ["show", `${ref}:README.md`], { cwd: repoDir });
      expect({ beforePublication, checkpoint: stdout, workspace: fs.readFileSync(path.join(repoDir, "README.md"), "utf8") }).toEqual({
        beforePublication: [], checkpoint: "Before input\n", workspace: "After native work\n",
      });
    });

    it("creates a ref pointing at a snapshot commit", async () => {
      const task = createMockTask(repoDir);

      const { ref: refName, sha } = await checkpointManager.createCheckpoint(task, 0);

      expect(refName).toBe(`refs/crab/checkpoints/${task.id}/0`);
      expect(sha).toMatch(/^[0-9a-f]{40}$/);
      expect(await refExists(repoDir, refName)).toBe(true);

      // With no uncommitted changes, the snapshot's tree equals HEAD's tree.
      const { stdout: headTree } = await execFile(
        "git",
        ["rev-parse", "HEAD^{tree}"],
        { cwd: repoDir },
      );
      const { stdout: refTree } = await execFile(
        "git",
        ["rev-parse", `${refName}^{tree}`],
        { cwd: repoDir },
      );
      expect(refTree.trim()).toBe(headTree.trim());
    });

    it("captures uncommitted changes in the snapshot", async () => {
      const task = createMockTask(repoDir);

      // Turn 0: baseline checkpoint, no uncommitted changes yet.
      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Simulate agent work WITHOUT committing — this is the common
      // real-world case that the old implementation failed to capture.
      fs.writeFileSync(path.join(repoDir, "dirty.ts"), "export const v = 1;\n");

      // Turn 1: checkpoint picks up the uncommitted file.
      const cp1 = await checkpointManager.createCheckpoint(task, 1);

      // Sanity: both snapshots have distinct tree SHAs because the
      // working tree differs (cp1 includes dirty.ts, cp0 doesn't).
      const { stdout: t0 } = await execFile("git", ["rev-parse", `${cp0.sha}^{tree}`], { cwd: repoDir });
      const { stdout: t1 } = await execFile("git", ["rev-parse", `${cp1.sha}^{tree}`], { cwd: repoDir });
      expect(t0.trim()).not.toBe(t1.trim());

      // `git diff cp0 cp1` must show dirty.ts as added — this is the
      // exact query the UI runs when the user clicks a turn dot.
      const { stdout: diffOut } = await execFile(
        "git",
        ["diff", "--name-status", cp0.sha, cp1.sha],
        { cwd: repoDir },
      );
      expect(diffOut).toContain("dirty.ts");
    });
  });

  describe("saved file restoration", () => {
    it.each(["snapshot", "file"])("cannot restore a %s through a replaced directory into its parent repository", async mode => {
      const task = createMockTask(repoDir);
      const checkpoint = await checkpointManager.createCheckpoint(task, 0);
      const replacement = path.join(repoDir, "replaced-workspace");
      fs.mkdirSync(replacement);
      fs.writeFileSync(path.join(repoDir, "README.md"), "primary user bytes");
      let rejected = false;
      try {
        const replaced = { ...task, worktreePath: replacement };
        if (mode === "snapshot") await checkpointManager.restoreFiles(replaced, checkpoint.ref);
        else await checkpointManager.revertFile(replaced, "README.md");
      } catch { rejected = true; }
      expect({ rejected, primary: fs.readFileSync(path.join(repoDir, "README.md"), "utf8") }).toEqual({ rejected: true, primary: "primary user bytes" });
    });

    it.each(["snapshot", "file"])("rejects a %s replacement Git pointer that shares the primary index", async mode => {
      const task = createMockTask(repoDir);
      const checkpoint = await checkpointManager.createCheckpoint(task, 0);
      const replacement = path.join(repoDir, "replaced-workspace");
      fs.mkdirSync(replacement);
      fs.writeFileSync(path.join(replacement, ".git"), `gitdir: ${path.join(repoDir, ".git")}\n`);
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      const replaced = { ...task, worktreePath: replacement };
      await expect(mode === "snapshot" ? checkpointManager.restoreFiles(replaced, checkpoint.ref) : checkpointManager.revertFile(replaced, "README.md")).rejects.toThrow("primary Git index");
      expect(fs.readFileSync(path.join(repoDir, ".git/index"))).toEqual(index);
    });

    it.each(["snapshot", "file"])("rejects a %s in an unrelated repository at the saved workspace path", async mode => {
      const task = createMockTask(repoDir);
      const checkpoint = await checkpointManager.createCheckpoint(task, 0);
      const replacement = path.join(repoDir, "unrelated-repository");
      await execFile("git", ["clone", "--no-hardlinks", repoDir, replacement]);
      const replaced = { ...task, worktreePath: replacement };
      await expect(mode === "snapshot" ? checkpointManager.restoreFiles(replaced, checkpoint.ref) : checkpointManager.revertFile(replaced, "README.md")).rejects.toThrow("different Git repository");
    });

    it("retains and restores every pre-restore file backup through Git pruning", async () => {
      const task = createMockTask(repoDir);
      const checkpoint = await checkpointManager.createCheckpoint(task, 0);
      fs.writeFileSync(path.join(repoDir, "untracked.txt"), "uncommitted output");
      await checkpointManager.restoreFiles(task, checkpoint.ref);
      const [backup] = await checkpointManager.listRestoreBackups(task);
      await execFile("git", ["prune", "--expire=now"], { cwd: repoDir });
      await checkpointManager.restoreFiles(task, backup.ref);
      expect({ contents: fs.readFileSync(path.join(repoDir, "untracked.txt"), "utf8"), backups: (await checkpointManager.listRestoreBackups(task)).length })
        .toEqual({ contents: "uncommitted output", backups: 2 });
    });

    it("does not change the index or files if the backup ref cannot be saved", async () => {
      const task = createMockTask(repoDir);
      const checkpoint = await checkpointManager.createCheckpoint(task, 0);
      fs.writeFileSync(path.join(repoDir, "untracked.txt"), "user bytes");
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      fs.mkdirSync(path.join(repoDir, ".git/refs/crab"), { recursive: true });
      fs.writeFileSync(path.join(repoDir, ".git/refs/crab/restore-backups"), "blocked ref directory");
      await expect(checkpointManager.restoreFiles(task, checkpoint.ref)).rejects.toThrow();
      expect({ index: fs.readFileSync(path.join(repoDir, ".git/index")), contents: fs.readFileSync(path.join(repoDir, "untracked.txt"), "utf8") })
        .toEqual({ index, contents: "user bytes" });
    });

    it("rejects another conversation's snapshot before changing files", async () => {
      const task = createMockTask(repoDir);
      const checkpoint = await checkpointManager.createCheckpoint(task, 0);
      const other = { ...task, id: `${task.id}-other` };
      await expect(checkpointManager.restoreFiles(other, checkpoint.ref)).rejects.toThrow("does not belong");
      expect(await checkpointManager.listRestoreBackups(other)).toEqual([]);
    });

    it("restores files without moving HEAD or rewriting conversation history", async () => {
      const task = createMockTask(repoDir);
      const first = await checkpointManager.createCheckpoint(task, 0);
      await commitFile(repoDir, "later.txt", "committed output", "Output");
      const second = await checkpointManager.createCheckpoint(task, 1);
      fs.writeFileSync(path.join(repoDir, "untracked.txt"), "uncommitted output");
      const { stdout: head } = await execFile("git", ["rev-parse", "HEAD"], { cwd: repoDir });
      const events = [...task.events];
      await checkpointManager.restoreFiles(task, `refs/crab/checkpoints/${task.id}/0`);
      expect({ head: (await execFile("git", ["rev-parse", "HEAD"], { cwd: repoDir })).stdout, events: task.events,
        refs: await Promise.all([first, second].map(cp => refExists(repoDir, cp.ref))),
        untracked: fs.existsSync(path.join(repoDir, "untracked.txt")) }).toEqual({ head, events, refs: [true, true], untracked: false });
    });

    it("rejects restoring over ignored user bytes", async () => {
      const task = createMockTask(repoDir);
      await commitFile(repoDir, "private.txt", "baseline", "Baseline");
      await checkpointManager.createCheckpoint(task, 0);
      await execFile("git", ["rm", "private.txt"], { cwd: repoDir });
      await commitFile(repoDir, ".gitignore", "private.txt\n", "Ignored file");
      fs.writeFileSync(path.join(repoDir, "private.txt"), "ignored user bytes");
      await expect(checkpointManager.restoreFiles(task, `refs/crab/checkpoints/${task.id}/0`)).rejects.toThrow();
      expect(fs.readFileSync(path.join(repoDir, "private.txt"), "utf8")).toBe("ignored user bytes");
    });

    it("includes staged files that an ignore rule now matches", async () => {
      const task = createMockTask(repoDir);
      fs.writeFileSync(path.join(repoDir, "staged.txt"), "staged bytes");
      await execFile("git", ["add", "staged.txt"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, ".gitignore"), "staged.txt\n");
      const cp = await checkpointManager.createCheckpoint(task, 0);
      expect((await execFile("git", ["show", `${cp.sha}:staged.txt`], { cwd: repoDir })).stdout).toBe("staged bytes");
    });

    it("reverts worktree to checkpoint state, removing later changes", async () => {
      const task = createMockTask(repoDir);

      // Turn 0: checkpoint at initial commit.
      await checkpointManager.createCheckpoint(task, 0);

      // Make changes and commit (simulating agent turn 1).
      await commitFile(repoDir, "feature.ts", "export const x = 1;\n", "Add feature");

      // Turn 1: checkpoint after first agent turn.
      await checkpointManager.createCheckpoint(task, 1);

      // Verify feature.ts exists at this point.
      expect(fs.existsSync(path.join(repoDir, "feature.ts"))).toBe(true);
      expect(fs.readFileSync(path.join(repoDir, "feature.ts"), "utf-8")).toBe("export const x = 1;\n");

      // Make more changes and commit (simulating agent turn 2).
      await commitFile(repoDir, "feature.ts", "export const x = 42;\n", "Modify feature");
      await commitFile(repoDir, "extra.ts", "export const y = 2;\n", "Add extra file");

      // Turn 2: checkpoint after second agent turn.
      await checkpointManager.createCheckpoint(task, 2);

      // Verify the later changes are present.
      expect(fs.readFileSync(path.join(repoDir, "feature.ts"), "utf-8")).toBe("export const x = 42;\n");
      expect(fs.existsSync(path.join(repoDir, "extra.ts"))).toBe(true);

      // Revert to turn 1.
      await checkpointManager.restoreFiles(task, `refs/crab/checkpoints/${task.id}/1`);

      // Verify worktree matches turn 1 state:
      // - feature.ts should have the turn 1 content.
      expect(fs.readFileSync(path.join(repoDir, "feature.ts"), "utf-8")).toBe("export const x = 1;\n");
      // - extra.ts should not exist (it was added in turn 2).
      expect(fs.existsSync(path.join(repoDir, "extra.ts"))).toBe(false);
    });




  });

  describe("listCheckpoints", () => {
    it("returns checkpoints sorted by turn number", async () => {
      const task = createMockTask(repoDir);

      await checkpointManager.createCheckpoint(task, 0);
      await commitFile(repoDir, "a.ts", "a\n", "commit a");
      await checkpointManager.createCheckpoint(task, 1);
      await commitFile(repoDir, "b.ts", "b\n", "commit b");
      await checkpointManager.createCheckpoint(task, 2);

      const checkpoints = await checkpointManager.listCheckpoints(task);

      expect(checkpoints).toHaveLength(3);
      expect(checkpoints[0].turnNumber).toBe(0);
      expect(checkpoints[1].turnNumber).toBe(1);
      expect(checkpoints[2].turnNumber).toBe(2);

      // Each checkpoint should have a valid SHA.
      for (const cp of checkpoints) {
        expect(cp.sha).toMatch(/^[0-9a-f]{40}$/);
        expect(cp.ref).toBe(`refs/crab/checkpoints/${task.id}/${cp.turnNumber}`);
      }
    });



    it("returns empty array when no checkpoints exist", async () => {
      const task = createMockTask(repoDir);
      const checkpoints = await checkpointManager.listCheckpoints(task);
      expect(checkpoints).toEqual([]);
    });
  });

  describe("cleanup", () => {
    it.each(["base", "checkpoint", "hold", "release", "cleanup"] as const)("keeps user branches intact when a private %s ref is symbolic", async operation => {
      const task = createMockTask(repoDir);
      const { stdout } = await execFile("git", ["rev-parse", "HEAD"], { cwd: repoDir });
      const original = stdout.trim();
      fs.writeFileSync(path.join(repoDir, "protected.txt"), "protected branch state");
      const protectedSha = await checkpointManager.snapshotHead(task);
      await execFile("git", ["branch", "protected", protectedSha], { cwd: repoDir });
      const ref = operation === "base" ? `refs/crab/bases/${original}`
        : operation === "hold" || operation === "release" ? `refs/crab/checkpoint-intents/${task.id}/1` : `refs/crab/checkpoints/${task.id}/1`;
      await execFile("git", ["symbolic-ref", ref, "refs/heads/protected"], { cwd: repoDir });
      if (operation === "base") await checkpointManager.pinBase(task);
      if (operation === "checkpoint") await checkpointManager.recordCheckpoint(task, 1, original);
      if (operation === "hold") await checkpointManager.holdCheckpoint(task, 1, original);
      if (operation === "release") await checkpointManager.releaseCheckpoint(task, 1);
      if (operation === "cleanup") await checkpointManager.cleanup(task, true);
      const { stdout: after } = await execFile("git", ["rev-parse", "--verify", "refs/heads/protected"], { cwd: repoDir });
      expect(after.trim()).toBe(protectedSha);
    });

    it("removes all checkpoint refs for a task", async () => {
      const task = createMockTask(repoDir);

      await checkpointManager.createCheckpoint(task, 0);
      await commitFile(repoDir, "a.ts", "a\n", "commit a");
      await checkpointManager.createCheckpoint(task, 1);
      await commitFile(repoDir, "b.ts", "b\n", "commit b");
      await checkpointManager.createCheckpoint(task, 2);

      // Verify refs exist.
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/0`)).toBe(true);
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/1`)).toBe(true);
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/2`)).toBe(true);

      await checkpointManager.cleanup(task);

      // All refs should be gone.
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/0`)).toBe(false);
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/1`)).toBe(false);
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/2`)).toBe(false);
    });

    it("does not throw when no checkpoints exist", async () => {
      const task = createMockTask(repoDir);
      await expect(checkpointManager.cleanup(task)).resolves.toBeUndefined();
    });
  });
});
