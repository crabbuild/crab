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
      { type: "text", text: "turn 0" },
      { type: "text", text: "turn 1" },
      { type: "text", text: "turn 2" },
      { type: "text", text: "turn 3" },
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
    await execFile("git", ["rev-parse", "--verify", ref], { cwd: repoDir });
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

  describe("createCheckpoint", () => {
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

  describe("full checkpoint flow: create → change → revert", () => {
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
      await checkpointManager.revertToCheckpoint(task, 1);

      // Verify worktree matches turn 1 state:
      // - feature.ts should have the turn 1 content.
      expect(fs.readFileSync(path.join(repoDir, "feature.ts"), "utf-8")).toBe("export const x = 1;\n");
      // - extra.ts should not exist (it was added in turn 2).
      expect(fs.existsSync(path.join(repoDir, "extra.ts"))).toBe(false);
    });

    it("deletes checkpoint refs after the reverted turn", async () => {
      const task = createMockTask(repoDir);

      await checkpointManager.createCheckpoint(task, 0);
      await commitFile(repoDir, "a.ts", "a\n", "commit a");
      await checkpointManager.createCheckpoint(task, 1);
      await commitFile(repoDir, "b.ts", "b\n", "commit b");
      await checkpointManager.createCheckpoint(task, 2);
      await commitFile(repoDir, "c.ts", "c\n", "commit c");
      await checkpointManager.createCheckpoint(task, 3);

      // Revert to turn 1.
      await checkpointManager.revertToCheckpoint(task, 1);

      // Turns 0 and 1 should still exist.
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/0`)).toBe(true);
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/1`)).toBe(true);

      // Turns 2 and 3 should be deleted.
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/2`)).toBe(false);
      expect(await refExists(repoDir, `refs/crab/checkpoints/${task.id}/3`)).toBe(false);
    });

    it("trims event history to the reverted turn", async () => {
      const task = createMockTask(repoDir);

      await checkpointManager.createCheckpoint(task, 0);
      await commitFile(repoDir, "a.ts", "a\n", "commit a");
      await checkpointManager.createCheckpoint(task, 1);
      await commitFile(repoDir, "b.ts", "b\n", "commit b");
      await checkpointManager.createCheckpoint(task, 2);

      expect(task.events).toHaveLength(4);

      // Revert to turn 1 — should keep only events at indices 0.
      await checkpointManager.revertToCheckpoint(task, 1);

      expect(task.events).toHaveLength(1);
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

    it("returns correct list after revert removes later checkpoints", async () => {
      const task = createMockTask(repoDir);

      await checkpointManager.createCheckpoint(task, 0);
      await commitFile(repoDir, "a.ts", "a\n", "commit a");
      await checkpointManager.createCheckpoint(task, 1);
      await commitFile(repoDir, "b.ts", "b\n", "commit b");
      await checkpointManager.createCheckpoint(task, 2);

      await checkpointManager.revertToCheckpoint(task, 1);

      const checkpoints = await checkpointManager.listCheckpoints(task);
      expect(checkpoints).toHaveLength(2);
      expect(checkpoints[0].turnNumber).toBe(0);
      expect(checkpoints[1].turnNumber).toBe(1);
    });

    it("returns empty array when no checkpoints exist", async () => {
      const task = createMockTask(repoDir);
      const checkpoints = await checkpointManager.listCheckpoints(task);
      expect(checkpoints).toEqual([]);
    });
  });

  describe("cleanup", () => {
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
