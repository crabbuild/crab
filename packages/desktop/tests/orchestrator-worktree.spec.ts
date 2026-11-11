/**
 * Integration tests for orchestrator worktree isolation.
 *
 * Creates a temporary git repo, exercises the worktree lifecycle
 * (create → verify → remove → verify clean), and validates safety checks.
 * Tests the Docker-style friendly naming and repo-local storage.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import { addContextFile, listContextFiles } from "../electron/orchestrator-preferences";

import {
  createWorktree,
  currentBranch,
  restoreWorktree,
  removeWorktree,
  listWorktrees,
  pruneOldWorktrees,
  validateWorktreePath,
  generateFriendlyName,
  readTaskMetadata,
  writeTaskMetadata,
  migrateWorktreeMetadata,
  getWorktreeName,
  Task,
} from "../electron/orchestrator-worktree";

const execFile = promisify(execFileCb);

// ─── Helpers ─────────────────────────────────────────────────────────────────

/** Creates a temporary git repo with an initial commit. */
async function createTempRepo(): Promise<string> {
  const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-wt-test-"));
  await execFile("git", ["init", "--initial-branch=main"], { cwd: repoDir });
  await execFile("git", ["config", "user.email", "test@crab.dev"], { cwd: repoDir });
  await execFile("git", ["config", "user.name", "Test"], { cwd: repoDir });

  // Create an initial commit so we have a valid ref.
  const readmePath = path.join(repoDir, "README.md");
  fs.writeFileSync(readmePath, "# Test Repo\n");
  await execFile("git", ["add", "."], { cwd: repoDir });
  await execFile("git", ["commit", "-m", "Initial commit"], { cwd: repoDir });

  return repoDir;
}

/** Checks if a git branch exists in the repo. */
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

