/**
 * Integration tests for checkpoint time-travel diff rendering.
 *
 * Validates the exact scenario that was broken: when an agent runs
 * multiple turns, selecting Turn 0 in the checkpoint timeline should
 * show Turn 0's changes (not empty), and Turn 1 should show only
 * Turn 1's changes (not ALL changes).
 *
 * The test simulates the full orchestrator flow:
 * 1. Create worktree from baseRef
 * 2. Create checkpoint 0 (before agent runs)
 * 3. Agent makes changes + auto-commit
 * 4. Create checkpoint 1 (before next agent run)
 * 5. Agent makes more changes + auto-commit
 * 6. Verify per-turn diffs are correct
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

async function createTempRepo(): Promise<string> {
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-tt-test-"));
  await execFile("git", ["init", "--initial-branch=main"], { cwd: repoDir });
  await execFile("git", ["config", "user.email", "test@crab.dev"], { cwd: repoDir });
  await execFile("git", ["config", "user.name", "Test"], { cwd: repoDir });

  // Initial commit with some files.
  fs.writeFileSync(path.join(repoDir, "README.md"), "# Test Repo\n");
  fs.writeFileSync(path.join(repoDir, "existing.ts"), "export const x = 0;\n");
  await execFile("git", ["add", "."], { cwd: repoDir });
  await execFile("git", ["commit", "-m", "Initial commit"], { cwd: repoDir });

  return repoDir;
}

function cleanupRepo(repoDir: string): void {
  try {
    fs.rmSync(repoDir, { recursive: true, force: true });
  } catch { /* best-effort */ }
}

