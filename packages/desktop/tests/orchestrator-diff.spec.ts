/**
 * Integration tests for orchestrator diff computation and apply logic.
 *
 * Creates a temporary git repo, simulates agent changes in a worktree,
 * then exercises: computeDiff, applyAll, cherryPickFiles, discard,
 * and conflict detection.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import {
  computeDiff,
  applyAll,
  cherryPickFiles,
  discard,
  parseDiffOutput,
} from "../electron/orchestrator-diff";
import type { Task } from "../electron/orchestrator";
import { createWorktree, readTaskMetadata } from "../electron/orchestrator-worktree";

const execFile = promisify(execFileCb);

// ─── Helpers ─────────────────────────────────────────────────────────────────

/** Creates a temporary git repo with an initial commit. */
async function createTempRepo(): Promise<string> {
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-diff-test-"));
  await execFile("git", ["init", "--initial-branch=main"], { cwd: repoDir });
  await execFile("git", ["config", "user.email", "test@crab.dev"], { cwd: repoDir });
  await execFile("git", ["config", "user.name", "Test"], { cwd: repoDir });

  // Create initial files.
  fs.writeFileSync(path.join(repoDir, "README.md"), "# Test Repo\n");
  fs.mkdirSync(path.join(repoDir, "src"), { recursive: true });
  fs.writeFileSync(path.join(repoDir, "src/index.ts"), "export const hello = 'world';\n");
  fs.writeFileSync(path.join(repoDir, "src/utils.ts"), "export function add(a: number, b: number) { return a + b; }\n");

  await execFile("git", ["add", "."], { cwd: repoDir });
  await execFile("git", ["commit", "-m", "Initial commit"], { cwd: repoDir });

  return repoDir;
}

/** Gets the current HEAD SHA. */
async function getHead(repoPath: string): Promise<string> {
  const { stdout } = await execFile("git", ["rev-parse", "HEAD"], { cwd: repoPath });
  return stdout.trim();
}

/** Creates a worktree simulating an agent branch. */
async function createAgentWorktree(
  repoPath: string,
  taskId: string,
  baseRef: string,
): Promise<string> {
  const worktreePath = path.join(os.tmpdir(), `crab-agent-${taskId}`);
  const branch = `agent/${taskId}`;
  await execFile("git", ["worktree", "add", "-b", branch, worktreePath, baseRef], {
    cwd: repoPath,
  });
  // Configure git user in worktree for commits.
  await execFile("git", ["config", "user.email", "agent@crab.dev"], { cwd: worktreePath });
  await execFile("git", ["config", "user.name", "Agent"], { cwd: worktreePath });
  return worktreePath;
}

/** Makes changes in the worktree and commits them. */
async function makeAgentChanges(worktreePath: string): Promise<void> {
  // Modify an existing file.
  fs.writeFileSync(
    path.join(worktreePath, "src/index.ts"),
    "export const hello = 'world';\nexport const version = '1.0.0';\n",
  );

  // Add a new file.
  fs.writeFileSync(
    path.join(worktreePath, "src/new-feature.ts"),
    "export function newFeature() { return 'implemented'; }\n",
  );

  // Delete a file.
  fs.unlinkSync(path.join(worktreePath, "src/utils.ts"));

  await execFile("git", ["add", "."], { cwd: worktreePath });
  await execFile("git", ["commit", "-m", "Agent changes"], { cwd: worktreePath });
}

/** Creates a mock Task object for testing. */
function createMockTask(
  repoPath: string,
  taskId: string,
  baseRef: string,
  worktreePath: string,
): Task {
  return {
    id: taskId,
    prompt: "Test task prompt",
    agentType: "claude-code",
    status: "reviewing",
    branch: `agent/${taskId}`,
    worktreePath,
    baseRef,
    repoPath,
    createdAt: Date.now(),
    events: [],
    feedback: [],
  };
}

/** Cleans up a temporary repo directory. */
function cleanup(dir: string): void {
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup.
  }
}