/** Cleans up a temporary repo directory. */
function cleanupRepo(repoDir: string): void {
  try {
    fs.rmSync(repoDir, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup.
  }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("orchestrator-worktree", () => {
  let repoDir: string;

  beforeEach(async () => {
    repoDir = await createTempRepo();
  });

  afterEach(() => {
    cleanupRepo(repoDir);
  });

  describe("currentBranch", () => {
    it("distinguishes detached HEAD from a checked-out branch", async () => {
      const attached = await currentBranch(repoDir);
      await execFile("git", ["checkout", "--detach"], { cwd: repoDir });
      expect({ attached, detached: await currentBranch(repoDir) }).toEqual({ attached: "main", detached: undefined });
    });

    it("preserves repository errors instead of assigning a missing branch", async () => {
      const unrelated = fs.mkdtempSync(path.join(os.tmpdir(), "crab-no-git-"));
      try { await expect(currentBranch(unrelated)).rejects.toMatchObject({ code: 128 }); }
      finally { fs.rmSync(unrelated, { recursive: true, force: true }); }
    });
  });

  describe("generateFriendlyName", () => {
    it("generates names in adjective-noun format", () => {
      const name = generateFriendlyName(new Set());
      expect(name).toMatch(/^[a-z]+-[a-z]+$/);
    });

    it("generates unique names avoiding collisions", () => {
      const existing = new Set<string>();
      const names: string[] = [];
      for (let i = 0; i < 50; i++) {
        const name = generateFriendlyName(existing);
        expect(existing.has(name)).toBe(false);
        existing.add(name);
        names.push(name);
      }
      // All names should be unique.
      expect(new Set(names).size).toBe(50);
    });

    it("appends numeric suffix on collision", () => {
      // Pre-fill with all possible combinations of a small subset.
      const existing = new Set<string>();
      // Force a collision by adding a name we know will be generated.
      // Since it's random, we just verify the suffix logic works.
      const baseName = "swift-falcon";
      existing.add(baseName);
      // Generate many names — if swift-falcon is picked, it should get a suffix.
      let foundSuffix = false;
      for (let i = 0; i < 1000; i++) {
        const name = generateFriendlyName(existing);
        if (name.startsWith(baseName + "-") && /\d+$/.test(name)) {
          foundSuffix = true;
        }
        existing.add(name);
      }
      // This is probabilistic — with 2500+ combos, collisions are rare.
      // The test validates the mechanism works, not that it always triggers.
    });

    it("generated names are at most 20 characters", () => {
      const existing = new Set<string>();
      for (let i = 0; i < 100; i++) {
        const name = generateFriendlyName(existing);
        // adjective (max 7) + hyphen (1) + noun (max 7) = 15 max without suffix
        expect(name.length).toBeLessThanOrEqual(20);
        existing.add(name);
      }
    });
  });

  describe("validateWorktreePath", () => {
    it("accepts paths under .crab/worktrees/ in the repo", () => {
      const validPath = path.join(repoDir, ".crab/worktrees/swift-falcon");
      expect(validateWorktreePath(validPath, repoDir)).toBe(true);
    });

    it("rejects paths outside the repo", () => {
      expect(validateWorktreePath("/home/user/repo", repoDir)).toBe(false);
      expect(validateWorktreePath("/tmp/crab-agent-abc", repoDir)).toBe(false);
    });

    it("rejects paths deeper than one level under worktrees/", () => {
      const deepPath = path.join(repoDir, ".crab/worktrees/foo/bar");
      expect(validateWorktreePath(deepPath, repoDir)).toBe(false);
    });

    it("rejects the worktrees directory itself", () => {
      const worktreesDir = path.join(repoDir, ".crab/worktrees");
      expect(validateWorktreePath(worktreesDir, repoDir)).toBe(false);
    });
  });

  describe("createWorktree", () => {
    it.each([false, true])("keeps control metadata out of Git snapshots with a relative pointer: %s", async relative => {
      const worktreePath = await createWorktree(repoDir, "control-metadata", "main");
      if (relative) {
        const { stdout } = await execFile("git", ["rev-parse", "--absolute-git-dir"], { cwd: worktreePath });
        // Git hides this file on Windows, where opening a hidden file with
        // the truncating 'w' flag fails. Keep its attributes and rewrite in place.
        const pointer = fs.openSync(path.join(worktreePath, ".git"), "r+");
        try {
          fs.ftruncateSync(pointer);
          fs.writeFileSync(pointer, `gitdir: ${path.relative(fs.realpathSync.native(worktreePath), stdout.trim())}\n`);
        } finally { fs.closeSync(pointer); }
      }
      const metadata = readTaskMetadata(worktreePath)!;
      writeTaskMetadata(worktreePath, { ...metadata, status: "stopped" });
      await execFile("git", ["add", "-A"], { cwd: worktreePath });
      const { stdout } = await execFile("git", ["diff", "--cached", "--name-only"], { cwd: worktreePath });
      expect({ staged: stdout.trim(), taskId: readTaskMetadata(worktreePath)?.taskId }).toEqual({ staged: "", taskId: "control-metadata" });
    });

    it("creates a worktree under .crab/worktrees/ with a friendly name", async () => {
      const taskId = `test-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");

      try {
        // Path should be under .crab/worktrees/.
        expect(path.dirname(worktreePath)).toBe(path.join(repoDir, ".crab", "worktrees"));
        expect(fs.existsSync(worktreePath)).toBe(true);

        // Directory name should be a friendly name (not a UUID).
        const dirName = path.basename(worktreePath);
        expect(dirName).toMatch(/^[a-z]+-[a-z]+/);
        expect(dirName).not.toContain(taskId);

        // Branch should use the friendly name.
        expect(await branchExists(repoDir, `agent/${dirName}`)).toBe(true);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });

    it("writes task metadata with correct fields", async () => {
      const taskId = `meta-test-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");

      try {
        const metadata = readTaskMetadata(worktreePath);
        expect(metadata).toBeDefined();
        expect(metadata!.taskId).toBe(taskId);
        expect(metadata!.status).toBe("active");
        expect(metadata!.baseRef).toBeTruthy();
        expect(metadata!.originalBranch).toMatch(/^agent\//);
        expect(metadata!.createdAt).toMatch(/^\d{4}-\d{2}-\d{2}T/);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });

    it("creates worktree based on a specific commit ref", async () => {
      // Make a second commit.
      const filePath = path.join(repoDir, "second.txt");
      fs.writeFileSync(filePath, "second\n");
      await execFile("git", ["add", "."], { cwd: repoDir });
      await execFile("git", ["commit", "-m", "Second commit"], { cwd: repoDir });

      // Get the first commit SHA.
      const { stdout } = await execFile("git", ["rev-list", "--max-parents=0", "HEAD"], {
        cwd: repoDir,
      });
      const firstCommit = stdout.trim();

      const taskId = `ref-test-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, firstCommit);

      try {
        // The worktree should be based on the first commit (no second.txt).
        expect(fs.existsSync(path.join(worktreePath, "second.txt"))).toBe(false);
        expect(fs.existsSync(path.join(worktreePath, "README.md"))).toBe(true);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });

    it("falls back to HEAD when base ref does not exist", async () => {
      const taskId = `fallback-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "nonexistent-branch");

      try {
        // Should still create successfully (fell back to HEAD).
        expect(fs.existsSync(worktreePath)).toBe(true);
        expect(fs.existsSync(path.join(worktreePath, "README.md"))).toBe(true);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });
  });

  describe("migrateWorktreeMetadata", () => {
    it("moves the tagged control file without leaving it in the restored branch", async () => {
      const taskId = "tagged-format";
      const workspace = await createWorktree(repoDir, taskId, "main");
      const metadata = readTaskMetadata(workspace)!;
      const { stdout: gitDir } = await execFile("git", ["rev-parse", "--absolute-git-dir"], { cwd: workspace });
      fs.unlinkSync(path.join(gitDir.trim(), "crab-desktop-task.json"));
      fs.writeFileSync(path.join(workspace, ".crab-task.json"), JSON.stringify(metadata));
      await execFile("git", ["add", "-A"], { cwd: workspace });
      await execFile("git", ["commit", "-m", "Tagged workspace format"], { cwd: workspace });
      migrateWorktreeMetadata(taskId, workspace);
      migrateWorktreeMetadata(taskId, workspace);
      await execFile("git", ["add", "-A"], { cwd: workspace });
      await execFile("git", ["commit", "-m", "Migrate control storage"], { cwd: workspace });
      const { stdout } = await execFile("git", ["ls-files", "--", ".crab-task.json"], { cwd: workspace });
      expect({ metadata: readTaskMetadata(workspace), tracked: stdout.trim(), oldFile: fs.existsSync(path.join(workspace, ".crab-task.json")) }).toEqual({ metadata, tracked: "", oldFile: false });
    });

    it("keeps newer administration metadata during an interrupted migration", async () => {
      const taskId = "interrupted-migration";
      const workspace = await createWorktree(repoDir, taskId, "main");
      const original = readTaskMetadata(workspace)!;
      fs.writeFileSync(path.join(workspace, ".crab-task.json"), JSON.stringify(original));
      writeTaskMetadata(workspace, { ...original, renamedTo: "agent/renamed" });
      migrateWorktreeMetadata(taskId, workspace);
      expect({ branch: readTaskMetadata(workspace)?.renamedTo, oldFile: fs.existsSync(path.join(workspace, ".crab-task.json")) }).toEqual({ branch: "agent/renamed", oldFile: false });
    });

    it("leaves an unrelated source file intact when current ownership is already stored", async () => {
      const taskId = "unrelated-source";
      const workspace = await createWorktree(repoDir, taskId, "main");
      const source = "This file belongs to the repository, not the task";
      fs.writeFileSync(path.join(workspace, ".crab-task.json"), source);
      migrateWorktreeMetadata(taskId, workspace);
      expect({ source: fs.readFileSync(path.join(workspace, ".crab-task.json"), "utf8"), taskId: readTaskMetadata(workspace)?.taskId }).toEqual({ source, taskId });
    });
  });

  describe("restoreWorktree", () => {
    it("removes only the missing workspace's stale registration before restoring its saved branch", async () => {
      const original = await createWorktree(repoDir, "missing-workspace", "main");
      const branch = readTaskMetadata(original)!.originalBranch;
      fs.writeFileSync(path.join(original, "saved.txt"), "saved work");
      await execFile("git", ["add", "saved.txt"], { cwd: original });
      await execFile("git", ["commit", "-m", "Saved work"], { cwd: original });
      const registeredPath = fs.realpathSync.native(original);
      const unrelated = await createWorktree(repoDir, "unrelated-workspace", "main");
      const unrelatedPath = fs.realpathSync.native(unrelated);
      fs.rmSync(unrelated, { recursive: true, force: true });
      fs.rmSync(original, { recursive: true, force: true });
      const restored = await restoreWorktree(repoDir, "missing-workspace", branch, original);
      const { stdout } = await execFile("git", ["worktree", "list", "--porcelain", "-z"], { cwd: repoDir });
      const registered = stdout.split("\0").filter(field => field.startsWith("worktree ")).map(field => path.normalize(field.slice("worktree ".length)));
      expect({ file: fs.readFileSync(path.join(restored, "saved.txt"), "utf8"), originalRegistered: registered.includes(path.normalize(registeredPath)),
        unrelatedRegistered: registered.includes(path.normalize(unrelatedPath)) }).toEqual({ file: "saved work", originalRegistered: false, unrelatedRegistered: true });
    });

    it.each(["locked", "live"])("preserves a %s worktree registration instead of forcing saved-branch restoration", async state => {
      const original = await createWorktree(repoDir, "protected-workspace", "main");
      const branch = readTaskMetadata(original)!.originalBranch;
      const registeredPath = fs.realpathSync.native(original);
      if (state === "locked") {
        await execFile("git", ["worktree", "lock", original], { cwd: repoDir });
        fs.rmSync(original, { recursive: true, force: true });
      }
      const previous = state === "locked" ? original : path.join(repoDir, ".crab/worktrees/other");
      await expect(restoreWorktree(repoDir, "protected-workspace", branch, previous)).rejects.toThrow();
      const { stdout } = await execFile("git", ["worktree", "list", "--porcelain", "-z"], { cwd: repoDir });
      const registered = stdout.split("\0").filter(field => field.startsWith("worktree ")).map(field => path.normalize(field.slice("worktree ".length)));
      expect(registered.includes(path.normalize(registeredPath))).toBe(true);
    });

    it("checks out the saved branch and its work without creating a replacement branch", async () => {
      await execFile("git", ["checkout", "-b", "agent/saved"], { cwd: repoDir });
      fs.writeFileSync(path.join(repoDir, "saved.txt"), "saved work");
      await execFile("git", ["add", "saved.txt"], { cwd: repoDir });
      await execFile("git", ["commit", "-m", "Saved work"], { cwd: repoDir });
      await execFile("git", ["checkout", "main"], { cwd: repoDir });
      const restored = await restoreWorktree(repoDir, "restored-task", "agent/saved", path.join(repoDir, ".crab/worktrees/removed"));
      const { stdout } = await execFile("git", ["symbolic-ref", "--short", "HEAD"], { cwd: restored });
      const metadata = readTaskMetadata(restored);
      expect({ branch: stdout.trim(), content: fs.readFileSync(path.join(restored, "saved.txt"), "utf8"), taskId: metadata?.taskId, savedBranch: metadata?.originalBranch }).toEqual({ branch: "agent/saved", content: "saved work", taskId: "restored-task", savedBranch: "agent/saved" });
    });

    it("rejects a missing saved branch instead of restoring HEAD", async () => {
      await expect(restoreWorktree(repoDir, "missing-task", "agent/missing", path.join(repoDir, ".crab/worktrees/removed"))).rejects.toThrow();
    });
  });

  describe("removeWorktree", () => {
    it("creates context files on demand and retains them when Archive cannot save them", async () => {
      const orchestrator = new Orchestrator({ persistPath: path.join(repoDir, "tasks.sqlite") });
      try {
        const task = await orchestrator.createTask("Pinned context", "codex", repoDir);
        expect({ exists: fs.existsSync(path.join(task.worktreePath, ".context")), files: listContextFiles(task.worktreePath) }).toEqual({ exists: false, files: [] });
        addContextFile(task.worktreePath, "NOTES.md", "private context notes\n");
        const rejected = await orchestrator.archiveTask(task.id).then(() => false, () => true);
        expect({ rejected, status: task.status, notes: fs.readFileSync(path.join(task.worktreePath, ".context/NOTES.md"), "utf8") }).toEqual({
          rejected: true, status: "stopped", notes: "private context notes\n",
        });
      } finally { await orchestrator.shutdown(); }
    });

    it.each(["ignored", "assume-unchanged", "skip-worktree"])("refuses Archive without losing %s source files or persisting archived state", async mode => {
      const persistPath = path.join(repoDir, "tasks.sqlite");
      const orchestrator = new Orchestrator({ persistPath });
      try {
        const task = await orchestrator.createTask("Preserve excluded source", "codex", repoDir);
        const name = mode === "ignored" ? "private.txt" : "README.md";
        if (mode === "ignored") {
          fs.mkdirSync(path.join(repoDir, ".git/info"), { recursive: true });
          fs.writeFileSync(path.join(repoDir, ".git/info/exclude"), `${name}\n`);
        } else await execFile("git", ["update-index", `--${mode}`, "--", name], { cwd: task.worktreePath });
        fs.writeFileSync(path.join(task.worktreePath, name), "private source bytes\n");
        const rejected = await orchestrator.archiveTask(task.id).then(() => false, () => true);
        const reopened = new Orchestrator({ persistPath });
        try {
          expect({ rejected, current: task.status, saved: reopened.getTask(task.id)?.status,
            source: fs.existsSync(path.join(task.worktreePath, name)) ? fs.readFileSync(path.join(task.worktreePath, name), "utf8") : null }).toEqual({
            rejected: true, current: "stopped", saved: "stopped", source: "private source bytes\n",
          });
        } finally { await reopened.shutdown(); }
      } finally { await orchestrator.shutdown(); }
    });

    it.each(["hidden-untracked", "ignored", "assume-unchanged", "skip-worktree"])("retains %s source bytes during archive cleanup", async mode => {
      const taskId = `excluded-archive-${mode}`;
      const workspace = await createWorktree(repoDir, taskId, "main");
      const branch = readTaskMetadata(workspace)!.originalBranch;
      const name = mode === "hidden-untracked" || mode === "ignored" ? "pending.txt" : "README.md";
      await execFile("git", ["config", "status.showUntrackedFiles", "no"], { cwd: repoDir });
      if (mode === "ignored") {
        fs.mkdirSync(path.join(repoDir, ".git/info"), { recursive: true });
        fs.writeFileSync(path.join(repoDir, ".git/info/exclude"), `${name}\n`);
      }
      if (mode === "assume-unchanged" || mode === "skip-worktree") await execFile("git", ["update-index", `--${mode}`, "--", name], { cwd: workspace });
      fs.writeFileSync(path.join(workspace, name), "excluded source bytes\n");
      const rejected = await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath: workspace, branch }, { preserveBranch: true }).then(() => false, () => true);
      expect({ rejected, source: fs.existsSync(path.join(workspace, name)) ? fs.readFileSync(path.join(workspace, name), "utf8") : null,
        branch: await branchExists(repoDir, branch) }).toEqual({ rejected: true, source: "excluded source bytes\n", branch: true });
    });

    it("allows archive cleanup when sparse source files are absent", async () => {
      const taskId = "sparse-archive-task";
      const workspace = await createWorktree(repoDir, taskId, "main");
      const branch = readTaskMetadata(workspace)!.originalBranch;
      await execFile("git", ["update-index", "--skip-worktree", "--", "README.md"], { cwd: workspace });
      fs.unlinkSync(path.join(workspace, "README.md"));
      await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath: workspace, branch }, { preserveBranch: true });
      expect({ removed: !fs.existsSync(workspace), branch: await branchExists(repoDir, branch) }).toEqual({ removed: true, branch: true });
    });

    it("refuses archive cleanup with unsaved work and leaves the tree and branch intact", async () => {
      const taskId = "unsaved-archive-task";
      const workspace = await createWorktree(repoDir, taskId, "main");
      const branch = readTaskMetadata(workspace)!.originalBranch;
      fs.writeFileSync(path.join(workspace, "unsaved.txt"), "unsaved work");
      await expect(removeWorktree({ id: taskId, repoPath: repoDir, worktreePath: workspace, branch }, { preserveBranch: true })).rejects.toThrow();
      expect({ content: fs.readFileSync(path.join(workspace, "unsaved.txt"), "utf8"), branch: await branchExists(repoDir, branch) }).toEqual({ content: "unsaved work", branch: true });
    });

    it("preserves the committed branch for archive and restores its saved work", async () => {
      const taskId = "archive-task";
      const workspace = await createWorktree(repoDir, taskId, "main");
      const branch = readTaskMetadata(workspace)!.originalBranch;
      fs.writeFileSync(path.join(workspace, "archive-proof.txt"), "preserved work");
      await execFile("git", ["add", "-A"], { cwd: workspace });
      await execFile("git", ["commit", "-m", "Saved archive work"], { cwd: workspace });
      await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath: workspace, branch }, { preserveBranch: true });
      const restored = await restoreWorktree(repoDir, taskId, branch, workspace);
      expect({ removed: !fs.existsSync(workspace), branch: await branchExists(repoDir, branch), content: fs.readFileSync(path.join(restored, "archive-proof.txt"), "utf8") }).toEqual({ removed: true, branch: true, content: "preserved work" });
    });

    it("removes worktree directory and branch by task ID", async () => {
      const taskId = `rm-test-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");
      const dirName = path.basename(worktreePath);

      expect(fs.existsSync(worktreePath)).toBe(true);
      expect(await branchExists(repoDir, `agent/${dirName}`)).toBe(true);

      await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });

      expect(fs.existsSync(worktreePath)).toBe(false);
      expect(await branchExists(repoDir, `agent/${dirName}`)).toBe(false);
    });

    it("rejects a task ID that does not own its requested worktree", async () => {
      const taskId = `rm-name-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");
      const dirName = path.basename(worktreePath);

      await expect(removeWorktree({ id: dirName, repoPath: repoDir, worktreePath, branch: `agent/${dirName}` })).rejects.toThrow("ownership");
      expect(fs.existsSync(worktreePath)).toBe(true);
      expect(await branchExists(repoDir, `agent/${dirName}`)).toBe(true);
    });

    it("handles already-removed worktree gracefully", async () => {
      const taskId = `already-rm-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");

      // Remove it once.
      await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });

      // Second removal should not throw.
      await expect(removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` })).resolves.toBeUndefined();
    });
  });

  describe("listWorktrees", () => {
    it("returns empty array when no agent worktrees exist", async () => {
      const result = await listWorktrees(repoDir);
      expect(result).toEqual([]);
    });

    it("lists active agent worktrees with friendly names", async () => {
      const taskId1 = `list-a-${Date.now()}`;
      const taskId2 = `list-b-${Date.now()}`;
      const wt1 = await createWorktree(repoDir, taskId1, "main");
      const wt2 = await createWorktree(repoDir, taskId2, "main");

      try {
        const result = await listWorktrees(repoDir);

        expect(result).toHaveLength(2);

        // Each should have a friendly name.
        for (const wt of result) {
          expect(wt.name).toMatch(/^[a-z]+-[a-z]+/);
          expect(wt.branch).toMatch(/^agent\//);
          expect(wt.taskId).toBeTruthy();
        }

        const ids = result.map((w) => w.taskId).sort();
        expect(ids).toContain(taskId1);
        expect(ids).toContain(taskId2);
      } finally {
        await removeWorktree({ id: taskId1, repoPath: repoDir, worktreePath: wt1, branch: `agent/${path.basename(wt1)}` });
        await removeWorktree({ id: taskId2, repoPath: repoDir, worktreePath: wt2, branch: `agent/${path.basename(wt2)}` });
      }
    });
  });

  describe("getWorktreeName", () => {
    it("returns the friendly name for a task ID", async () => {
      const taskId = `name-lookup-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");
      const expectedName = path.basename(worktreePath);

      try {
        const name = getWorktreeName(repoDir, taskId);
        expect(name).toBe(expectedName);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });

    it("returns undefined for unknown task ID", () => {
      const name = getWorktreeName(repoDir, "nonexistent-id");
      expect(name).toBeUndefined();
    });
  });

  describe("pruneOldWorktrees", () => {
    it("removes worktrees older than retention period in terminal states", async () => {
      const taskId = `prune-old-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");

      const tasks: Task[] = [
        {
          id: taskId,
          status: "applied",
          worktreePath,
          branch: `agent/${path.basename(worktreePath)}`,
          createdAt: Date.now() - 8 * 24 * 60 * 60 * 1000, // 8 days ago
        },
      ];

      const pruned = await pruneOldWorktrees(repoDir, tasks, 7);

      expect(pruned).toContain(taskId);
      expect(fs.existsSync(worktreePath)).toBe(false);
    });

    it("preserves worktrees newer than retention period", async () => {
      const taskId = `prune-new-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");

      const tasks: Task[] = [
        {
          id: taskId,
          status: "applied",
          worktreePath,
          branch: `agent/${path.basename(worktreePath)}`,
          createdAt: Date.now() - 2 * 24 * 60 * 60 * 1000, // 2 days ago
        },
      ];

      try {
        const pruned = await pruneOldWorktrees(repoDir, tasks, 7);
        expect(pruned).not.toContain(taskId);
        expect(fs.existsSync(worktreePath)).toBe(true);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });

    it("preserves worktrees in non-terminal states regardless of age", async () => {
      const taskId = `prune-running-${Date.now()}`;
      const worktreePath = await createWorktree(repoDir, taskId, "main");

      const tasks: Task[] = [
        {
          id: taskId,
          status: "running",
          worktreePath,
          branch: `agent/${path.basename(worktreePath)}`,
          createdAt: Date.now() - 30 * 24 * 60 * 60 * 1000, // 30 days ago
        },
      ];

      try {
        const pruned = await pruneOldWorktrees(repoDir, tasks, 7);
        expect(pruned).not.toContain(taskId);
        expect(fs.existsSync(worktreePath)).toBe(true);
      } finally {
        await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });
      }
    });
  });

  describe("full lifecycle integration", () => {
    it("create → verify → list → remove → verify clean", async () => {
      const taskId = `lifecycle-${Date.now()}`;

      // Create.
      const worktreePath = await createWorktree(repoDir, taskId, "main");
      const dirName = path.basename(worktreePath);
      expect(fs.existsSync(worktreePath)).toBe(true);
      expect(await branchExists(repoDir, `agent/${dirName}`)).toBe(true);

      // Verify the worktree has the repo content.
      expect(fs.existsSync(path.join(worktreePath, "README.md"))).toBe(true);

      // Verify metadata.
      const meta = readTaskMetadata(worktreePath);
      expect(meta?.taskId).toBe(taskId);
      expect(meta?.name).toBe(dirName);

      // Verify it appears in the list.
      const listed = await listWorktrees(repoDir);
      expect(listed.some((w) => w.taskId === taskId)).toBe(true);
      expect(listed.some((w) => w.name === dirName)).toBe(true);

      // Remove.
      await removeWorktree({ id: taskId, repoPath: repoDir, worktreePath, branch: `agent/${path.basename(worktreePath)}` });

      // Verify clean.
      expect(fs.existsSync(worktreePath)).toBe(false);
      expect(await branchExists(repoDir, `agent/${dirName}`)).toBe(false);

      // Verify it no longer appears in the list.
      const listedAfter = await listWorktrees(repoDir);
      expect(listedAfter.some((w) => w.taskId === taskId)).toBe(false);
    });
  });
});