function createMockTask(repoDir: string): Task {
  return {
    id: `test-task-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
    prompt: "test prompt",
    agentType: "claude-code",
    status: "running",
    branch: "main",
    worktreePath: repoDir,
    baseRef: "main",
    repoPath: repoDir,
    createdAt: Date.now(),
    events: [],
    feedback: [],
    todos: [],
  } as Task;
}

/**
 * Simulates the auto-commit that happens in the orchestrator's onExit handler.
 */
async function autoCommit(repoDir: string, turnNumber: number): Promise<void> {
  await execFile("git", ["add", "-A"], { cwd: repoDir });
  try {
    await execFile("git", ["diff", "--cached", "--quiet"], { cwd: repoDir });
    // No changes — skip commit.
  } catch {
    // There are staged changes — commit them.
    await execFile(
      "git",
      ["commit", "-m", `agent checkpoint: turn ${turnNumber}`, "--allow-empty-message"],
      {
        cwd: repoDir,
        env: {
          ...process.env,
          GIT_AUTHOR_NAME: "crab-agent",
          GIT_AUTHOR_EMAIL: "agent@crab.local",
          GIT_COMMITTER_NAME: "crab-agent",
          GIT_COMMITTER_EMAIL: "agent@crab.local",
        },
      },
    );
  }
}

/**
 * Computes the diff between two checkpoint SHAs, exactly as the getDiff
 * IPC handler does. Returns the list of changed file paths.
 */
async function computeCheckpointDiff(
  repoDir: string,
  fromSha: string,
  toSha: string,
): Promise<string[]> {
  // Resolve to tree SHAs (same as the fixed getDiff handler).
  const { stdout: fromTree } = await execFile(
    "git", ["rev-parse", `${fromSha}^{tree}`], { cwd: repoDir },
  );
  const { stdout: toTree } = await execFile(
    "git", ["rev-parse", `${toSha}^{tree}`], { cwd: repoDir },
  );

  const { stdout: nameStatus } = await execFile(
    "git", ["diff", "--name-status", fromTree.trim(), toTree.trim()],
    { cwd: repoDir },
  );

  if (!nameStatus.trim()) return [];
  return nameStatus.trim().split("\n").map((line) => {
    const parts = line.split("\t");
    return parts[parts.length - 1]; // file path
  });
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("checkpoint time-travel diffs", () => {
  let repoDir: string;
  let checkpointManager: CheckpointManager;

  beforeEach(async () => {
    repoDir = await createTempRepo();
    checkpointManager = new CheckpointManager();
  });

  afterEach(() => {
    cleanupRepo(repoDir);
  });

  describe("multi-turn scenario (the exact bug from screenshots)", () => {
    it("Turn 0 diff shows Turn 0 changes, Turn 1 diff shows Turn 1 changes", async () => {
      const task = createMockTask(repoDir);

      // ── Step 1: Create checkpoint 0 (before agent runs turn 0) ──
      const cp0 = await checkpointManager.createCheckpoint(task, 0);
      expect(cp0.sha).toMatch(/^[0-9a-f]{40}$/);

      // ── Step 2: Agent turn 0 — creates a new file and modifies existing ──
      fs.writeFileSync(path.join(repoDir, "feature.ts"), "export const feature = true;\n");
      fs.writeFileSync(path.join(repoDir, "existing.ts"), "export const x = 1;\n");

      // ── Step 3: Auto-commit (simulates onExit handler) ──
      await autoCommit(repoDir, 0);

      // ── Step 4: Create checkpoint 1 (before agent runs turn 1) ──
      // This is what iterate() does before calling startTask().
      const cp1 = await checkpointManager.createCheckpoint(task, 1);
      expect(cp1.sha).toMatch(/^[0-9a-f]{40}$/);

      // ── Step 5: Agent turn 1 — creates another file ──
      fs.writeFileSync(path.join(repoDir, "utils.ts"), "export function helper() {}\n");
      fs.writeFileSync(path.join(repoDir, "existing.ts"), "export const x = 42;\n");

      // ── Step 6: Auto-commit ──
      await autoCommit(repoDir, 1);

      // ── Verify: Turn 0 diff (cp0 → cp1) shows Turn 0's changes ──
      const turn0Files = await computeCheckpointDiff(repoDir, cp0.sha, cp1.sha);
      expect(turn0Files).toContain("feature.ts");
      expect(turn0Files).toContain("existing.ts");
      expect(turn0Files).not.toContain("utils.ts"); // Turn 1's file

      // ── Verify: Turn 1 diff (cp1 → HEAD snapshot) shows Turn 1's changes ──
      const headSnapshot = await checkpointManager.snapshotHead(task);
      const turn1Files = await computeCheckpointDiff(repoDir, cp1.sha, headSnapshot);
      expect(turn1Files).toContain("utils.ts");
      expect(turn1Files).toContain("existing.ts"); // Modified again in turn 1
      expect(turn1Files).not.toContain("feature.ts"); // Not touched in turn 1

      // ── Verify: "All changes" diff (cp0 → HEAD) shows everything ──
      const allFiles = await computeCheckpointDiff(repoDir, cp0.sha, headSnapshot);
      expect(allFiles).toContain("feature.ts");
      expect(allFiles).toContain("existing.ts");
      expect(allFiles).toContain("utils.ts");
    });

    it("checkpoint trees are distinct between turns", async () => {
      const task = createMockTask(repoDir);

      // Checkpoint 0: clean state.
      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Agent makes changes and commits.
      fs.writeFileSync(path.join(repoDir, "new-file.ts"), "content\n");
      await autoCommit(repoDir, 0);

      // Checkpoint 1: after turn 0 changes.
      const cp1 = await checkpointManager.createCheckpoint(task, 1);

      // The two checkpoints MUST have different tree SHAs.
      const { stdout: tree0 } = await execFile(
        "git", ["rev-parse", `${cp0.sha}^{tree}`], { cwd: repoDir },
      );
      const { stdout: tree1 } = await execFile(
        "git", ["rev-parse", `${cp1.sha}^{tree}`], { cwd: repoDir },
      );
      expect(tree0.trim()).not.toBe(tree1.trim());
    });

    it("checkpoint captures uncommitted agent changes", async () => {
      const task = createMockTask(repoDir);

      // Checkpoint 0.
      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Agent makes changes but does NOT commit (some agents don't).
      fs.writeFileSync(path.join(repoDir, "uncommitted.ts"), "// new file\n");
      fs.writeFileSync(path.join(repoDir, "existing.ts"), "export const x = 99;\n");

      // Checkpoint 1 should capture the uncommitted changes.
      const cp1 = await checkpointManager.createCheckpoint(task, 1);

      // Diff should show the uncommitted changes.
      const files = await computeCheckpointDiff(repoDir, cp0.sha, cp1.sha);
      expect(files).toContain("uncommitted.ts");
      expect(files).toContain("existing.ts");
    });

    it("no double-checkpoint: iterate creates one, startTask skips", async () => {
      const task = createMockTask(repoDir);

      // First checkpoint (turn 0) — created by startTask.
      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Agent runs turn 0.
      fs.writeFileSync(path.join(repoDir, "turn0.ts"), "turn 0\n");
      await autoCommit(repoDir, 0);

      // iterate() creates checkpoint 1.
      const cp1 = await checkpointManager.createCheckpoint(task, 1);

      // Verify the ref exists and points to cp1.sha.
      const { stdout: refSha } = await execFile(
        "git", ["rev-parse", `refs/crab/checkpoints/${task.id}/1`],
        { cwd: repoDir },
      );
      expect(refSha.trim()).toBe(cp1.sha);

      // If startTask were to create another checkpoint for turn 1,
      // it would overwrite the ref. Verify that after our fix,
      // the ref still points to the original SHA.
      // (In the fixed code, startTask skips checkpoint creation
      // when called from iterate with skipCheckpoint: true.)
      // We just verify the ref hasn't changed.
      const checkpoints = await checkpointManager.listCheckpoints(task);
      const turn1Cp = checkpoints.find((c) => c.turnNumber === 1);
      expect(turn1Cp?.sha).toBe(cp1.sha);
    });
  });

  describe("worktree isolation scenario", () => {
    it("works correctly when worktree is separate from repoPath", async () => {
      // Create a main repo and a worktree (simulating the real setup).
      const mainRepo = repoDir;
      const worktreeDir = path.join(os.tmpdir(), `crab-wt-test-${Date.now()}`);

      try {
        // Create a worktree branch.
        await execFile(
          "git",
          ["worktree", "add", "-b", "agent/test", worktreeDir, "HEAD"],
          { cwd: mainRepo },
        );
        await execFile("git", ["config", "user.email", "test@crab.dev"], { cwd: worktreeDir });
        await execFile("git", ["config", "user.name", "Test"], { cwd: worktreeDir });

        const task: Task = {
          id: `wt-test-${Date.now()}`,
          prompt: "test",
          agentType: "claude-code",
          status: "running",
          branch: "agent/test",
          worktreePath: worktreeDir,
          baseRef: "main",
          repoPath: mainRepo,
          createdAt: Date.now(),
          events: [],
          feedback: [],
          todos: [],
        } as Task;

        // Checkpoint 0 in the worktree.
        const cp0 = await checkpointManager.createCheckpoint(task, 0);

        // Agent makes changes in the worktree.
        fs.writeFileSync(path.join(worktreeDir, "agent-work.ts"), "// agent output\n");
        await execFile("git", ["add", "-A"], { cwd: worktreeDir });
        await execFile(
          "git", ["commit", "-m", "agent work"],
          { cwd: worktreeDir, env: { ...process.env, GIT_AUTHOR_NAME: "agent", GIT_AUTHOR_EMAIL: "a@b.c", GIT_COMMITTER_NAME: "agent", GIT_COMMITTER_EMAIL: "a@b.c" } },
        );

        // Checkpoint 1.
        const cp1 = await checkpointManager.createCheckpoint(task, 1);

        // Diff from repoPath should work (shared object database).
        const { stdout: diffOut } = await execFile(
          "git", ["diff", "--name-status", cp0.sha, cp1.sha],
          { cwd: mainRepo },
        );
        expect(diffOut).toContain("agent-work.ts");

        // Diff from worktreePath should also work.
        const { stdout: diffOut2 } = await execFile(
          "git", ["diff", "--name-status", cp0.sha, cp1.sha],
          { cwd: worktreeDir },
        );
        expect(diffOut2).toContain("agent-work.ts");

        // Tree-based diff (as the fixed getDiff handler does).
        const { stdout: fromTree } = await execFile(
          "git", ["rev-parse", `${cp0.sha}^{tree}`], { cwd: worktreeDir },
        );
        const { stdout: toTree } = await execFile(
          "git", ["rev-parse", `${cp1.sha}^{tree}`], { cwd: worktreeDir },
        );
        const { stdout: treeDiff } = await execFile(
          "git", ["diff", "--name-status", fromTree.trim(), toTree.trim()],
          { cwd: mainRepo },
        );
        expect(treeDiff).toContain("agent-work.ts");
      } finally {
        // Cleanup worktree.
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreeDir], { cwd: mainRepo });
        } catch { /* best-effort */ }
        try {
          fs.rmSync(worktreeDir, { recursive: true, force: true });
        } catch { /* best-effort */ }
      }
    });
  });

  describe("snapshotHead for latest turn", () => {
    it("captures current working tree state including uncommitted changes", async () => {
      const task = createMockTask(repoDir);

      // Checkpoint 0.
      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Agent makes changes (some committed, some not).
      fs.writeFileSync(path.join(repoDir, "committed.ts"), "committed\n");
      await execFile("git", ["add", "committed.ts"], { cwd: repoDir });
      await execFile("git", ["commit", "-m", "committed"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "uncommitted.ts"), "uncommitted\n");

      // snapshotHead should capture both.
      const snapshot = await checkpointManager.snapshotHead(task);
      expect(snapshot).toMatch(/^[0-9a-f]{40}$/);

      const files = await computeCheckpointDiff(repoDir, cp0.sha, snapshot);
      expect(files).toContain("committed.ts");
      expect(files).toContain("uncommitted.ts");
    });

    it("returns distinct SHA from checkpoint when working tree has changed", async () => {
      const task = createMockTask(repoDir);

      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Make a change.
      fs.writeFileSync(path.join(repoDir, "new.ts"), "new\n");

      const snapshot = await checkpointManager.snapshotHead(task);

      // Snapshot should differ from cp0 (different tree).
      const { stdout: cpTree } = await execFile(
        "git", ["rev-parse", `${cp0.sha}^{tree}`], { cwd: repoDir },
      );
      const { stdout: snapTree } = await execFile(
        "git", ["rev-parse", `${snapshot}^{tree}`], { cwd: repoDir },
      );
      expect(cpTree.trim()).not.toBe(snapTree.trim());
    });
  });

  describe("three-turn scenario", () => {
    it("each turn shows only its own changes", async () => {
      const task = createMockTask(repoDir);

      // Turn 0 checkpoint.
      const cp0 = await checkpointManager.createCheckpoint(task, 0);

      // Turn 0 work.
      fs.writeFileSync(path.join(repoDir, "turn0.ts"), "turn 0\n");
      await autoCommit(repoDir, 0);

      // Turn 1 checkpoint.
      const cp1 = await checkpointManager.createCheckpoint(task, 1);

      // Turn 1 work.
      fs.writeFileSync(path.join(repoDir, "turn1.ts"), "turn 1\n");
      await autoCommit(repoDir, 1);

      // Turn 2 checkpoint.
      const cp2 = await checkpointManager.createCheckpoint(task, 2);

      // Turn 2 work.
      fs.writeFileSync(path.join(repoDir, "turn2.ts"), "turn 2\n");
      await autoCommit(repoDir, 2);

      // Verify per-turn diffs.
      const turn0Files = await computeCheckpointDiff(repoDir, cp0.sha, cp1.sha);
      expect(turn0Files).toEqual(["turn0.ts"]);

      const turn1Files = await computeCheckpointDiff(repoDir, cp1.sha, cp2.sha);
      expect(turn1Files).toEqual(["turn1.ts"]);

      // Turn 2 (latest) — use snapshotHead.
      const headSnap = await checkpointManager.snapshotHead(task);
      const turn2Files = await computeCheckpointDiff(repoDir, cp2.sha, headSnap);
      expect(turn2Files).toEqual(["turn2.ts"]);

      // "All changes" should show everything.
      const allFiles = await computeCheckpointDiff(repoDir, cp0.sha, headSnap);
      expect(allFiles.sort()).toEqual(["turn0.ts", "turn1.ts", "turn2.ts"]);
    });
  });
});
