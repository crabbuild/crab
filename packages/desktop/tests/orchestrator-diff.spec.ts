/**
 * Integration tests for orchestrator diff computation and apply logic.
 *
 * Creates a temporary git repo, simulates agent changes in a worktree,
 * then exercises: computeDiff, applyAll, cherryPickFiles, discard,
 * and conflict detection.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import type { BrowserWindow, IpcMain, IpcMainInvokeEvent } from "electron";
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
  registerDiffIpc,
} from "../electron/orchestrator-diff";
import type { Task } from "../electron/orchestrator";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import { createWorktree, readTaskMetadata } from "../electron/orchestrator-worktree";
import { parsePatch } from "../src/lib/parse-patch";
import { groupPatchByFile } from "../src/lib/file-state";

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
    baseBranch: "main",
    repoPath,
    createdAt: Date.now(),
    events: [],
    feedback: [],
    todos: [],
  };
}

function diffIpc(task: Task) {
  const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
  const send = vi.fn();
  const owner = { getTask: (id: string) => id === task.id ? task : undefined, getRpcRouter: () => undefined, revertFile: vi.fn() };
  const window = { isDestroyed: () => false, webContents: { send } } as unknown as BrowserWindow;
  registerDiffIpc({ handle: (channel, handler) => { handlers.set(channel, handler); } } as IpcMain, owner, () => window);
  const read = async (payload: { checkpointSha?: string; checkpointFromSha?: string } = {}) => {
    const handler = handlers.get("orchestrator:get-diff");
    if (!handler) throw new Error("Diff handler was not registered");
    return await handler({} as IpcMainInvokeEvent, { taskId: task.id, ...payload }) as { success: boolean; diff?: Awaited<ReturnType<typeof computeDiff>>; error?: string };
  };
  return { read, send };
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
    it("preserves trailing filename spaces in file rows and mutation anchors", () => {
      const raw = "diff --git a/trailing  b/trailing \nnew file mode 100644\n--- /dev/null\n+++ b/trailing \t\n@@ -0,0 +1 @@\n+content\n";
      expect({ files: parseDiffOutput(raw, "A\ttrailing \n").map(file => file.path), anchors: parsePatch(raw).hunks.map(hunk => hunk.path), staged: Object.keys(groupPatchByFile(raw)) })
        .toEqual({ files: ["trailing "], anchors: ["trailing "], staged: ["trailing "] });
    });
    it("preserves quoted UTF-8 identity in remote files, hunk anchors, and staging state", () => {
      const raw = 'diff --git "a/\\351\\233\\252\\tfile" "b/\\351\\233\\252\\tfile"\nnew file mode 100644\n--- /dev/null\n+++ "b/\\351\\233\\252\\tfile"\n@@ -0,0 +1 @@\n+content\n';
      const files = parseDiffOutput(raw, 'A\t"\\351\\233\\252\\tfile"\n');
      expect({ files: files.map(file => file.path), anchors: parsePatch(raw).hunks.map(hunk => hunk.path), staged: Object.keys(groupPatchByFile(raw)) })
        .toEqual({ files: ["雪\tfile"], anchors: ["雪\tfile"], staged: ["雪\tfile"] });
    });

    it("counts changed content that begins with patch-header characters", () => {
      const patch = "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n---before\n+++after\n";
      const [file] = parseDiffOutput(patch, "M\tfile\n");
      expect({ additions: file.additions, deletions: file.deletions }).toEqual({ additions: 1, deletions: 1 });
    });
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

  describe("registered comparison IPC", () => {
    it("routes saved and latest-turn comparisons through the same complete reader", async () => {
      const task = createMockTask(repoDir, "ipc", await getHead(repoDir), repoDir);
      fs.writeFileSync(path.join(repoDir, "README.md"), "saved turn\n");
      const cp = await new CheckpointManager().createCheckpoint(task, 1);
      fs.writeFileSync(path.join(repoDir, "README.md"), "current turn\n");
      fs.writeFileSync(path.join(repoDir, "latest.txt"), "new bytes\n");
      const { read } = diffIpc(task);
      const saved = await read({ checkpointSha: cp.sha });
      const live = await read({ checkpointSha: "HEAD", checkpointFromSha: cp.sha });
      expect({ saved: saved.diff?.files.map(file => file.path), live: live.diff?.files.map(file => file.path).sort(), savedPatch: saved.diff?.rawDiff.includes("+saved turn"), livePatch: live.diff?.rawDiff.includes("+current turn") })
        .toEqual({ saved: ["README.md"], live: ["README.md", "latest.txt"], savedPatch: true, livePatch: true });
    });

    it("returns a visible failure without publishing a clean diff on Git errors", async () => {
      const task = createMockTask(repoDir, "ipc-error", "missing-baseline", repoDir);
      const { read, send } = diffIpc(task);
      const result = await read();
      expect({ success: result.success, missing: result.error?.includes("missing-baseline"), notices: send.mock.calls })
        .toEqual({ success: false, missing: true, notices: [] });
    });

    it("rejects remote saved comparisons before any local filesystem access", async () => {
      const task = { ...createMockTask("ssh://fixture/repo", "remote", "base", "/remote/worktree"), isRemote: true };
      expect(await diffIpc(task).read({ checkpointSha: "saved-ref" })).toEqual({ success: false, error: "Saved-file comparisons require a local workspace" });
    });
  });

  describe("computeDiff", () => {
    it("includes mixed tracked and untracked changes without changing the real index", async () => {
      const task = createMockTask(repoDir, "mixed", await getHead(repoDir), repoDir);
      fs.writeFileSync(path.join(repoDir, "src/index.ts"), "changed\n");
      fs.writeFileSync(path.join(repoDir, "new.txt"), "new bytes\n");
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      const diff = await computeDiff(task);
      expect({ paths: diff.files.map(file => file.path).sort(), index: fs.readFileSync(path.join(repoDir, ".git/index")) })
        .toEqual({ paths: ["new.txt", "src/index.ts"], index });
    });

    it("shows no net change when staged edits were undone in the working tree", async () => {
      const task = createMockTask(repoDir, "net-change", await getHead(repoDir), repoDir);
      const file = path.join(repoDir, "src/index.ts");
      const original = fs.readFileSync(file);
      fs.writeFileSync(file, "staged change\n");
      await execFile("git", ["add", "src/index.ts"], { cwd: repoDir });
      fs.writeFileSync(file, original);
      expect((await computeDiff(task)).files).toEqual([]);
    });

    it.each([false, true])("compares a captured untracked baseline with changed bytes: %s", async changed => {
      const task = createMockTask(repoDir, "baseline", await getHead(repoDir), repoDir);
      fs.writeFileSync(path.join(repoDir, "user.txt"), "original user bytes\n");
      task.baseRef = await new CheckpointManager().captureBase(task);
      if (changed) fs.writeFileSync(path.join(repoDir, "user.txt"), "changed user bytes\n");
      const files = (await computeDiff(task)).files;
      expect(files.map(file => ({ path: file.path, status: file.status }))).toEqual(changed ? [{ path: "user.txt", status: "modified" }] : []);
    });

    it("rejects a missing baseline instead of comparing a different state", async () => {
      fs.writeFileSync(path.join(repoDir, "src/index.ts"), "changed\n");
      await expect(computeDiff(createMockTask(repoDir, "invalid", "refs/heads/missing-baseline", repoDir))).rejects.toThrow();
    });

    it("preserves unusual paths, empty files, binary files, and symbolic links", async () => {
      const task = createMockTask(repoDir, "paths", await getHead(repoDir), repoDir);
      const unusual = process.platform === "win32" ? "snow 雪 space.txt" : "line\tbreak\n雪.txt";
      fs.writeFileSync(path.join(repoDir, unusual), "content\n");
      fs.writeFileSync(path.join(repoDir, "empty.txt"), "");
      fs.writeFileSync(path.join(repoDir, "binary.bin"), Buffer.from([0, 1, 2]));
      const names = ["binary.bin", "empty.txt", unusual];
      if (process.platform !== "win32") {
        fs.symlinkSync("../outside-user-file", path.join(repoDir, "link"));
        names.push("link");
      }
      expect((await computeDiff(task)).files.map(file => ({ path: file.path, status: file.status })).sort((a, b) => a.path.localeCompare(b.path)))
        .toEqual(names.map(filePath => ({ path: filePath, status: "added" })).sort((a, b) => a.path.localeCompare(b.path)));
    });

    it("preserves rename identity when Git quotes the new path", async () => {
      const task = createMockTask(repoDir, "rename", await getHead(repoDir), repoDir);
      const renamed = process.platform === "win32" ? "renamed 雪.ts" : "renamed\t雪.ts";
      await execFile("git", ["mv", "src/utils.ts", renamed], { cwd: repoDir });
      expect((await computeDiff(task)).files.map(file => ({ path: file.path, oldPath: file.oldPath, status: file.status })))
        .toEqual([{ path: renamed, oldPath: "src/utils.ts", status: "renamed" }]);
    });

    it("keeps staged additions visible after an ignore rule matches them", async () => {
      const task = createMockTask(repoDir, "staged-ignored", await getHead(repoDir), repoDir);
      fs.writeFileSync(path.join(repoDir, "staged.txt"), "staged bytes\n");
      await execFile("git", ["add", "staged.txt"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, ".gitignore"), "staged.txt\n");
      expect((await computeDiff(task)).files.map(file => file.path).sort()).toEqual([".gitignore", "staged.txt"]);
    });

    it("reads a split index without changing staged bytes", async () => {
      const task = createMockTask(repoDir, "split-index", await getHead(repoDir), repoDir);
      fs.writeFileSync(path.join(repoDir, "src/index.ts"), "staged\n");
      await execFile("git", ["add", "src/index.ts"], { cwd: repoDir });
      await execFile("git", ["update-index", "--split-index"], { cwd: repoDir });
      const staged = (await execFile("git", ["diff", "--cached"], { cwd: repoDir })).stdout;
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      fs.writeFileSync(path.join(repoDir, "src/index.ts"), "changed\n");
      const diff = await computeDiff(task);
      expect({ paths: diff.files.map(file => file.path), index: fs.readFileSync(path.join(repoDir, ".git/index")), staged: (await execFile("git", ["diff", "--cached"], { cwd: repoDir })).stdout })
        .toEqual({ paths: ["src/index.ts"], index, staged });
    });

    it("compares an unborn workspace with a saved baseline without creating its index", async () => {
      const unborn = path.join(repoDir, "unborn");
      await execFile("git", ["init", "--initial-branch=main", unborn]);
      const task = createMockTask(unborn, "unborn", "HEAD", unborn);
      task.baseRef = await new CheckpointManager().captureBase(task);
      fs.writeFileSync(path.join(unborn, "new.txt"), "new bytes\n");
      expect({ paths: (await computeDiff(task)).files.map(file => file.path), index: fs.existsSync(path.join(unborn, ".git/index")) })
        .toEqual({ paths: ["new.txt"], index: false });
    });

    it("surfaces required filter failures without changing the real index", async () => {
      const task = createMockTask(repoDir, "filter", await getHead(repoDir), repoDir);
      fs.writeFileSync(path.join(repoDir, ".gitattributes"), "*.bin filter=fixture\n");
      fs.writeFileSync(path.join(repoDir, "payload.bin"), "new bytes\n");
      await execFile("git", ["config", "filter.fixture.clean", "exit 17"], { cwd: repoDir });
      await execFile("git", ["config", "filter.fixture.required", "true"], { cwd: repoDir });
      const index = fs.readFileSync(path.join(repoDir, ".git/index"));
      await expect(computeDiff(task)).rejects.toThrow();
      expect(fs.readFileSync(path.join(repoDir, ".git/index"))).toEqual(index);
    });

    it("rejects a recreated folder that would read its parent repository", async () => {
      const task = createMockTask(repoDir, "replaced", await getHead(repoDir), repoDir);
      task.worktreePath = path.join(repoDir, "recreated");
      fs.mkdirSync(task.worktreePath);
      fs.writeFileSync(path.join(repoDir, "README.md"), "primary change\n");
      await expect(computeDiff(task)).rejects.toThrow("no longer matches");
    });

    it("reads saved comparisons from repository objects after the worktree is gone", async () => {
      const task = createMockTask(repoDir, "saved", await getHead(repoDir), repoDir);
      const snapshots = new CheckpointManager();
      const first = await snapshots.createCheckpoint(task, 0);
      fs.writeFileSync(path.join(repoDir, "README.md"), "next turn\n");
      const second = await snapshots.createCheckpoint(task, 1);
      task.worktreePath = path.join(repoDir, "absent");
      expect((await computeDiff(task, first.sha, second.sha)).files.map(file => file.path)).toEqual(["README.md"]);
    });

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
        expect(diff.headRef).toBe("HEAD");
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

  describe("captured Apply contract", () => {
    it.each(["all", "selected"].flatMap(operation => ["missing", "deleted", "symbolic"].map(target => ({ operation, target }))))(
      "refuses $operation Apply with a $target destination before changing either checkout", async ({ operation, target }) => {
        const baseRef = await getHead(repoDir);
        const taskId = `captured-target-${operation}-${target}`;
        const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);
        try {
          await makeAgentChanges(worktreePath);
          fs.writeFileSync(path.join(worktreePath, "pending.txt"), "retained pending work\n");
          const sourceHead = await getHead(worktreePath);
          await execFile("git", ["checkout", "-b", "other"], { cwd: repoDir });
          if (target === "deleted") await execFile("git", ["branch", "-D", "main"], { cwd: repoDir });
          if (target === "symbolic") {
            await execFile("git", ["branch", "referent"], { cwd: repoDir });
            await execFile("git", ["symbolic-ref", "refs/heads/main", "refs/heads/referent"], { cwd: repoDir });
          }
          const task = { ...createMockTask(repoDir, taskId, baseRef, worktreePath), baseBranch: target === "missing" ? undefined : "main" };
          const apply = operation === "all" ? applyAll(task) : cherryPickFiles(task, ["src/new-feature.ts"]);
          await expect(apply).rejects.toThrow();
          expect({ target: await getHead(repoDir), source: await getHead(worktreePath), pending: fs.readFileSync(path.join(worktreePath, "pending.txt"), "utf8"),
            applied: fs.existsSync(path.join(repoDir, "src/new-feature.ts")) }).toEqual({ target: baseRef, source: sourceHead, pending: "retained pending work\n", applied: false });
          if (target === "symbolic") {
            expect((await execFile("git", ["rev-parse", "refs/heads/referent"], { cwd: repoDir })).stdout.trim()).toBe(baseRef);
          }
        } finally { cleanup(worktreePath); }
      },
    );

    it.skipIf(process.platform === "win32")("refuses an alias introduced after building the inactive merge candidate", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = "late-symbolic-target";
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);
      const bin = fs.mkdtempSync(path.join(os.tmpdir(), "crab-apply-ref-fixture-"));
      const originalPath = process.env.PATH;
      const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
      try {
        await makeAgentChanges(worktreePath);
        const source = await getHead(worktreePath);
        await execFile("git", ["checkout", "-b", "other"], { cwd: repoDir });
        await execFile("git", ["branch", "referent"], { cwd: repoDir });
        const git = (await execFile("which", ["git"])).stdout.trim();
        fs.writeFileSync(path.join(bin, "git"), `#!/bin/sh
if [ "$1" = commit-tree ]; then
  ${quote(git)} symbolic-ref refs/heads/main refs/heads/referent || exit 1
fi
exec ${quote(git)} "$@"
`, { mode: 0o755 });
        vi.stubEnv("PATH", `${bin}${path.delimiter}${originalPath ?? ""}`);
        const task = { ...createMockTask(repoDir, taskId, baseRef, worktreePath), baseBranch: "main" };
        await expect(applyAll(task)).rejects.toThrow(/symbolic branch ref/i);
        expect({ alias: (await execFile("git", ["symbolic-ref", "refs/heads/main"], { cwd: repoDir })).stdout.trim(),
          referent: (await execFile("git", ["rev-parse", "refs/heads/referent"], { cwd: repoDir })).stdout.trim(),
          source: await getHead(worktreePath) }).toEqual({ alias: "refs/heads/referent", referent: baseRef, source });
      } finally {
        vi.unstubAllEnvs();
        cleanup(worktreePath);
        cleanup(bin);
      }
    });

    it.each(["all", "selected"])("refuses %s Apply after the source checkout changes branch", async operation => {
      const baseRef = await getHead(repoDir);
      const taskId = `changed-source-${operation}`;
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);
      try {
        await makeAgentChanges(worktreePath);
        await execFile("git", ["checkout", "-b", "user-work"], { cwd: worktreePath });
        fs.writeFileSync(path.join(worktreePath, "pending.txt"), "user branch work\n");
        const sourceHead = await getHead(worktreePath);
        const task = { ...createMockTask(repoDir, taskId, baseRef, worktreePath), baseBranch: "main" };
        const apply = operation === "all" ? applyAll(task) : cherryPickFiles(task, ["src/new-feature.ts"]);
        await expect(apply).rejects.toThrow(/branch/i);
        expect({ target: await getHead(repoDir), source: await getHead(worktreePath), pending: fs.readFileSync(path.join(worktreePath, "pending.txt"), "utf8") })
          .toEqual({ target: baseRef, source: sourceHead, pending: "user branch work\n" });
      } finally { cleanup(worktreePath); }
    });

    it.skipIf(process.platform === "win32")("retains the target and pending work when its source commit hook rejects Apply", async () => {
      const baseRef = await getHead(repoDir);
      const taskId = "rejected-source-commit";
      const worktreePath = await createAgentWorktree(repoDir, taskId, baseRef);
      const hook = path.join(repoDir, ".git/hooks/pre-commit");
      try {
        await makeAgentChanges(worktreePath);
        const sourceHead = await getHead(worktreePath);
        fs.writeFileSync(path.join(worktreePath, "src/new-feature.ts"), "pending task content\n");
        fs.mkdirSync(path.dirname(hook), { recursive: true });
        fs.writeFileSync(hook, "#!/bin/sh\nprintf 'fixture commit rejected\\n' >&2\nexit 1\n", { mode: 0o755 });
        const task = { ...createMockTask(repoDir, taskId, baseRef, worktreePath), baseBranch: "main" };
        await expect(applyAll(task)).rejects.toThrow(/fixture commit rejected/);
        expect({ target: await getHead(repoDir), source: await getHead(worktreePath), pending: fs.readFileSync(path.join(worktreePath, "src/new-feature.ts"), "utf8") })
          .toEqual({ target: baseRef, source: sourceHead, pending: "pending task content\n" });
      } finally { fs.rmSync(hook, { force: true }); cleanup(worktreePath); }
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
      task.branch = branch;
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