/** Checks if a git branch exists. */
async function branchExists(repoPath: string, branch: string): Promise<boolean> {
  try {
    await execFile("git", ["rev-parse", "--verify", `refs/heads/${branch}`], {
      cwd: repoPath,
    });
    return true;
  } catch {
    return false;
  }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("orchestrator-diff", () => {
  let repoDir: string;

  beforeEach(async () => {
    repoDir = await createTempRepo();
  });

  afterEach(() => {
    cleanup(repoDir);
  });

  describe("parseDiffOutput", () => {
    it("returns empty array for empty diff", () => {
      expect(parseDiffOutput("", "")).toEqual([]);
    });

    it("parses a simple modification diff", () => {
      const rawDiff = `diff --git a/src/index.ts b/src/index.ts
index abc1234..def5678 100644
--- a/src/index.ts
+++ b/src/index.ts
@@ -1 +1,2 @@
 export const hello = 'world';
+export const version = '1.0.0';
`;
      const nameStatus = "M\tsrc/index.ts\n";

      const files = parseDiffOutput(rawDiff, nameStatus);
      expect(files).toHaveLength(1);
      expect(files[0].path).toBe("src/index.ts");
      expect(files[0].status).toBe("modified");
      expect(files[0].additions).toBe(1);
      expect(files[0].deletions).toBe(0);
    });

    it("parses renamed files", () => {
      const rawDiff = `diff --git a/old-name.ts b/new-name.ts
similarity index 100%
rename from old-name.ts
rename to new-name.ts
`;
      const nameStatus = "R100\told-name.ts\tnew-name.ts\n";

      const files = parseDiffOutput(rawDiff, nameStatus);
      expect(files).toHaveLength(1);
      expect(files[0].path).toBe("new-name.ts");
      expect(files[0].status).toBe("renamed");
      expect(files[0].oldPath).toBe("old-name.ts");
    });
  });

  describe("computeDiff", () => {
    it("computes a structured diff with file list and patches", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `diff-test-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        await makeAgentChanges(worktreePath);

        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);
        const diff = await computeDiff(task);

        expect(diff.taskId).toBe(taskId);
        expect(diff.baseRef).toBe(baseRef);
        expect(diff.headRef).toMatch(/^[0-9a-f]{40}$/);
        expect(diff.files.length).toBeGreaterThan(0);
        expect(diff.rawDiff).toContain("diff --git");

        // Check that we have the expected file changes.
        const filePaths = diff.files.map((f) => f.path);
        expect(filePaths).toContain("src/index.ts");
        expect(filePaths).toContain("src/new-feature.ts");
        expect(filePaths).toContain("src/utils.ts");

        // Verify statuses.
        const indexFile = diff.files.find((f) => f.path === "src/index.ts");
        expect(indexFile?.status).toBe("modified");
        expect(indexFile?.additions).toBeGreaterThan(0);

        const newFile = diff.files.find((f) => f.path === "src/new-feature.ts");
        expect(newFile?.status).toBe("added");

        const deletedFile = diff.files.find((f) => f.path === "src/utils.ts");
        expect(deletedFile?.status).toBe("deleted");

        // Verify totals.
        expect(diff.totalAdditions).toBeGreaterThan(0);
        expect(diff.totalDeletions).toBeGreaterThan(0);
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });

    it("returns empty file list when no changes exist", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `no-changes-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        // No changes made — just the worktree creation.
        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);
        const diff = await computeDiff(task);

        expect(diff.files).toHaveLength(0);
        expect(diff.totalAdditions).toBe(0);
        expect(diff.totalDeletions).toBe(0);
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });
  });

  describe("applyAll", () => {
    it("merges agent branch into current branch cleanly", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `apply-test-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        await makeAgentChanges(worktreePath);

        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);
        const result = await applyAll(task);

        expect(result.success).toBe(true);
        expect(result.conflicts).toBeUndefined();

        // Verify the merge happened — new file should exist in main repo.
        expect(fs.existsSync(path.join(repoDir, "src/new-feature.ts"))).toBe(true);

        // Verify the deleted file is gone.
        expect(fs.existsSync(path.join(repoDir, "src/utils.ts"))).toBe(false);

        // Verify the modified file has the new content.
        const content = fs.readFileSync(path.join(repoDir, "src/index.ts"), "utf-8");
        expect(content).toContain("version");

        // Verify it was a merge commit (--no-ff).
        const { stdout: log } = await execFile("git", ["log", "--oneline", "-1"], { cwd: repoDir });
        expect(log).toContain("Apply agent task");
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });

    it("lands into an inactive target branch without checking it out", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `inactive-target-${Date.now()}`;
      const targetBranch = `integration-${taskId}`;
      const otherBranch = `other-${taskId}`;

      await execFile("git", ["branch", targetBranch, baseRef], { cwd: repoDir });
      await execFile("git", ["checkout", "-b", otherBranch, baseRef], { cwd: repoDir });
      const worktreePath = await createAgentWorktree(repoDir, taskId, targetBranch);

      try {
        await makeAgentChanges(worktreePath);

        const task = {
          ...createMockTask(repoDir, taskId, baseRef, worktreePath),
          baseBranch: targetBranch,
        };
        const result = await applyAll(task);

        expect(result.success).toBe(true);

        const { stdout: current } = await execFile(
          "git",
          ["symbolic-ref", "-q", "--short", "HEAD"],
          { cwd: repoDir },
        );
        expect(current.trim()).toBe(otherBranch);

        const { stdout: log } = await execFile(
          "git",
          ["log", "--oneline", "-1", targetBranch],
          { cwd: repoDir },
        );
        expect(log).toContain("Apply agent task");

        const { stdout: landedFile } = await execFile(
          "git",
          ["show", `${targetBranch}:src/new-feature.ts`],
          { cwd: repoDir },
        );
        expect(landedFile).toContain("newFeature");
        expect(fs.existsSync(path.join(repoDir, "src/new-feature.ts"))).toBe(false);
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["checkout", "main"], { cwd: repoDir });
        } catch { /* best-effort */ }
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        for (const branch of [`agent/${taskId}`, targetBranch, otherBranch]) {
          try {
            await execFile("git", ["branch", "-D", branch], { cwd: repoDir });
          } catch { /* already cleaned */ }
        }
      }
    });

    it("detects conflicts and returns conflict file list", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `conflict-test-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        // Make conflicting changes on main.
        fs.writeFileSync(
          path.join(repoDir, "src/index.ts"),
          "export const hello = 'conflicting change on main';\n",
        );
        await execFile("git", ["add", "."], { cwd: repoDir });
        await execFile("git", ["commit", "-m", "Conflicting change on main"], { cwd: repoDir });

        // Make different changes on the agent branch.
        fs.writeFileSync(
          path.join(worktreePath, "src/index.ts"),
          "export const hello = 'agent change';\n",
        );
        await execFile("git", ["add", "."], { cwd: worktreePath });
        await execFile("git", ["commit", "-m", "Agent conflicting change"], { cwd: worktreePath });

        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);
        const result = await applyAll(task);

        expect(result.success).toBe(false);
        expect(result.conflicts).toBeDefined();
        expect(result.conflicts!.length).toBeGreaterThan(0);
        expect(result.conflicts).toContain("src/index.ts");

        // Verify the merge was aborted — repo should be clean.
        const { stdout: status } = await execFile("git", ["status", "--porcelain"], { cwd: repoDir });
        expect(status.trim()).toBe("");
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });
  });

  describe("cherryPickFiles", () => {
    it("commits only the selected files from the agent branch", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `cherry-pick-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        await makeAgentChanges(worktreePath);

        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);

        // Only cherry-pick the new file, not the modification or deletion.
        await cherryPickFiles(task, ["src/new-feature.ts"]);

        // The new file should exist.
        expect(fs.existsSync(path.join(repoDir, "src/new-feature.ts"))).toBe(true);

        // The modified file should NOT have the agent's changes.
        const indexContent = fs.readFileSync(path.join(repoDir, "src/index.ts"), "utf-8");
        expect(indexContent).not.toContain("version");

        // The deleted file should still exist (we didn't cherry-pick the deletion).
        expect(fs.existsSync(path.join(repoDir, "src/utils.ts"))).toBe(true);

        const { stdout: committed } = await execFile("git", ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"], { cwd: repoDir });
        expect(committed.trim().split("\n")).toEqual(["src/new-feature.ts"]);
      } finally {
        // Reset the repo state.
        try {
          await execFile("git", ["checkout", "--", "."], { cwd: repoDir });
          await execFile("git", ["clean", "-fd"], { cwd: repoDir });
          await execFile("git", ["reset", "HEAD"], { cwd: repoDir });
        } catch { /* best-effort */ }
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });

    it("handles empty file list gracefully", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `cherry-empty-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);
        // Should not throw.
        await expect(cherryPickFiles(task, [])).resolves.toBeUndefined();
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });
  });

  describe("discard", () => {
    it("removes worktree and branch", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `discard-test-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, baseRef);

      const branch = readTaskMetadata(worktreePath)!.originalBranch;
      // Use the product's registered repo-local worktree; cleanup deliberately
      // refuses legacy temporary directories outside its ownership boundary.
      expect(fs.existsSync(worktreePath)).toBe(true);
      expect(await branchExists(repoDir, branch)).toBe(true);

      const task = createMockTask(repoDir, taskId, baseRef, worktreePath);
      await discard(task);

      // Both should be gone.
      expect(fs.existsSync(worktreePath)).toBe(false);
      expect(await branchExists(repoDir, branch)).toBe(false);
    });
  });

  describe("full lifecycle integration", () => {
    it("agent makes changes → diff computed → apply merges cleanly", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `lifecycle-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        // 1. Agent makes changes.
        await makeAgentChanges(worktreePath);

        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);

        // 2. Compute diff.
        const diff = await computeDiff(task);
        expect(diff.files.length).toBe(3); // modified, added, deleted
        expect(diff.totalAdditions).toBeGreaterThan(0);

        // 3. Apply all.
        const result = await applyAll(task);
        expect(result.success).toBe(true);

        // 4. Verify final state.
        expect(fs.existsSync(path.join(repoDir, "src/new-feature.ts"))).toBe(true);
        expect(fs.existsSync(path.join(repoDir, "src/utils.ts"))).toBe(false);
        const content = fs.readFileSync(path.join(repoDir, "src/index.ts"), "utf-8");
        expect(content).toContain("version");
      } finally {
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });

    it("cherry-pick applies subset of changes", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = `cherry-lifecycle-${Date.now()}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);

      try {
        // Agent makes changes.
        await makeAgentChanges(worktreePath);

        const task = createMockTask(repoDir, taskId, baseRef, worktreePath);

        // Compute diff to verify what's available.
        const diff = await computeDiff(task);
        expect(diff.files.length).toBe(3);

        // Cherry-pick only the modified file.
        await cherryPickFiles(task, ["src/index.ts"]);

        // Only the modified file should have agent changes.
        const content = fs.readFileSync(path.join(repoDir, "src/index.ts"), "utf-8");
        expect(content).toContain("version");

        // New file should NOT exist (not cherry-picked).
        expect(fs.existsSync(path.join(repoDir, "src/new-feature.ts"))).toBe(false);

        // Deleted file should still exist (not cherry-picked).
        expect(fs.existsSync(path.join(repoDir, "src/utils.ts"))).toBe(true);
      } finally {
        // Reset the repo state.
        try {
          await execFile("git", ["checkout", "--", "."], { cwd: repoDir });
          await execFile("git", ["clean", "-fd"], { cwd: repoDir });
          await execFile("git", ["reset", "HEAD"], { cwd: repoDir });
        } catch { /* best-effort */ }
        cleanup(worktreePath);
        try {
          await execFile("git", ["worktree", "remove", "--force", worktreePath], { cwd: repoDir });
        } catch { /* already cleaned */ }
        try {
          await execFile("git", ["branch", "-D", `agent/${taskId}`], { cwd: repoDir });
        } catch { /* already cleaned */ }
      }
    });
  });
});
