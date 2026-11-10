/**
 * Unit tests for the Orchestrator task lifecycle manager.
 *
 * Mocks git commands and agent adapters to test the state machine
 * transitions, persistence, and timeout behaviour without spawning
 * real processes.
 */

import type { AgentInputResponse } from "../electron/orchestrator-adapters";
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import Sqlite from "better-sqlite3";

import { Orchestrator, Task, type TaskSubmission } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { ChatJournal } from "../electron/orchestrator-chat";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import { OrchestratorTaskDatabase } from "../electron/orchestrator-task-db";
import { AgentSubmissionUncertainError, type AgentTurnOptions } from "../electron/orchestrator-adapters";
import { agentRegistry } from "../electron/orchestrator-registry";
import { WorkspaceScripts } from "../electron/orchestrator-scripts";
import { readTaskMetadata } from "../electron/orchestrator-worktree";
import * as diff from "../electron/orchestrator-diff";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock the worktree module — no real git operations.
vi.mock("../electron/orchestrator-worktree", () => ({
  createWorktree: vi.fn(async (_repoPath: string, taskId: string, _baseRef: string) => {
    return path.join(os.tmpdir(), `crab-agent-${taskId}`);
  }),
  removeWorktree: vi.fn(async () => {}),
  restoreWorktree: vi.fn(async (_repoPath: string, taskId: string) => path.join(os.tmpdir(), `crab-agent-${taskId}`)),
  readTaskMetadata: vi.fn(() => undefined),
  migrateWorktreeMetadata: vi.fn(),
}));

// Track mock adapter instances for assertions.
let mockAdapterInstances: MockAdapter[] = [];
let hangPostTurnGitAdd = false;
let failPostTurnGitAdd = false;
let heldPostTurnGitAdd: Promise<{ stdout: string; stderr: string }> | undefined;
let execFileCalls: Array<{ command: string; args: string[]; cwd?: string }> = [];
let mockCurrentBranch = "main";
let checkpointRefs = new Set<string>();

class MockAdapter {
  name = "Mock Agent";
  binary = "mock-agent";
  private eventListeners = new Set<(event: any) => void>();
  private exitListeners = new Set<(code: number) => void>();
  spawned = false;
  cancelled = false;
  spawnPrompt = "";
  spawnCwd = "";
  spawnOptions?: AgentTurnOptions;
  declare review?: (instructions: string, cwd: string, env?: Record<string, string>, options?: AgentTurnOptions) => Promise<void>;

  // Track iterate() and respondApproval() calls.
  iterateCalls: string[] = [];
  respondApprovalCalls: Array<{ requestId: string; decision: string }> = [];
  respondInputCalls: Array<{ requestId: string; response: AgentInputResponse }> = [];

  spawn(prompt: string, cwd: string, _env?: Record<string, string>, options?: AgentTurnOptions): void | Promise<void> {
    this.spawned = true;
    this.spawnPrompt = prompt;
    this.spawnCwd = cwd;
    this.spawnOptions = options;
  }

  sendMessage(_text: string): void {}

  cancel(): void | Promise<void> {
    this.cancelled = true;
  }

  iterate(prompt: string): void {
    this.iterateCalls.push(prompt);
  }

  respondApproval(requestId: string, decision: string): boolean {
    this.respondApprovalCalls.push({ requestId, decision });
    return true;
  }

  respondInput(requestId: string, response: AgentInputResponse): boolean {
    this.respondInputCalls.push({ requestId, response });
    return true;
  }

  onEvent(listener: (event: any) => void): () => void {
    this.eventListeners.add(listener);
    return () => { this.eventListeners.delete(listener); };
  }

  onExit(listener: (code: number) => void): () => void {
    this.exitListeners.add(listener);
    return () => { this.exitListeners.delete(listener); };
  }

  // Test helpers to simulate agent behaviour.
  simulateEvent(event: any): void {
    for (const listener of this.eventListeners) {
      listener(event);
    }
  }

  simulateExit(code: number): void {
    for (const listener of this.exitListeners) {
      listener(code);
    }
  }
}

vi.mock("../electron/orchestrator-adapters", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../electron/orchestrator-adapters")>();
  return {
    ...actual,
    createAdapter: vi.fn(() => {
      const adapter = new MockAdapter();
      mockAdapterInstances.push(adapter);
      return adapter;
    }),
  };
});

// Mock child_process + util so that promisify(execFile) returns our async mock.
vi.mock("node:child_process", () => ({
  execFile: vi.fn(),
}));

vi.mock("node:util", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:util")>();
  return {
    ...actual,
    promisify: vi.fn(() => {
      return async (...args: any[]) => {
        const command = args[0] as string;
        const cmdArgs = args[1] as string[];
        const options = args[2] as { cwd?: string } | undefined;
        execFileCalls.push({ command, args: cmdArgs, cwd: options?.cwd });
        if (cmdArgs[0] === "write-tree" || cmdArgs[0] === "commit-tree") return { stdout: "a".repeat(40) + "\n", stderr: "" };
        const revision = cmdArgs.at(-1)?.replace(/\^\{commit\}$/, "");
        if (cmdArgs[0] === "rev-parse" && revision && /^[a-f0-9]{40}$/.test(revision)) return { stdout: revision + "\n", stderr: "" };
        if (cmdArgs[0] === "update-ref") {
          if (cmdArgs[1] === "-d") checkpointRefs.delete(cmdArgs[2]);
          else checkpointRefs.add(cmdArgs[1]);
        }
        if (cmdArgs[0] === "for-each-ref" && cmdArgs[1] === "--format=%(refname)") {
          return { stdout: checkpointRefs.has(cmdArgs[2]) ? `${cmdArgs[2]}\n` : "", stderr: "" };
        }
        if (failPostTurnGitAdd && cmdArgs[0] === "add" && cmdArgs.includes("-A")) throw new Error("Archive snapshot failed");
        if (heldPostTurnGitAdd && cmdArgs[0] === "add" && cmdArgs.includes("-A")) return heldPostTurnGitAdd;
        if (hangPostTurnGitAdd && cmdArgs.includes("add") && cmdArgs.includes("-A")) {
          return new Promise(() => {});
        }
        if (cmdArgs[0] === "checkout" && cmdArgs[1]) {
          mockCurrentBranch = cmdArgs[1];
          return { stdout: "", stderr: "" };
        }
        if (cmdArgs.includes("rev-parse") && cmdArgs.includes("--abbrev-ref")) {
          return { stdout: `${mockCurrentBranch}\n`, stderr: "" };
        }
        if (cmdArgs.includes("rev-parse") && cmdArgs.includes("HEAD")) {
          return { stdout: "abc123def456789\n", stderr: "" };
        }
        if (cmdArgs.includes("rev-parse")) {
          return { stdout: "abc123def456789\n", stderr: "" };
        }
        return { stdout: "", stderr: "" };
      };
    }),
  };
});

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createTempPersistPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-orch-test-"));
  return path.join(dir, "agent-tasks.sqlite");
}

function cleanupPath(filePath: string): void {
  try {
    const dir = path.dirname(filePath);
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup.
  }
}

function markOwned(task: Task): void {
  vi.mocked(readTaskMetadata).mockImplementation(worktreePath => worktreePath === task.worktreePath ? {
    name: "Fixture", taskId: task.id, createdAt: new Date().toISOString(), status: "active", baseRef: task.baseRef, originalBranch: task.branch,
  } : undefined);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Orchestrator", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    vi.mocked(readTaskMetadata).mockImplementation(() => undefined);
    mockAdapterInstances = [];
    hangPostTurnGitAdd = false;
    failPostTurnGitAdd = false;
    heldPostTurnGitAdd = undefined;
    execFileCalls = [];
    mockCurrentBranch = "main";
    checkpointRefs = new Set();
    persistPath = createTempPersistPath();
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(path.dirname(persistPath), "settings.json"));
    orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000 });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllEnvs();
    cleanupPath(persistPath);
  });

  describe("hunk workspace owner", () => {
    it("awaits both native owners before reverting a shared file", async () => {
      const workspace = path.dirname(persistPath);
      const parent = await orchestrator.createTask("Parent", "codex", workspace, "main", { worktreeMode: "primary" });
      const peer = await orchestrator.createTask("Sibling", "claude-code", workspace, "main", { worktreeMode: "primary" });
      await orchestrator.startTask(parent.id);
      await orchestrator.startTask(peer.id);
      const filePath = path.join(workspace, "source.txt");
      fs.writeFileSync(filePath, "after");
      let closed!: () => void;
      const closing = new Promise<void>(resolve => { closed = resolve; });
      const first = mockAdapterInstances[0];
      vi.spyOn(first, "cancel").mockImplementation(async () => { first.cancelled = true; await closing; });
      const comparison = vi.spyOn(diff, "computeDiff").mockResolvedValue({ taskId: parent.id, baseRef: "main", headRef: "HEAD", rawDiff: "", files: [{ path: "source.txt", status: "modified", additions: 1, deletions: 1, patch: "" }], totalAdditions: 1, totalDeletions: 1 });
      const revert = vi.spyOn(CheckpointManager.prototype, "revertFile").mockImplementation(async () => { fs.writeFileSync(filePath, "before"); });
      const editing = orchestrator.revertFile(peer.id, "source.txt");
      try {
        await vi.waitFor(() => expect(mockAdapterInstances.map(adapter => adapter.cancelled)).toEqual([true, true]));
        expect({ bytes: fs.readFileSync(filePath, "utf8"), writes: revert.mock.calls.length }).toEqual({ bytes: "after", writes: 0 });
        await expect(orchestrator.iterate(parent.id, "Must wait")).rejects.toThrow("being updated");
      } finally {
        closed();
        try { await editing; } finally { comparison.mockRestore(); revert.mockRestore(); }
      }
      expect({ bytes: fs.readFileSync(filePath, "utf8"), statuses: [parent.status, peer.status] }).toEqual({ bytes: "before", statuses: ["stopped", "stopped"] });
    });

    it("drains an accepted snapshot preflight before acknowledging quit", async () => {
      const task = await orchestrator.createTask("Restore", "codex", "/tmp/repo");
      markOwned(task);
      let release!: () => void;
      let entered = false;
      const gate = new Promise<void>(resolve => { release = resolve; });
      const resolve = vi.spyOn(CheckpointManager.prototype, "snapshotSha").mockImplementation(async () => {
        entered = true;
        await gate;
        return "a".repeat(40);
      });
      const restoring = orchestrator.restoreFiles(task.id, `refs/crab/checkpoints/${task.id}/0`);
      await vi.waitFor(() => expect(entered).toBe(true));
      let closed = false;
      const shutdown = orchestrator.shutdown().then(() => { closed = true; });
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(closed).toBe(false);
      } finally { release(); await Promise.all([restoring, shutdown]); resolve.mockRestore(); }
    });

    it("validates snapshot ownership before stopping a native owner", async () => {
      const task = await orchestrator.createTask("Keep working", "codex", "/tmp/repo");
      markOwned(task);
      await orchestrator.startTask(task.id);
      await expect(orchestrator.restoreFiles(task.id, "refs/crab/checkpoints/another-task/0")).rejects.toThrow("does not belong");
      expect(mockAdapterInstances[0].cancelled).toBe(false);
      await orchestrator.cancelTask(task.id);
    });

    it.each(["primary", "existing"] as const)("rejects full restoration of an unowned %s folder", async worktreeMode => {
      const task = await orchestrator.createTask("User files", "codex", path.dirname(persistPath), "main", { worktreeMode, worktreePath: path.dirname(persistPath) });
      await expect(orchestrator.restoreFiles(task.id, "HEAD")).rejects.toThrow("Crab-owned worktree");
    });

    it.each([undefined, "another-task"])("rejects replaced workspace ownership %s before cancelling a provider", async taskId => {
      const task = await orchestrator.createTask("Keep working", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      markOwned({ ...task, id: taskId ?? task.id });
      if (!taskId) vi.mocked(readTaskMetadata).mockReturnValue(undefined);
      const resolve = vi.spyOn(CheckpointManager.prototype, "snapshotSha").mockResolvedValue("a".repeat(40));
      try {
        await expect(orchestrator.restoreFiles(task.id, `refs/crab/checkpoints/${task.id}/0`)).rejects.toThrow("no longer belongs");
        expect(mockAdapterInstances[0].cancelled).toBe(false);
      } finally { resolve.mockRestore(); await orchestrator.cancelTask(task.id); }
    });

    it("rejects a borrowed restore after its owner was discarded even if the directory remains", async () => {
      const parent = await orchestrator.createTask("Parent", "codex", "/tmp/repo");
      markOwned(parent);
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child");
      await orchestrator.discardTask(parent.id);
      await expect(orchestrator.restoreFiles(child.id, `refs/crab/checkpoints/${child.id}/0`)).rejects.toThrow("Crab-owned worktree");
    });

    it("rechecks workspace ownership after native cancellation settles", async () => {
      const task = await orchestrator.createTask("Restore", "codex", "/tmp/repo");
      markOwned(task);
      await orchestrator.startTask(task.id);
      vi.spyOn(mockAdapterInstances[0], "cancel").mockImplementation(() => { markOwned({ ...task, id: "replacement-owner" }); });
      const resolve = vi.spyOn(CheckpointManager.prototype, "snapshotSha").mockResolvedValue("a".repeat(40));
      const restore = vi.spyOn(CheckpointManager.prototype, "restoreFiles");
      try {
        await expect(orchestrator.restoreFiles(task.id, `refs/crab/checkpoints/${task.id}/0`)).rejects.toThrow("no longer belongs");
        expect(restore).not.toHaveBeenCalled();
      } finally { resolve.mockRestore(); restore.mockRestore(); }
    });

    it("waits for the preceding native turn's Git writer before editing", async () => {
      const workspace = path.dirname(persistPath);
      const task = await orchestrator.createTask("Edit", "codex", workspace, "main", { worktreeMode: "primary" });
      await orchestrator.startTask(task.id);
      const filePath = path.join(workspace, "source.txt");
      fs.writeFileSync(filePath, "before");
      let release!: (result: { stdout: string; stderr: string }) => void;
      heldPostTurnGitAdd = new Promise(resolve => { release = resolve; });
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: Date.now(), outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const edit = orchestrator.editHunk(task.id, { filePath, oldString: "before", newString: "after" }, "apply");
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(fs.readFileSync(filePath, "utf8")).toBe("before");
      } finally { heldPostTurnGitAdd = undefined; release({ stdout: "", stderr: "" }); await edit; }
      expect(fs.readFileSync(filePath, "utf8")).toBe("after");
    });

    it.each(["child", "sibling"])("awaits both native owners before a %s edits shared files", async relationship => {
      const workspace = path.dirname(persistPath);
      const parent = await orchestrator.createTask("Parent", "codex", workspace, "main", { worktreeMode: "primary" });
      const peer = relationship === "child"
        ? orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child")
        : await orchestrator.createTask("Sibling", "claude-code", workspace, "main", { worktreeMode: "primary" });
      await orchestrator.startTask(parent.id);
      await orchestrator.startTask(peer.id);
      const filePath = path.join(workspace, "source.txt");
      fs.writeFileSync(filePath, "before");
      let closed!: () => void;
      const closing = new Promise<void>(resolve => { closed = resolve; });
      const first = mockAdapterInstances[0];
      const changed = vi.fn();
      orchestrator.on("orchestrator:files-changed", changed);
      vi.spyOn(first, "cancel").mockImplementation(async () => { first.cancelled = true; await closing; });
      const edit = orchestrator.editHunk(peer.id, { filePath, oldString: "before", newString: "after" }, "apply");
      try {
        await vi.waitFor(() => expect(mockAdapterInstances.map(adapter => adapter.cancelled)).toEqual([true, true]));
        expect(fs.readFileSync(filePath, "utf8")).toBe("before");
        await expect(orchestrator.iterate(parent.id, "Must wait")).rejects.toThrow("being updated");
      } finally { closed(); await edit; }
      expect({ contents: fs.readFileSync(filePath, "utf8"), statuses: [parent.status, peer.status] }).toEqual({ contents: "after", statuses: ["stopped", "stopped"] });
      expect(changed.mock.calls).toEqual([[{ taskIds: [parent.id, peer.id] }]]);
    });

    it("preserves bytes when workspace closure fails", async () => {
      const workspace = path.dirname(persistPath);
      const task = await orchestrator.createTask("Edit", "codex", workspace, "main", { worktreeMode: "primary" });
      const filePath = path.join(workspace, "source.txt");
      fs.writeFileSync(filePath, "before");
      const close = vi.spyOn(WorkspaceScripts.prototype, "close").mockRejectedValueOnce(new Error("Writer still alive"));
      try {
        await expect(orchestrator.editHunk(task.id, { filePath, oldString: "before", newString: "after" }, "apply")).rejects.toThrow("Writer still alive");
        expect(fs.readFileSync(filePath, "utf8")).toBe("before");
      } finally { close.mockRestore(); }
    });
  });

  describe("createTask", () => {
    it("creates a task with correct initial state", async () => {
      const task = await orchestrator.createTask(
        "Fix the bug",
        "claude-code",
        "/tmp/repo",
        "main",
      );

      expect(task.id).toBeDefined();
      expect(task.prompt).toBe("Fix the bug");
      expect(task.agentType).toBe("claude-code");
      expect(task.status).toBe("created");
      expect(task.branch).toBe(`agent/${task.id}`);
      expect(task.worktreePath).toBe(path.join(os.tmpdir(), `crab-agent-${task.id}`));
      expect(task.baseRef).toBe("abc123def456789");
      expect(task.repoPath).toBe("/tmp/repo");
      expect(task.createdAt).toBeGreaterThan(0);
      expect(task.events).toEqual([]);
      expect(task.feedback).toEqual([]);
    });

    it("uses HEAD when no baseRef is provided", async () => {
      const task = await orchestrator.createTask(
        "Add feature",
        "codex",
        "/tmp/repo",
      );

      expect(task.baseRef).toBe("abc123def456789");
    });

    it("persists the task to disk", async () => {
      const task = await orchestrator.createTask("Test task", "claude-code", "/tmp/repo", "main");

      expect(fs.existsSync(persistPath)).toBe(true);
      expect(fs.existsSync(persistPath.replace(/\.sqlite$/, ".json"))).toBe(false);

      const orchestrator2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const persisted = orchestrator2.getTask(task.id);
      expect(persisted?.prompt).toBe("Test task");
    });

    it("emits task-status event on creation", async () => {
      const events: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => events.push(e));

      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ taskId: task.id, status: "created" });
    });

    it("can target the primary workspace without creating a worktree", async () => {
      const { createWorktree } = await import("../electron/orchestrator-worktree");
      const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-primary-repo-"));
      try {
        const task = await orchestrator.createTask(
          "Work in place",
          "codex",
          repoDir,
          "feature/local-work",
          { worktreeMode: "primary" },
        );

        expect(createWorktree).not.toHaveBeenCalled();
        expect(task.worktreeMode).toBe("primary");
        expect(task.worktreePath).toBe(repoDir);
        expect(task.branch).toBe("feature/local-work");
        expect(execFileCalls).toContainEqual({
          command: "git",
          args: ["checkout", "feature/local-work"],
          cwd: repoDir,
        });

        await orchestrator.startTask(task.id);
        expect(mockAdapterInstances[0].spawnCwd).toBe(repoDir);
      } finally {
        fs.rmSync(repoDir, { recursive: true, force: true });
      }
    });
  });

  describe("workspace setup", () => {
    it.each([false, true])("keeps child metadata consistent when parent deletion persistence fails: %s", async fail => {
      const parent = await orchestrator.createTask("Parent", "codex", "/tmp/repo");
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child");
      await orchestrator.discardTask(parent.id);
      const save = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks");
      try {
        if (fail) {
          save.mockImplementationOnce(() => { throw new Error("Deletion denied"); });
          await expect(orchestrator.removeTask(parent.id)).rejects.toThrow("Deletion denied");
        } else await orchestrator.removeTask(parent.id);
        const reloaded = new Orchestrator({ persistPath });
        expect({ parent: Boolean(orchestrator.getTask(parent.id)), savedParent: Boolean(reloaded.getTask(parent.id)),
          childParent: child.parentTaskId, savedChildParent: reloaded.getTask(child.id)?.parentTaskId }).toEqual({
          parent: fail, savedParent: fail, childParent: fail ? parent.id : undefined, savedChildParent: fail ? parent.id : undefined,
        });
      } finally { save.mockRestore(); }
    });

    it("times out the preceding native turn while its queued follow-up is checkpointing", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      let release!: (sha: string) => void;
      const snapshot = vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockReturnValueOnce(new Promise(resolve => { release = resolve; }));
      const input = orchestrator.iterate(task.id, "Never sent").catch(error => String(error));
      try {
        await vi.advanceTimersByTimeAsync(5001);
        expect({ status: task.status, cancelled: mockAdapterInstances[0].cancelled, sent: mockAdapterInstances[0].iterateCalls }).toEqual({ status: "failed", cancelled: true, sent: [] });
      } finally { release?.("a".repeat(40)); await input; snapshot.mockRestore(); }
    });

    it("keeps a previous native turn's slot after a queued follow-up cannot be checkpointed", async () => {
      const owner = new Orchestrator({ persistPath, maxConcurrent: 1 });
      const task = await owner.createTask("Initial", "codex", "/tmp/repo");
      await owner.startTask(task.id);
      const snapshot = vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockRejectedValueOnce(new Error("Capture denied"));
      const other = await owner.createTask("Other", "claude-code", "/tmp/repo");
      let admitted = false;
      let next: Promise<void> | undefined;
      try {
        await expect(owner.iterate(task.id, "Latest input")).rejects.toThrow("Capture denied");
        next = owner.startTask(other.id).then(() => { admitted = true; });
        await vi.advanceTimersByTimeAsync(0);
        expect({ status: task.status, pending: task.pendingInput?.phase, admitted, sent: mockAdapterInstances[0].iterateCalls }).toEqual({ status: "running", pending: "unsent", admitted: false, sent: [] });
        mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
        await next;
        expect({ status: task.status, outcome: task.events.find(event => event.type === "done")?.outcome, admitted }).toEqual({ status: "failed", outcome: "completed", admitted: true });
      } finally { snapshot.mockRestore(); await owner.shutdown(); await next?.catch(() => {}); }
    });

    it("retains an unsent input when the preceding native turn completes during failed capture", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      let reject!: (error: Error) => void;
      const snapshot = vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockReturnValueOnce(new Promise((_resolve, fail) => { reject = fail; }));
      const input = orchestrator.iterate(task.id, "Latest input").catch(error => String(error));
      try {
        await vi.advanceTimersByTimeAsync(0);
        mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
        reject(new Error("Capture denied"));
        await input;
        expect({ status: task.status, pending: task.pendingInput?.phase, sent: mockAdapterInstances[0].iterateCalls }).toEqual({ status: "failed", pending: "unsent", sent: [] });
      } finally { reject?.(new Error("Capture denied")); await input; snapshot.mockRestore(); }
    });

    it("rejects primary task creation when its initial user-workspace baseline cannot be captured", async () => {
      const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "crab-primary-baseline-"));
      const snapshot = vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockRejectedValueOnce(new Error("Baseline denied"));
      try {
        await expect(orchestrator.createTask("Initial", "codex", workspace, "main", { worktreeMode: "primary" })).rejects.toThrow("Baseline denied");
        expect({ tasks: orchestrator.getAllTasks().length, exists: fs.existsSync(workspace) }).toEqual({ tasks: 0, exists: true });
      } finally { snapshot.mockRestore(); fs.rmSync(workspace, { recursive: true, force: true }); }
    });

    it("shows automatic Git commit failure without changing native completion into failure", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      const errors: string[] = [];
      orchestrator.on("orchestrator:error", (payload: { message: string }) => errors.push(payload.message));
      failPostTurnGitAdd = true;
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      expect({ status: task.status, outcome: task.events.at(-1)?.outcome, errors }).toEqual({ status: "reviewing", outcome: "completed", errors: ["Auto-commit after turn failed: Archive snapshot failed"] });
    });

    it.each(["codex", "claude-code"])("blocks %s's first native dispatch when its checkpoint cannot be captured", async agent => {
      const task = await orchestrator.createTask("Initial", agent, "/tmp/repo");
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "createLightweightCheckpoint").mockRejectedValueOnce(new Error("Checkpoint capture denied"));
      try {
        await expect(orchestrator.startTask(task.id)).rejects.toThrow("Checkpoint capture denied");
        expect({ status: task.status, spawned: mockAdapterInstances.filter(adapter => adapter.spawned).length }).toEqual({ status: "failed", spawned: 0 });
      } finally { checkpoint.mockRestore(); }
    });

    it.each(["capture", "pin"] as const)("retains the exact unsent follow-up for Retry after checkpoint %s fails", async phase => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const checkpoint = phase === "capture"
        ? vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockRejectedValueOnce(new Error("Checkpoint preparation denied"))
        : vi.spyOn(CheckpointManager.prototype, "holdCheckpoint").mockRejectedValueOnce(new Error("Checkpoint preparation denied"));
      try {
        await expect(orchestrator.iterate(task.id, "Latest input")).rejects.toThrow("Checkpoint preparation denied");
        expect({ status: task.status, sent: mockAdapterInstances[0].iterateCalls, pending: task.pendingInput?.phase, counter: task.turnNumber }).toEqual({ status: "failed", sent: [], pending: "unsent", counter: undefined });
        const reloaded = new Orchestrator({ persistPath });
        await reloaded.retryTask(task.id);
        expect({ prompt: mockAdapterInstances.at(-1)?.spawnPrompt, turn: reloaded.getTask(task.id)?.turnNumber }).toEqual({ prompt: "Latest input", turn: 1 });
        await reloaded.cancelTask(task.id);
      } finally { checkpoint.mockRestore(); }
    });

    it("shares one preparation job before either parent or child can spawn", async () => {
      const parent = await orchestrator.createTask("Parent", "codex", "/tmp/repo");
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child");
      let release!: (result: { success: boolean; output: string; exitCode: number }) => void;
      const setup = vi.spyOn(WorkspaceScripts.prototype, "setup").mockReturnValueOnce(new Promise(resolve => { release = resolve; }));
      const starts = [orchestrator.startTask(parent.id), orchestrator.startTask(child.id)];
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect({ preparations: setup.mock.calls.length, spawned: mockAdapterInstances.filter(adapter => adapter.spawned).length }).toEqual({ preparations: 1, spawned: 0 });
        release({ success: true, output: "ready", exitCode: 0 });
        await Promise.all(starts);
        await orchestrator.iterate(child.id, "Next");
        expect({ preparations: setup.mock.calls.length, spawned: mockAdapterInstances.filter(adapter => adapter.spawned).length }).toEqual({ preparations: 1, spawned: 2 });
      } finally { release?.({ success: true, output: "", exitCode: 0 }); await Promise.allSettled(starts); setup.mockRestore(); }
    });

    it("blocks native startup after setup failure and reruns preparation only on Retry", async () => {
      const task = await orchestrator.createTask("Initial", "claude-code", "/tmp/repo");
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "recordCheckpoint");
      const setup = vi.spyOn(WorkspaceScripts.prototype, "setup")
        .mockResolvedValueOnce({ success: false, output: "setup failed", exitCode: 17, error: "Script exited with code 17" })
        .mockResolvedValue({ success: true, output: "ready", exitCode: 0 });
      try {
        await expect(orchestrator.startTask(task.id)).rejects.toThrow("Workspace setup failed");
        expect({ status: task.status, spawned: mockAdapterInstances.filter(adapter => adapter.spawned).length }).toEqual({ status: "failed", spawned: 0 });
        await orchestrator.retryTask(task.id);
        expect({ setups: setup.mock.calls.length, status: task.status, spawned: mockAdapterInstances.filter(adapter => adapter.spawned).length,
          checkpoints: checkpoint.mock.calls.length }).toEqual({ setups: 2, status: "running", spawned: 1, checkpoints: 1 });
      } finally { setup.mockRestore(); checkpoint.mockRestore(); }
    });

    it("does not rerun successful setup when a saved conversation resumes cold", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo");
      const setup = vi.spyOn(WorkspaceScripts.prototype, "setup").mockResolvedValue({ success: true, output: "ready", exitCode: 0 });
      try {
        await orchestrator.startTask(task.id);
        await orchestrator.cancelTask(task.id);
        const reloaded = new Orchestrator({ persistPath });
        await reloaded.retryTask(task.id);
        expect(setup).toHaveBeenCalledOnce();
        await reloaded.cancelTask(task.id);
      } finally { setup.mockRestore(); }
    });

    it("requires explicit Retry after setup was interrupted across a restart", async () => {
      const task = await orchestrator.createTask("Initial", "claude-code", "/tmp/repo");
      task.status = "running";
      task.workspaceSetup = "running";
      await orchestrator.persistTasks();
      const reloaded = new Orchestrator({ persistPath });
      const setup = vi.spyOn(WorkspaceScripts.prototype, "setup").mockResolvedValue({ success: true, output: "ready", exitCode: 0 });
      try {
        expect(reloaded.getTask(task.id)?.status).toBe("interrupted");
        await expect(reloaded.iterate(task.id, "Resume")).rejects.toThrow("Use Retry");
        expect({ preparations: setup.mock.calls.length, providers: mockAdapterInstances.length }).toEqual({ preparations: 0, providers: 0 });
        await reloaded.retryTask(task.id);
        expect({ preparations: setup.mock.calls.length, providers: mockAdapterInstances.length }).toEqual({ preparations: 1, providers: 1 });
      } finally { setup.mockRestore(); await reloaded.cancelTask(task.id); }
    });

    it("exposes Retry when preparation fails before a restored conversation's follow-up", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      await orchestrator.cancelTask(task.id);
      task.status = "reviewing";
      task.workspaceSetup = "pending";
      const setup = vi.spyOn(WorkspaceScripts.prototype, "setup")
        .mockResolvedValueOnce({ success: false, output: "failed", exitCode: 17 })
        .mockResolvedValue({ success: true, output: "ready", exitCode: 0 });
      try {
        await expect(orchestrator.iterate(task.id, "Latest input")).rejects.toThrow("Workspace setup failed");
        expect(task.status).toBe("failed");
        const reloaded = new Orchestrator({ persistPath });
        await reloaded.retryTask(task.id);
        expect({ prompt: mockAdapterInstances.at(-1)?.spawnPrompt, setups: setup.mock.calls.length, turn: reloaded.getTask(task.id)?.turnNumber }).toEqual({ prompt: "Latest input", setups: 2, turn: 1 });
        await reloaded.cancelTask(task.id);
      } finally { setup.mockRestore(); }
    });
  });

  describe("startTask", () => {
    it("waits for the adapter's startup submission instead of resolving after process creation", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo", "main");
      const original = MockAdapter.prototype.spawn;
      let release: (() => void) | undefined;
      const spawn = vi.spyOn(MockAdapter.prototype, "spawn").mockImplementation(function (this: MockAdapter, ...args) {
        original.apply(this, args);
        return new Promise<void>(resolve => { release = resolve; });
      });
      const startup = orchestrator.startTask(task.id);
      let settled = false;
      void startup.then(() => { settled = true; }, () => {});
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect({ spawned: mockAdapterInstances[0].spawned, settled }).toEqual({ spawned: true, settled: false });
        release?.();
        await startup;
        expect(settled).toBe(true);
      } finally { release?.(); await startup; spawn.mockRestore(); }
    });

    it("reports a missing provider without running a different agent", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo", "main");
      task.agentType = "missing-provider";
      await expect(orchestrator.startTask(task.id)).rejects.toThrow("Unknown agent type");
      expect({ provider: task.agentType, status: task.status, processes: mockAdapterInstances.length, outcomes: task.events.filter(event => event.type === "done").map(event => event.outcome) }).toEqual({ provider: "missing-provider", status: "failed", processes: 0, outcomes: ["failed"] });
    });

    it("keeps queued startup pending until capacity is released", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const first = await orchestrator.createTask("First", "codex", "/tmp/repo", "main");
      const second = await orchestrator.createTask("Second", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(first.id);
      const startup = orchestrator.startTask(second.id);
      let settled = false;
      void startup.then(() => { settled = true; }, () => {});
      await vi.advanceTimersByTimeAsync(0);
      expect({ status: second.status, settled, processes: mockAdapterInstances.length }).toEqual({ status: "queued", settled: false, processes: 1 });
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await startup;
      expect({ status: second.status, prompt: mockAdapterInstances[1].spawnPrompt }).toEqual({ status: "running", prompt: "Second" });
    });

    it.each(["codex", "claude-code"] as const)("queues an idle %s follow-up behind running work without respawning its process", async agent => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const idle = await orchestrator.createTask("First", agent, "/tmp/repo", "main");
      await orchestrator.startTask(idle.id);
      const first = mockAdapterInstances[0];
      first.simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const blocker = await orchestrator.createTask("Blocker", agent, "/tmp/repo", "main");
      await orchestrator.startTask(blocker.id);
      const input = orchestrator.iterate(idle.id, "Follow-up");
      let accepted = false;
      void input.then(() => { accepted = true; });
      await vi.advanceTimersByTimeAsync(0);
      expect({ status: orchestrator.getTask(idle.id)?.status, accepted, sent: first.iterateCalls, feedback: idle.feedback }).toEqual({
        status: "queued", accepted: false, sent: [], feedback: [],
      });
      const later = await orchestrator.createTask("Later", agent, "/tmp/repo", "main");
      const startup = orchestrator.startTask(later.id);
      await vi.advanceTimersByTimeAsync(0);
      mockAdapterInstances[1].simulateEvent({ type: "done", timestamp: 2, outcome: "completed" });
      await input;
      expect({ status: idle.status, later: later.status, sent: first.iterateCalls, processes: mockAdapterInstances.length }).toEqual({
        status: "running", later: "queued", sent: ["Follow-up"], processes: 2,
      });
      first.simulateEvent({ type: "done", timestamp: 3, outcome: "completed" });
      await startup;
      expect(mockAdapterInstances.at(-1)?.spawnPrompt).toBe("Later");
    });

    it("cancels a queued follow-up before checkpointing or native dispatch", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const idle = await orchestrator.createTask("First", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(idle.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const blocker = await orchestrator.createTask("Blocker", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(blocker.id);
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "snapshotHead");
      const input = orchestrator.iterate(idle.id, "Cancelled follow-up");
      const cancelled = expect(input).rejects.toThrow("Message cancelled before submission");
      try {
        await vi.advanceTimersByTimeAsync(0);
        await orchestrator.cancelTask(idle.id);
        await cancelled;
        mockAdapterInstances[1].simulateEvent({ type: "done", timestamp: 2, outcome: "completed" });
        await vi.advanceTimersByTimeAsync(0);
        expect({ status: idle.status, feedback: idle.feedback, snapshots: checkpoint.mock.calls, sent: mockAdapterInstances[0].iterateCalls,
          processes: mockAdapterInstances.length }).toEqual({ status: "stopped", feedback: [], snapshots: [], sent: [], processes: 2 });
      } finally { checkpoint.mockRestore(); }
    });

    it("keeps its capacity reservation while a follow-up checkpoint is being captured", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const idle = await orchestrator.createTask("First", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(idle.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      let finish!: (sha: string) => void;
      const capture = vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockReturnValueOnce(new Promise(resolve => { finish = resolve; }));
      const input = orchestrator.iterate(idle.id, "Follow-up");
      const later = await orchestrator.createTask("Later", "codex", "/tmp/repo", "main");
      const startup = orchestrator.startTask(later.id);
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect({ status: orchestrator.getTask(idle.id)?.status, later: later.status, processes: mockAdapterInstances.length }).toEqual({
          status: "iterating", later: "queued", processes: 1,
        });
        finish("a".repeat(40));
        await input;
        mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 2, outcome: "completed" });
        await startup;
      } finally { finish("a".repeat(40)); await input.catch(() => {}); capture.mockRestore(); }
    });

    it("does not charge a workflow coordinator against provider capacity", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const root = await orchestrator.createWorkflowRootTask("/tmp/repo", "Workflow", "Coordinate", "definition");
      const node = await orchestrator.createTask("Node", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(node.id);
      expect({ root: root.status, node: node.status, processes: mockAdapterInstances.length }).toEqual({ root: "running", node: "running", processes: 1 });
    });

    it("archives a workflow coordinator without staging or removing the primary checkout", async () => {
      const repo = fs.mkdtempSync(path.join(os.tmpdir(), "crab-workflow-owner-"));
      const source = path.join(repo, "keep.txt");
      fs.writeFileSync(source, "User changes");
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      try {
        const root = await orchestrator.createWorkflowRootTask(repo, "Workflow", "Coordinate", "definition");
        execFileCalls = [];
        await orchestrator.archiveTask(root.id);
        expect({ mode: root.worktreeMode, status: root.status, source: fs.readFileSync(source, "utf8"), git: execFileCalls,
          removal: vi.mocked(removeWorktree).mock.calls }).toEqual({ mode: "primary", status: "archived", source: "User changes", git: [], removal: [] });
      } finally { fs.rmSync(repo, { recursive: true, force: true }); }
    });

    it("migrates a released coordinator's missing primary-workspace ownership", async () => {
      const root = await orchestrator.createWorkflowRootTask("/tmp/repo", "Workflow", "Coordinate", "definition");
      root.worktreeMode = undefined;
      await orchestrator.persistTasks();
      const reloaded = new Orchestrator({ persistPath });
      expect(reloaded.getTask(root.id)?.worktreeMode).toBe("primary");
    });

    it("does not charge restored local status without a provider against capacity", async () => {
      const interrupted = await orchestrator.createTask("Previous owner", "codex", "/tmp/repo", "main");
      interrupted.status = "running";
      await orchestrator.persistTasks();
      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const task = await reloaded.createTask("New work", "codex", "/tmp/repo", "main");
      await reloaded.startTask(task.id);
      expect({ status: task.status, processes: mockAdapterInstances.length }).toEqual({ status: "running", processes: 1 });
      await reloaded.cancelTask(task.id);
    });

    it("resumes a persisted undispatched start without an in-memory queue waiter", async () => {
      const queued = await orchestrator.createTask("Undispatched work", "codex", "/tmp/repo", "main");
      queued.status = "queued";
      await orchestrator.persistTasks();
      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const first = await reloaded.createTask("First", "codex", "/tmp/repo", "main");
      await reloaded.startTask(first.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      expect({ status: reloaded.getTask(queued.id)?.status, prompt: mockAdapterInstances[1]?.spawnPrompt }).toEqual({
        status: "running", prompt: "Undispatched work",
      });
      await reloaded.cancelTask(queued.id);
    });

    it("restores a queued follow-up as unsent and retries its exact input and options only on request", async () => {
      const task = await orchestrator.createTask("Original task", "codex", "/tmp/repo", "main");
      task.status = "queued";
      task.pendingInput = { id: "queued-send", content: "Unsent follow-up", kind: "message", phase: "queued", turnNumber: 1,
        options: { mode: "plan", model: "selected-model" } };
      await orchestrator.persistTasks();
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "chat.sqlite"));
      const original = new ChatJournal(db, id => orchestrator.getTask(id));
      original.submission({ taskId: task.id, id: "queued-send", content: "Unsent follow-up", state: "pending" });
      const reloaded = new Orchestrator({ persistPath, maxConcurrent: 1, timeoutMs: 5000 });
      const chat = new ChatJournal(db, id => reloaded.getTask(id));
      reloaded.on("orchestrator:submission", (submission: TaskSubmission) => chat.submission(submission));
      try {
        await reloaded.recoverTaskInput(task.id);
        const other = await reloaded.createTask("Other work", "codex", "/tmp/repo", "main");
        await reloaded.startTask(other.id);
        mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
        await vi.advanceTimersByTimeAsync(0);
        expect({ processes: mockAdapterInstances.map(adapter => adapter.spawnPrompt), status: reloaded.getTask(task.id)?.status,
          feedback: reloaded.getTask(task.id)?.feedback, notice: chat.snapshot(task.id).messages.find(message => message.id === "queued-send")?.deliveryError }).toEqual({
          processes: ["Other work"], status: "interrupted", feedback: [],
          notice: "Queued message was not sent before the connection closed. Use Retry to send it.",
        });
        await reloaded.retryTask(task.id);
        expect({ input: mockAdapterInstances.at(-1)?.spawnPrompt, options: mockAdapterInstances.at(-1)?.spawnOptions,
          messages: chat.snapshot(task.id).messages.filter(message => message.role === "user").map(message => message.content) }).toEqual({
          input: "Unsent follow-up", options: { model: "selected-model", reasoningLevel: undefined, mode: "plan" },
          messages: ["Original task", "Unsent follow-up"],
        });
      } finally { await reloaded.cancelTask(task.id); db.close(); }
    });

    it.each(["validation", "native"])("retains the unsent input after %s rejection of a new submission", async (stage) => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      task.status = "interrupted";
      task.pendingInput = { id: "unsent", content: "Saved input", kind: "message", phase: "unsent", turnNumber: 1 };
      await orchestrator.persistTasks();
      const capabilities = agentRegistry.getCapabilities("codex");
      const rejection = stage === "validation"
        ? vi.spyOn(agentRegistry, "getCapabilities").mockReturnValueOnce({ ...capabilities, supportsIteration: false })
        : vi.spyOn(mockAdapterInstances[0], "iterate").mockImplementationOnce(() => { throw new Error("Native rejection"); });
      try {
        await expect(orchestrator.iterate(task.id, "Rejected input")).rejects.toThrow(stage === "validation" ? "does not support iteration" : "Native rejection");
        expect(task.pendingInput?.content).toBe("Saved input");
      } finally { rejection.mockRestore(); }
    });

    it("releases follow-up capacity after native rejection", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const first = await orchestrator.createTask("First", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(first.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      let reject!: (error: Error) => void;
      vi.spyOn(mockAdapterInstances[0], "iterate").mockImplementation(() => new Promise<void>((_resolve, fail) => { reject = fail; }));
      const input = orchestrator.iterate(first.id, "Rejected input");
      const rejected = expect(input).rejects.toThrow("Native rejection");
      await vi.advanceTimersByTimeAsync(0);
      const next = await orchestrator.createTask("Next", "codex", "/tmp/repo", "main");
      const startup = orchestrator.startTask(next.id);
      await vi.advanceTimersByTimeAsync(0);
      reject(new Error("Native rejection"));
      await rejected;
      await startup;
      expect({ previous: first.status, next: next.status, feedback: first.feedback, processes: mockAdapterInstances.length }).toEqual({
        previous: "reviewing", next: "running", feedback: [], processes: 2,
      });
    });

    it("restores an idle conversation after queue persistence fails without leaving a waiter", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const idle = await orchestrator.createTask("Idle", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(idle.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const blocker = await orchestrator.createTask("Blocker", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(blocker.id);
      const save = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks").mockImplementationOnce(() => { throw new Error("Queue could not be saved"); });
      try {
        await expect(orchestrator.iterate(idle.id, "Unsent input")).rejects.toThrow("Queue could not be saved");
        mockAdapterInstances[1].simulateEvent({ type: "done", timestamp: 2, outcome: "completed" });
        await vi.advanceTimersByTimeAsync(0);
        expect({ status: idle.status, sent: mockAdapterInstances[0].iterateCalls, feedback: idle.feedback, processes: mockAdapterInstances.length }).toEqual({
          status: "reviewing", sent: [], feedback: [], processes: 2,
        });
      } finally { save.mockRestore(); }
    });

    it("waits for the retired connection to close before retrying", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const previous = mockAdapterInstances[0];
      previous.simulateEvent({ type: "done", timestamp: 1, outcome: "failed" });
      await vi.advanceTimersByTimeAsync(0);
      let close: (() => void) | undefined;
      vi.spyOn(previous, "cancel").mockImplementation(() => new Promise<void>(resolve => { close = resolve; }));
      const retry = orchestrator.retryTask(task.id);
      await vi.advanceTimersByTimeAsync(0);
      expect(mockAdapterInstances).toHaveLength(1);
      previous.simulateEvent({ type: "request_resolved", timestamp: 2, requestId: "old-request" });
      previous.simulateEvent({ type: "done", timestamp: 3, outcome: "interrupted" });
      previous.simulateExit(1);
      close?.();
      await retry;
      expect({ status: task.status, processes: mockAdapterInstances.length, resolved: task.events.some(event => event.requestId === "old-request"), interrupted: task.events.some(event => event.outcome === "interrupted") }).toEqual({ status: "running", processes: 2, resolved: true, interrupted: false });
    });

    it("spawns the adapter and updates status to running", async () => {
      const task = await orchestrator.createTask("Do work", "claude-code", "/tmp/repo", "main");

      await orchestrator.startTask(task.id);

      expect(task.status).toBe("running");
      expect(task.startedAt).toBeGreaterThan(0);
      expect(mockAdapterInstances).toHaveLength(1);
      expect(mockAdapterInstances[0].spawned).toBe(true);
      expect(mockAdapterInstances[0].spawnCwd).toBe(task.worktreePath);
    });

    it("throws if task does not exist", async () => {
      await expect(orchestrator.startTask("nonexistent")).rejects.toThrow(
        "Task not found: nonexistent",
      );
    });

    it("throws if task is not in a startable state", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      // Task is now "running" — cannot start again.
      await expect(orchestrator.startTask(task.id)).rejects.toThrow(
        'Cannot start task in status "running"',
      );
    });

    it("collects agent events into the task", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "text", timestamp: 1000, content: "Hello" });
      adapter.simulateEvent({ type: "tool_use", timestamp: 2000, tool: "Read" });

      expect(task.events).toHaveLength(2);
      expect(task.events[0].content).toBe("Hello");
      expect(task.events[1].tool).toBe("Read");
    });

    it("emits orchestrator:event for each agent event", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "text", timestamp: 1000, content: "Hi" });

      expect(events).toHaveLength(1);
      expect(events[0].taskId).toBe(task.id);
      expect(events[0].event.content).toBe("Hi");
    });
  });

  describe("child conversations", () => {
    it("migrates the owning task's control metadata before a borrowing conversation starts", async () => {
      const { migrateWorktreeMetadata } = await import("../electron/orchestrator-worktree");
      const parent = await orchestrator.createTask("Owner", "codex", "/tmp/repo", "main");
      const child = orchestrator.createChildTaskForAgent(parent.id, "codex", "Borrow the workspace");
      try {
        await orchestrator.startTask(child.id);
        expect(vi.mocked(migrateWorktreeMetadata).mock.calls).toEqual([[parent.id, parent.worktreePath]]);
      } finally { fs.rmSync(parent.worktreePath, { recursive: true, force: true }); }
    });

    it.each(["codex", "claude-code"])("routes %s child requests and completion independently of its parent", async agentType => {
      const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "crab-child-workspace-"));
      try {
        const parent = await orchestrator.createTask("Coordinate", agentType, workspace, "main", { worktreeMode: "primary" });
        parent.model = "selected-model";
        parent.reasoningLevel = "selected-effort";
        const child = orchestrator.createChildTaskForAgent(parent.id, agentType, "Review", "reviewer");
        await orchestrator.startTask(child.id);
        const adapter = mockAdapterInstances[0];
        adapter.simulateEvent({ type: "text", timestamp: 1, content: "child reply" });
        orchestrator.recordApproval(child.id, { requestId: "approval", decision: "allow_once", summary: "Owned fixture" });
        orchestrator.recordInput(child.id, "question", { kind: "questions", answers: { mode: ["Thorough"] } });
        adapter.simulateEvent({ type: "done", outcome: "completed", timestamp: 2 });
        await vi.advanceTimersByTimeAsync(0);

        expect({
          cwd: adapter.spawnCwd, options: adapter.spawnOptions, parentStatus: parent.status, parentEvents: parent.events,
          childStatus: child.status, events: child.events.map(event => event.type),
          approvals: adapter.respondApprovalCalls, inputs: adapter.respondInputCalls,
        }).toEqual({
          cwd: workspace, options: { model: "selected-model", reasoningLevel: "selected-effort" }, parentStatus: "created", parentEvents: [], childStatus: "reviewing", events: ["text", "done"],
          approvals: [{ requestId: "approval", decision: "allow_once" }],
          inputs: [{ requestId: "question", response: { kind: "questions", answers: { mode: ["Thorough"] } } }],
        });
        await orchestrator.cancelTask(child.id);
        expect(adapter.cancelled).toBe(true);
      } finally { fs.rmSync(workspace, { recursive: true, force: true }); }
    });

    it("discards a child without disposing its parent's workspace", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const parent = await orchestrator.createTask("Coordinate", "codex", "/tmp/repo", "main");
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Review");
      await orchestrator.discardTask(child.id);
      expect({ childStatus: child.status, childOwnership: child.worktreeMode, parentStatus: parent.status, removed: vi.mocked(removeWorktree).mock.calls }).toEqual({
        childStatus: "discarded", childOwnership: "existing", parentStatus: "created", removed: [],
      });
    });

    it("keeps applying shared changes on the parent conversation", async () => {
      const parent = await orchestrator.createTask("Coordinate", "codex", "/tmp/repo", "main");
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Review");
      await expect(orchestrator.applyTask(child.id)).rejects.toThrow("parent conversation");
    });

    it("rejects applying a workspace while a child is still running", async () => {
      const parent = await orchestrator.createTask("Coordinate", "codex", "/tmp/repo", "main");
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Review");
      child.status = "running";
      await expect(orchestrator.applyTask(parent.id)).rejects.toThrow("all conversations");
    });

    it("waits for child shutdown and retires queued siblings before disposing the owned workspace", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const parent = await orchestrator.createTask("Coordinate", "codex", "/tmp/repo", "main");
      fs.mkdirSync(parent.worktreePath, { recursive: true });
      try {
        const child = orchestrator.createChildTaskForAgent(parent.id, "codex", "Review");
        const queued = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Test");
        await orchestrator.startTask(child.id);
        const queuedStartup = orchestrator.startTask(queued.id);
        const cancelledStartup = expect(queuedStartup).rejects.toThrow("Task startup was cancelled");
        await vi.advanceTimersByTimeAsync(0);
        let closed: (() => void) | undefined;
        vi.spyOn(mockAdapterInstances[0], "cancel").mockImplementation(() => new Promise<void>(resolve => { closed = resolve; }));
        const discard = orchestrator.discardTask(parent.id);
        await vi.advanceTimersByTimeAsync(0);
        expect({ removed: vi.mocked(removeWorktree).mock.calls, queued: queued.status, processes: mockAdapterInstances.length }).toEqual({ removed: [], queued: "stopped", processes: 1 });
        expect(() => orchestrator.createChildTaskForAgent(parent.id, "codex", "Late child")).toThrow("being removed");
        await expect(orchestrator.startTask(queued.id)).rejects.toThrow("being removed");
        await expect(orchestrator.iterate(child.id, "Late follow-up")).rejects.toThrow("being removed");
        closed?.();
        await discard;
        await cancelledStartup;
        expect({ removed: vi.mocked(removeWorktree).mock.calls, child: child.status, parent: parent.status }).toEqual({
          removed: [[parent.repoPath, parent.id]], child: "stopped", parent: "discarded",
        });
      } finally { fs.rmSync(parent.worktreePath, { recursive: true, force: true }); }
    });
  });

  describe("agent exit handling", () => {
    it("transitions to reviewing on exit code 0", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      mockAdapterInstances[0].simulateExit(0);
      // Allow async handler to complete.
      await vi.advanceTimersByTimeAsync(0);

      expect(task.status).toBe("reviewing");
      expect(task.exitCode).toBe(0);
      expect(task.completedAt).toBeGreaterThan(0);
      expect(statusEvents.some((e) => e.status === "reviewing")).toBe(true);
    });

    it.each(["codex", "claude-code"])("transitions %s turns to reviewing on done without waiting for process exit", async agentType => {
      const task = await orchestrator.createTask("Test", agentType, "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "completed", timestamp: 1000, content: "done" });
      await vi.advanceTimersByTimeAsync(0);

      expect(task.status).toBe("reviewing");
      expect(task.exitCode).toBe(0);
      expect(task.completedAt).toBeGreaterThan(0);
      expect(statusEvents.some((e) => e.status === "reviewing")).toBe(true);
    });

    it.each(["codex", "claude-code"])("transitions %s turns to failed when done follows a fatal error", async agentType => {
      const task = await orchestrator.createTask("Test", agentType, "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      mockAdapterInstances[0].simulateEvent({ type: "error", timestamp: 1000, error: "turn failed" });
      mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "failed", timestamp: 1001, content: "" });
      await vi.advanceTimersByTimeAsync(0);

      expect(task.status).toBe("failed");
      expect(task.exitCode).toBe(1);
      expect(task.completedAt).toBeGreaterThan(0);
      expect(statusEvents.some((e) => e.status === "failed")).toBe(true);
    });

    it("transitions to failed on non-zero exit code", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      mockAdapterInstances[0].simulateExit(1);
      await vi.advanceTimersByTimeAsync(0);

      expect(task.status).toBe("failed");
      expect(task.exitCode).toBe(1);
    });

    it("transitions to reviewing even if post-turn git snapshot hangs", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      hangPostTurnGitAdd = true;

      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      expect(task.status).toBe("reviewing");
      expect(task.exitCode).toBe(0);
    });
  });

  describe("completion recovery", () => {
    it("delivers a committed terminal row even when clearing its task receipt fails", async () => {
      const task = await orchestrator.createTask("Owned acknowledgement failure", "codex", "/tmp/repo", "main");
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "ack-chat.sqlite"));
      const { registerChatIpc } = await import("../electron/orchestrator-chat");
      const delivered: Array<{ upsert?: Array<{ kind?: string }> }> = [];
      registerChatIpc({ handle() {} }, orchestrator, db, () => ({ isDestroyed: () => false, webContents: {
        send: (channel, payload) => { if (channel === "orchestrator:chat-update") delivered.push(payload as { upsert?: Array<{ kind?: string }> }); },
      } }));
      const replace = OrchestratorTaskDatabase.prototype.replaceTasks;
      let fail = true;
      const failure = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks").mockImplementation(function(this: OrchestratorTaskDatabase, tasks) {
        if (fail && tasks.some(candidate => candidate.id === task.id && candidate.status === "reviewing" && !candidate.pendingCompletions)) {
          fail = false;
          throw new Error("Owned receipt failure");
        }
        return replace.call(this, tasks);
      });
      try {
        await orchestrator.startTask(task.id);
        mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "completed", timestamp: 100 });
        expect(delivered.some(update => update.upsert?.some(message => message.kind === "done"))).toBe(true);
      } finally { failure.mockRestore(); await orchestrator.cancelTask(task.id); db.close(); }
    });

    it("journals a pending completion before a buffered later turn when storage recovers", async () => {
      const task = await orchestrator.createTask("Owned buffered turns", "claude-code", "/tmp/repo", "main");
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "ordered-chat.sqlite"));
      const { registerChatIpc } = await import("../electron/orchestrator-chat");
      const chat = registerChatIpc({ handle() {} }, orchestrator, db, () => null);
      const write = db.updateMessages.bind(db);
      let fail = true;
      const failure = vi.spyOn(db, "updateMessages").mockImplementation((id, messages, deleted) => {
        if (fail && messages.some(message => message.kind === "done")) { fail = false; throw new Error("Owned chat storage failure"); }
        write(id, messages, deleted);
      });
      try {
        await orchestrator.startTask(task.id);
        const adapter = mockAdapterInstances[0];
        adapter.simulateEvent({ type: "turn_started", timestamp: 1 });
        adapter.simulateEvent({ type: "done", outcome: "completed", timestamp: 2 });
        adapter.simulateEvent({ type: "turn_started", timestamp: 3 });
        expect(chat.snapshot(task.id).messages.filter(message => message.role === "agent").map(message => message.kind)).toEqual(["turn_started", "done", "turn_started"]);
      } finally { failure.mockRestore(); await orchestrator.cancelTask(task.id); db.close(); }
    });

    it.each([
      { outcome: "failed", plan: false, status: "failed" },
      { outcome: "interrupted", plan: false, status: "interrupted" },
      { outcome: "completed", plan: true, status: "planning" },
    ])("restores $outcome with planning=$plan without inventing a successful implementation", async ({ outcome, plan, status }) => {
      const task = await orchestrator.createTask("Owned outcome", "codex", "/tmp/repo", "main");
      task.planFirst = plan;
      await orchestrator.startTask(task.id);
      orchestrator.on("orchestrator:event", ({ event }) => { if (event.type === "done") throw new Error("Owned completion crash"); });
      expect(() => mockAdapterInstances[0].simulateEvent({ type: "done", outcome, timestamp: 100 })).toThrow("Owned completion crash");
      const restored = new Orchestrator({ persistPath });
      expect(restored.getTask(task.id)?.status).toBe(status);
    });

    it.each(["before-journal", "after-journal"])("replays an unapplied completion acknowledgement exactly once after a %s crash", async phase => {
      const task = await orchestrator.createTask("Complete owned work", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      orchestrator.on("orchestrator:event", ({ event }) => { if (event.type === "done") throw new Error("Owned completion crash"); });
      expect(() => mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "completed", timestamp: 100, turnId: "native-turn", content: "Final reply" })).toThrow("Owned completion crash");
      const completion = task.pendingCompletions?.[0];
      if (!completion) throw new Error("Native terminal record missing");
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "completion-chat.sqlite"));
      try {
        if (phase === "after-journal") new ChatJournal(db, () => task).event(task.id, completion.event, completion.id);
        const { registerChatIpc } = await import("../electron/orchestrator-chat");
        for (let restart = 0; restart < 2; restart++) {
          const restored = new Orchestrator({ persistPath });
          registerChatIpc({ handle() {} }, restored, db, () => null);
          expect({ status: restored.getTask(task.id)?.status, pending: restored.getTask(task.id)?.pendingCompletions,
            terminals: db.loadMessages(task.id).filter(message => message.kind === "done").map(message => ({ id: message.id, content: message.content, outcome: JSON.parse(message.metadata ?? "{}").outcome })) }).toEqual({
            status: "reviewing", pending: undefined, terminals: [{ id: completion.id, content: "Final reply", outcome: "completed" }],
          });
        }
      } finally { db.close(); }
    });

    it("preserves a terminal record when journal acknowledgement cannot be persisted", async () => {
      const task = await orchestrator.createTask("Complete owned work", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "failed", timestamp: 100 });
      const pending = task.pendingCompletions;
      const failure = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks").mockImplementation(() => { throw new Error("Owned task storage failure"); });
      try {
        expect(() => orchestrator.acknowledgeCompletion(task.id, pending![0].id)).toThrow("Owned task storage failure");
        expect(task.pendingCompletions).toBe(pending);
      } finally { failure.mockRestore(); }
    });

    it.each(["codex", "claude-code"])("persists %s completion before publishing the terminal event", async agent => {
      const task = await orchestrator.createTask("Complete owned work", agent, "/tmp/repo", "main");
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "completion-chat.sqlite"));
      const { registerChatIpc } = await import("../electron/orchestrator-chat");
      registerChatIpc({ handle() {} }, orchestrator, db, () => null);
      await orchestrator.startTask(task.id);
      let durableStatus: string | undefined;
      orchestrator.on("orchestrator:event", ({ event }) => {
        if (event.type !== "done") return;
        const saved = new OrchestratorTaskDatabase(persistPath);
        try { durableStatus = saved.loadTasks().find(candidate => candidate.id === task.id)?.status; }
        finally { saved.close(); }
        throw new Error("Owned completion crash");
      });
      try {
        expect(() => mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "completed", timestamp: Date.now() })).toThrow("Owned completion crash");
        const restored = new Orchestrator({ persistPath });
        expect({ statusAtPublish: durableStatus, restored: restored.getTask(task.id)?.status, terminalRows: db.loadMessages(task.id).filter(message => message.kind === "done").length }).toEqual({
          statusAtPublish: "reviewing", restored: "reviewing", terminalRows: 1,
        });
      } finally { db.close(); }
    });

  });

  describe("shutdown", () => {
    it("does not resurrect an undispatched job already stopped before quit", async () => {
      orchestrator = new Orchestrator({ persistPath, maxConcurrent: 1, timeoutMs: 5000 });
      const blocker = await orchestrator.createTask("Blocker", "codex", "/tmp/repo");
      await orchestrator.startTask(blocker.id);
      const task = await orchestrator.createTask("Stopped queue", "codex", "/tmp/repo");
      const startup = orchestrator.startTask(task.id);
      const rejected = expect(startup).rejects.toThrow("Task startup was cancelled");
      await vi.advanceTimersByTimeAsync(0);
      const stop = orchestrator.cancelTask(task.id);
      await Promise.all([stop, orchestrator.shutdown(), rejected]);
      expect({ status: task.status, persisted: new Orchestrator({ persistPath }).getTask(task.id)?.status, processes: mockAdapterInstances.length }).toEqual({ status: "stopped", persisted: "stopped", processes: 1 });
    });

    it("drains workspace creation accepted before the admission barrier", async () => {
      const { createWorktree } = await import("../electron/orchestrator-worktree");
      let release!: (worktree: string) => void;
      vi.mocked(createWorktree).mockImplementationOnce(() => new Promise(resolve => { release = resolve; }));
      const creation = orchestrator.createTask("Accepted", "codex", "/tmp/repo");
      await vi.advanceTimersByTimeAsync(0);
      let closed = false;
      const shutdown = orchestrator.shutdown().then(() => { closed = true; });
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(closed).toBe(false);
        release(path.join(path.dirname(persistPath), "accepted-worktree"));
        const task = await creation;
        await shutdown;
        expect({ closed, status: new Orchestrator({ persistPath }).getTask(task.id)?.status, processes: mockAdapterInstances.length }).toEqual({ closed: true, status: "created", processes: 0 });
      } finally { release(path.join(path.dirname(persistPath), "accepted-worktree")); await creation; await shutdown; }
    });

    it("waits for the completed turn's snapshot without changing its outcome", async () => {
      const task = await orchestrator.createTask("Completed", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      let release!: (result: { stdout: string; stderr: string }) => void;
      heldPostTurnGitAdd = new Promise(resolve => { release = resolve; });
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: Date.now(), outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      let closed = false;
      const shutdown = orchestrator.shutdown().then(() => { closed = true; });
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(closed).toBe(false);
        heldPostTurnGitAdd = undefined;
        release({ stdout: "", stderr: "" });
        await shutdown;
        expect({ closed, status: task.status }).toEqual({ closed: true, status: "reviewing" });
      } finally { heldPostTurnGitAdd = undefined; release({ stdout: "", stderr: "" }); await shutdown; }
    });

    it.each(["codex", "claude-code"] as const)("closes %s owners before resolving and preserves undispatched initial work", async agent => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const idle = await orchestrator.createTask("Completed", agent, "/tmp/repo");
      await orchestrator.startTask(idle.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: Date.now(), outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const active = await orchestrator.createTask("Active", agent, "/tmp/repo");
      await orchestrator.startTask(active.id);
      const queued = await orchestrator.createTask("Undispatched", agent, "/tmp/repo");
      const startup = orchestrator.startTask(queued.id);
      const rejected = expect(startup).rejects.toThrow("shutting down");
      await vi.advanceTimersByTimeAsync(0);
      const release: Array<() => void> = [];
      const closures = mockAdapterInstances.map(adapter => vi.spyOn(adapter, "cancel").mockImplementation(() => new Promise<void>(resolve => { release.push(resolve); })));
      let closed = false;
      const shutdown = orchestrator.shutdown().then(() => { closed = true; });
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect({ closed, statuses: [idle.status, active.status, queued.status], processes: mockAdapterInstances.length }).toEqual({
          closed: false, statuses: ["reviewing", "stopped", "queued"], processes: 2,
        });
        await expect(orchestrator.createTask("Too late", agent, "/tmp/repo")).rejects.toThrow("shutting down");
        await expect(orchestrator.iterate(idle.id, "Too late")).rejects.toThrow("shutting down");
        release.forEach(resolve => resolve());
        await Promise.all([shutdown, orchestrator.shutdown(), rejected]);
        const reloaded = new Orchestrator({ persistPath });
        expect({ closed, statuses: [idle.id, active.id, queued.id].map(id => reloaded.getTask(id)?.status), calls: closures.map(close => close.mock.calls.length), processes: mockAdapterInstances.length }).toEqual({
          closed: true, statuses: ["reviewing", "stopped", "queued"], calls: [1, 1], processes: 2,
        });
      } finally { release.forEach(resolve => resolve()); await shutdown; await startup.catch(() => {}); closures.forEach(close => close.mockRestore()); }
    });

    it("joins an existing Stop instead of releasing its owner twice", async () => {
      const task = await orchestrator.createTask("Active", "codex", "/tmp/repo");
      await orchestrator.startTask(task.id);
      let release!: () => void;
      const close = vi.spyOn(mockAdapterInstances[0], "cancel").mockReturnValue(new Promise<void>(resolve => { release = resolve; }));
      const stop = orchestrator.cancelTask(task.id);
      let closed = false;
      const shutdown = orchestrator.shutdown().then(() => { closed = true; });
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect({ closed, calls: close.mock.calls.length }).toEqual({ closed: false, calls: 1 });
        release();
        await Promise.all([stop, shutdown]);
      } finally { release(); await Promise.all([stop, shutdown]); close.mockRestore(); }
    });

    it("waits for a pending checkpoint without dispatching the cancelled input", async () => {
      const task = await orchestrator.createTask("Initial", "claude-code", "/tmp/repo");
      await orchestrator.startTask(task.id);
      let release!: (sha: string) => void;
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockReturnValueOnce(new Promise(resolve => { release = resolve; }));
      const input = orchestrator.iterate(task.id, "Never sent");
      const rejected = expect(input).rejects.toThrow("Message cancelled before submission");
      await vi.advanceTimersByTimeAsync(0);
      let closed = false;
      const shutdown = orchestrator.shutdown().then(() => { closed = true; });
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(closed).toBe(false);
        release("a".repeat(40));
        await Promise.all([shutdown, rejected]);
        expect({ closed, status: task.status, sent: mockAdapterInstances[0].iterateCalls, feedback: task.feedback }).toEqual({ closed: true, status: "stopped", sent: [], feedback: [] });
      } finally { release("a".repeat(40)); await shutdown; await input.catch(() => {}); checkpoint.mockRestore(); }
    });
  });

  describe("cancelTask", () => {
    it("sends SIGTERM and updates status to stopped", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      await orchestrator.cancelTask(task.id);

      expect(task.status).toBe("stopped");
      expect(task.completedAt).toBeGreaterThan(0);
      expect(mockAdapterInstances[0].cancelled).toBe(true);
    });

    it("throws if task does not exist", async () => {
      await expect(orchestrator.cancelTask("nonexistent")).rejects.toThrow(
        "Task not found: nonexistent",
      );
    });

    it("emits task-status event", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const events: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => events.push(e));

      await orchestrator.cancelTask(task.id);

      expect(events.some((e) => e.status === "stopped")).toBe(true);
    });
  });

  describe("timeout", () => {
    it("cancels a startup that is still waiting for its adapter", async () => {
      const task = await orchestrator.createTask("Initial", "codex", "/tmp/repo", "main");
      const original = MockAdapter.prototype.spawn;
      let rejectStartup: ((error: Error) => void) | undefined;
      const spawn = vi.spyOn(MockAdapter.prototype, "spawn").mockImplementation(function (this: MockAdapter, ...args) {
        original.apply(this, args);
        return new Promise<void>((_resolve, reject) => { rejectStartup = reject; });
      });
      const startup = orchestrator.startTask(task.id);
      const failure = expect(startup).rejects.toThrow("Startup cancelled");
      try {
        await vi.advanceTimersByTimeAsync(0);
        const adapter = mockAdapterInstances[0];
        vi.spyOn(adapter, "cancel").mockImplementation(() => { adapter.cancelled = true; rejectStartup?.(new Error("Startup cancelled")); });
        await vi.advanceTimersByTimeAsync(5000);
        await failure;
        expect({ status: task.status, cancelled: adapter.cancelled, processes: mockAdapterInstances.length }).toEqual({ status: "failed", cancelled: true, processes: 1 });
      } finally { rejectStartup?.(new Error("Startup cancelled")); await failure; spawn.mockRestore(); }
    });

    it("kills agent after configured duration", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const timeoutEvents: any[] = [];
      orchestrator.on("orchestrator:timeout", (e) => timeoutEvents.push(e));

      // Advance past the timeout (5000ms configured in beforeEach).
      await vi.advanceTimersByTimeAsync(5001);

      expect(task.status).toBe("failed");
      expect(mockAdapterInstances[0].cancelled).toBe(true);
      expect(timeoutEvents).toHaveLength(1);
      expect(timeoutEvents[0].taskId).toBe(task.id);
    });

    it("does not fire if task completes before timeout", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const timeoutEvents: any[] = [];
      orchestrator.on("orchestrator:timeout", (e) => timeoutEvents.push(e));

      // Agent exits before timeout.
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      // Advance past the timeout — should not fire.
      await vi.advanceTimersByTimeAsync(6000);

      expect(timeoutEvents).toHaveLength(0);
    });

    it("does not fire if task is cancelled before timeout", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const timeoutEvents: any[] = [];
      orchestrator.on("orchestrator:timeout", (e) => timeoutEvents.push(e));

      await orchestrator.cancelTask(task.id);
      await vi.advanceTimersByTimeAsync(6000);

      expect(timeoutEvents).toHaveLength(0);
    });
  });

  describe("persistence", () => {
    it.each(["codex", "claude-code"])("restores %s native model, effort, and mode after process reload", async agent => {
      const task = await orchestrator.createTask("Saved configuration", agent, "/tmp/repo", "main");
      try {
        task.model = "provider-selected-model";
        task.reasoningLevel = "provider-selected-effort";
        task.planFirst = true;
        await orchestrator.persistTasks();
        const saved = new Orchestrator({ persistPath }).getTask(task.id);
        expect({ model: saved?.model, effort: saved?.reasoningLevel, plan: saved?.planFirst }).toEqual({ model: "provider-selected-model", effort: "provider-selected-effort", plan: true });
      } finally { fs.rmSync(task.worktreePath, { recursive: true, force: true }); }
    });

    it("saves tasks without events array", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "text", timestamp: 1000, content: "Hi" });

      const orchestrator2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const persisted = orchestrator2.getTask(task.id);
      expect(persisted?.events).toEqual([]);
      expect(persisted?.prompt).toBe("Test");
    });

    it("loads tasks on construction", async () => {
      // Create a task and persist it.
      await orchestrator.createTask("Persisted task", "codex", "/tmp/repo", "main");

      // Create a new orchestrator instance pointing to the same file.
      const orchestrator2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const tasks = orchestrator2.getAllTasks();

      expect(tasks).toHaveLength(1);
      expect(tasks[0].prompt).toBe("Persisted task");
      expect(tasks[0].agentType).toBe("codex");
      expect(tasks[0].events).toEqual([]); // Events are not persisted.
    });

    it("persists on every status change", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");

      // After create.
      let reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(reloaded.getTask(task.id)?.status).toBe("created");

      // After start.
      await orchestrator.startTask(task.id);
      const saved = new OrchestratorTaskDatabase(persistPath);
      try { expect(saved.loadTasks().find(candidate => candidate.id === task.id)?.status).toBe("running"); }
      finally { saved.close(); }
      reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(reloaded.getTask(task.id)?.status).toBe("interrupted");

      // After cancel.
      await orchestrator.cancelTask(task.id);
      reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(reloaded.getTask(task.id)?.status).toBe("stopped");
    });

    it("persists sidebar pin, collection, and parent-child metadata", async () => {
      const parent = await orchestrator.createTask("Coordinate sidebar work", "claude-code", "/tmp/repo", "main");
      await orchestrator.setTaskPinned(parent.id, true);
      await orchestrator.setTaskSidebarGroup(parent.id, "Launch");
      const child = orchestrator.createChildTaskForAgent(parent.id, "codex", "Review sidebar hierarchy", "reviewer");
      await orchestrator.persistTasks();

      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const loadedParent = reloaded.getTask(parent.id);
      const loadedChild = reloaded.getTask(child.id);

      expect(loadedParent?.pinnedAt).toBeGreaterThan(0);
      expect(loadedParent?.sidebarGroup).toBe("Launch");
      expect(loadedParent?.childTaskIds).toContain(child.id);
      expect(loadedChild?.parentTaskId).toBe(parent.id);
    });

    it("ignores legacy JSON task files after the SQLite cutoff", () => {
      const legacyData = {
        tasks: [
          {
            id: "task-legacy-compat",
            prompt: "Legacy task",
            agentType: "codex",
            status: "reviewing",
            branch: "agent/task-legacy-compat",
            worktreePath: "/tmp/crab-agent-task-legacy-compat",
            baseRef: "abc123",
            repoPath: "/tmp/repo",
            createdAt: 1700000000000,
            feedback: [],
            todos: [],
          },
        ],
      };
      const legacyJsonPath = persistPath.replace(/\.sqlite$/, ".json");

      fs.mkdirSync(path.dirname(legacyJsonPath), { recursive: true });
      fs.writeFileSync(legacyJsonPath, JSON.stringify(legacyData), "utf-8");

      const orch = new Orchestrator({ persistPath: legacyJsonPath, timeoutMs: 5000 });
      const task = orch.getTask("task-legacy-compat");

      expect(task).toBeUndefined();
    });

    it("handles missing persistence file gracefully", () => {
      const missingPath = path.join(os.tmpdir(), "nonexistent-dir-xyz", "tasks.sqlite");
      // Should not throw.
      const orch = new Orchestrator({ persistPath: missingPath });
      expect(orch.getAllTasks()).toEqual([]);
    });

    it("ignores corrupted legacy JSON files", () => {
      const legacyJsonPath = persistPath.replace(/\.sqlite$/, ".json");
      fs.mkdirSync(path.dirname(legacyJsonPath), { recursive: true });
      fs.writeFileSync(legacyJsonPath, "not valid json{{{", "utf-8");

      // Should not throw.
      const orch = new Orchestrator({ persistPath: legacyJsonPath });
      expect(orch.getAllTasks()).toEqual([]);
    });
  });

  describe("review", () => {
    it.each(["codex", "claude-code"])("admits and journals %s reviews through the same input owner", async agent => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const task = await orchestrator.createTask("Original task", agent, "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const nativeReview = vi.fn(async (_instructions: string) => {});
      if (agent === "codex") adapter.review = nativeReview;
      const blocker = await orchestrator.createTask("Blocker", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(blocker.id);
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "chat.sqlite"));
      const chat = new ChatJournal(db, id => orchestrator.getTask(id));
      orchestrator.on("orchestrator:submission", (submission: TaskSubmission) => chat.submission(submission));
      try {
        const reviewing = orchestrator.startReview(task.id);
        await vi.advanceTimersByTimeAsync(0);
        expect({ status: task.status, kind: task.pendingInput?.kind, native: nativeReview.mock.calls.length, inputs: adapter.iterateCalls }).toEqual({
          status: "queued", kind: "review", native: 0, inputs: [],
        });
        mockAdapterInstances[1].simulateEvent({ type: "done", timestamp: 2, outcome: "completed" });
        await reviewing;
        const instruction = task.feedback[0];
        const saved = new OrchestratorTaskDatabase(persistPath);
        try {
          expect({ original: task.prompt, calls: agent === "codex" ? nativeReview.mock.calls[0]?.[0] : adapter.iterateCalls[0],
            kind: saved.loadTasks().find(candidate => candidate.id === task.id)?.lastInputKind,
            journal: chat.snapshot(task.id).messages.filter(message => message.role === "user").at(-1)?.content }).toEqual({
            original: "Original task", calls: instruction, kind: "review", journal: instruction,
          });
        } finally { saved.close(); }
        expect(instruction).toContain(JSON.stringify(task.baseRef));
        adapter.simulateEvent({ type: "done", timestamp: 3, outcome: "completed" });
        await vi.advanceTimersByTimeAsync(0);
        await orchestrator.iterate(task.id, "Continue implementation");
        expect(task.lastInputKind).toBe("message");
      } finally { db.close(); }
    });

    it("cancels a queued native review before dispatch", async () => {
      orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000, maxConcurrent: 1 });
      const blocker = await orchestrator.createTask("Blocker", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(blocker.id);
      const task = await orchestrator.createTask("Review target", "codex", "/tmp/repo", "main");
      task.status = "reviewing";
      const pending = orchestrator.startReview(task.id);
      const rejected = expect(pending).rejects.toThrow("Message cancelled before submission");
      await vi.advanceTimersByTimeAsync(0);
      await orchestrator.cancelTask(task.id);
      await rejected;
      expect({ processes: mockAdapterInstances.length, feedback: task.feedback, pending: task.pendingInput }).toEqual({ processes: 1, feedback: [], pending: undefined });
    });

    it("restores the accepted native review kind for explicit Retry after owner loss", async () => {
      const calls: string[] = [];
      MockAdapter.prototype.review = async function(instructions) { calls.push(instructions); };
      let reloaded: Orchestrator | undefined;
      try {
        const task = await orchestrator.createTask("Original task", "codex", "/tmp/repo", "main");
        task.status = "reviewing";
        await orchestrator.startReview(task.id);
        mockAdapterInstances[0].simulateExit(1);
        await vi.advanceTimersByTimeAsync(0);
        reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
        await reloaded.retryTask(task.id);
        expect({ calls, originalSpawns: mockAdapterInstances.filter(adapter => adapter.spawned).length }).toEqual({
          calls: [task.feedback[0], task.feedback[0]], originalSpawns: 0,
        });
        await reloaded.cancelTask(task.id);
      } finally { delete MockAdapter.prototype.review; }
    });
  });

  describe("iterate", () => {
    it.each(["archived", "applied", "discarded"] as const)("does not submit input or recreate a %s workspace", async status => {
      const task = await orchestrator.createTask("Closed work", "codex", "/tmp/repo", "main");
      task.status = status;
      try {
        await expect(orchestrator.iterate(task.id, "Unexpected work")).rejects.toThrow(status === "discarded" ? "read-only" : "Reopen");
        expect({ status: task.status, feedback: task.feedback, processes: mockAdapterInstances.length, turns: task.turnNumber }).toEqual({ status, feedback: [], processes: 0, turns: undefined });
      } finally { fs.rmSync(task.worktreePath, { recursive: true, force: true }); }
    });

    it("journals direct internal follow-ups through the same submission owner", async () => {
      const task = await orchestrator.createTask("Initial", "claude-code", "/tmp/repo", "main");
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "chat.sqlite"));
      const chat = new ChatJournal(db, id => orchestrator.getTask(id));
      orchestrator.on("orchestrator:submission", (submission: TaskSubmission) => chat.submission(submission));
      try {
        await orchestrator.startTask(task.id);
        await orchestrator.iterate(task.id, "Internal follow-up", undefined, "internal-send");
        expect(chat.snapshot(task.id).messages.map(message => ({ content: message.content, pending: message.pendingDelivery }))).toEqual([
          { content: "Initial", pending: undefined }, { content: "Internal follow-up", pending: undefined },
        ]);
      } finally { db.close(); }
    });

    it("rejects a send before provider dispatch when its pending intent cannot be saved", async () => {
      const task = await orchestrator.createTask("Initial", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      orchestrator.on("orchestrator:submission", (submission: TaskSubmission) => {
        if (submission.state === "pending") throw new Error("Disk full");
      });
      await expect(orchestrator.iterate(task.id, "Must not dispatch")).rejects.toThrow("Disk full");
      expect({ calls: mockAdapterInstances[0].iterateCalls, feedback: task.feedback, turn: task.turnNumber }).toEqual({ calls: [], feedback: [], turn: undefined });
    });

    it.each(["codex", "claude-code"] as const)("sends %s initial settings through the native adapter boundary", async agent => {
      const task = await orchestrator.createTask("Original prompt", agent, "/tmp/repo", "main");
      task.model = "selected-model";
      task.reasoningLevel = "low";
      task.planFirst = true;
      await orchestrator.startTask(task.id);
      expect({ prompt: mockAdapterInstances[0].spawnPrompt, options: mockAdapterInstances[0].spawnOptions }).toEqual({ prompt: "Original prompt", options: { model: "selected-model", reasoningLevel: "low", mode: "plan" } });
    });

    it("retains separately acknowledged settings when a later control rejects a message", async () => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "done", timestamp: 1000, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      vi.spyOn(adapter, "iterate").mockImplementation(() => {
        adapter.simulateEvent({ type: "configuration_changed", timestamp: 1001, configuration: { mode: "plan" } });
        throw new Error("Model unavailable");
      });
      await expect(orchestrator.iterate(task.id, "Rejected", { mode: "plan", model: "unavailable" })).rejects.toThrow("Model unavailable");
      expect({ status: task.status, plan: task.planFirst, model: task.model }).toEqual({ status: "reviewing", plan: true, model: undefined });
    });

    it("classifies a completed Codex turn using its own late settings acknowledgement", async () => {
      const task = await orchestrator.createTask("Plan", "codex", "/tmp/repo", "main");
      task.planFirst = true;
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "done", timestamp: 1000, outcome: "completed", turnId: "fast-turn" });
      adapter.simulateEvent({ type: "configuration_changed", timestamp: 1001, turnId: "fast-turn", configuration: { mode: "instant" } });
      await vi.advanceTimersByTimeAsync(0);
      expect({ status: task.status, plan: task.planFirst }).toEqual({ status: "reviewing", plan: false });
    });

    it.each(["codex", "claude-code"] as const)("keeps explicit %s resets through cold resume", async agent => {
      const task = await orchestrator.createTask("Original", agent, "/tmp/repo", "main");
      task.model = "prior-model";
      task.reasoningLevel = "low";
      task.planFirst = true;
      task.status = "reviewing";
      await orchestrator.persistTasks();
      await orchestrator.iterate(task.id, "Reset settings", { mode: "instant", model: null, reasoningLevel: null });
      expect({ options: mockAdapterInstances[0].spawnOptions, model: task.model, effort: task.reasoningLevel, plan: task.planFirst }).toEqual({
        options: { mode: "instant", model: null, reasoningLevel: null }, model: undefined, effort: undefined, plan: false,
      });
    });

    it.each([false, true])("retains only acknowledged cold-start settings when startup rejects (mode acknowledged: %s)", async acknowledged => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      task.model = "prior-model";
      task.reasoningLevel = "low";
      task.status = "reviewing";
      await orchestrator.persistTasks();
      const spawn = vi.spyOn(MockAdapter.prototype, "spawn").mockImplementationOnce(function(this: MockAdapter) {
        if (acknowledged) this.simulateEvent({ type: "configuration_changed", timestamp: 1, configuration: { mode: "plan" } });
        throw new Error("Model unavailable");
      });
      try {
        await expect(orchestrator.iterate(task.id, "Rejected", { mode: "plan", model: "unavailable", reasoningLevel: "high" })).rejects.toThrow("Model unavailable");
        const saved = new Orchestrator({ persistPath, timeoutMs: 5000 }).getTask(task.id);
        expect({ model: saved?.model, effort: saved?.reasoningLevel, plan: Boolean(saved?.planFirst), feedback: saved?.feedback }).toEqual({
          model: "prior-model", effort: "low", plan: acknowledged, feedback: [],
        });
      } finally { spawn.mockRestore(); }
    });

    it("waits for the pre-turn snapshot before calling a live adapter", async () => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      let release!: (sha: string) => void;
      const snapshot = "a".repeat(40);
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "snapshotHead")
        .mockReturnValueOnce(new Promise(resolve => { release = resolve; }));
      const iteration = orchestrator.iterate(task.id, "Next turn");
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(mockAdapterInstances[0].iterateCalls).toEqual([]);
        release(snapshot);
        await iteration;
        expect(mockAdapterInstances[0].iterateCalls).toEqual(["Next turn"]);
      } finally {
        release(snapshot);
        await iteration;
        checkpoint.mockRestore();
      }
    });

    it("does not dispatch or respawn a message stopped while its checkpoint is pending", async () => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      let release!: (sha: string) => void;
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "snapshotHead")
        .mockReturnValueOnce(new Promise(resolve => { release = resolve; }));
      const snapshot = "a".repeat(40);
      const iteration = orchestrator.iterate(task.id, "Cancelled message");
      const cancelled = expect(iteration).rejects.toThrow("Message cancelled before submission");
      try {
        await vi.advanceTimersByTimeAsync(0);
        await orchestrator.cancelTask(task.id);
        release(snapshot);
        await cancelled;
        expect({ status: task.status, processes: mockAdapterInstances.length, submissions: mockAdapterInstances[0].iterateCalls,
          feedback: task.feedback, turn: task.turnNumber }).toEqual({ status: "stopped", processes: 1, submissions: [], feedback: [], turn: undefined });
      } finally {
        release(snapshot);
        await iteration.catch(() => {});
        checkpoint.mockRestore();
      }
    });

    it("re-spawns agent with feedback and updates status", async () => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      // Simulate agent completing.
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("reviewing");

      // Send feedback.
      await orchestrator.iterate(task.id, "Fix the error handling");

      expect(task.feedback).toContain("Fix the error handling");
      expect(task.status).toBe("running");
      // A new adapter should have been created.
      expect(mockAdapterInstances).toHaveLength(2);
      expect(mockAdapterInstances[1].spawned).toBe(true);
      expect(mockAdapterInstances[1].spawnPrompt).toContain("Fix the error handling");
    });

    it("calls adapter.iterate when present (no prompt rewrite, no startTask)", async () => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      // The adapter is still alive (no exit). Since MockAdapter now has
      // iterate(), the orchestrator should call it directly.
      await orchestrator.iterate(task.id, "Add logging");

      // The live adapter's iterate() should have been called with the
      // raw feedback — no prompt rewriting.
      expect(mockAdapterInstances[0].iterateCalls).toEqual(["Add logging"]);
      // No new adapter should have been spawned.
      expect(mockAdapterInstances).toHaveLength(1);
      // Task should transition to running.
      expect(task.status).toBe("running");
      // Feedback is recorded.
      expect(task.feedback).toContain("Add logging");
    });
  });

  describe("archiveTask", () => {
    it("waits for the completed turn's background snapshot before archiving", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Save work", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      let release!: (result: { stdout: string; stderr: string }) => void;
      heldPostTurnGitAdd = new Promise(resolve => { release = resolve; });
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1000, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const archive = orchestrator.archiveTask(task.id);
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(vi.mocked(removeWorktree).mock.calls).toEqual([]);
        heldPostTurnGitAdd = undefined;
        release({ stdout: "", stderr: "" });
        await archive;
        expect(task.status).toBe("archived");
      } finally {
        heldPostTurnGitAdd = undefined;
        release({ stdout: "", stderr: "" });
        await archive.catch(() => {});
        fs.rmSync(task.worktreePath, { recursive: true, force: true });
      }
    });

    it("waits for shared process and checkpoint closure before removing the workspace", async () => {
      orchestrator = new Orchestrator({ persistPath, maxConcurrent: 1, timeoutMs: 5000 });
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const parent = await orchestrator.createTask("Own workspace", "codex", "/tmp/repo", "main");
      mockCurrentBranch = parent.branch;
      const child = orchestrator.createChildTaskForAgent(parent.id, "codex", "Work");
      await orchestrator.startTask(child.id);
      const queued = orchestrator.createChildTaskForAgent(parent.id, "codex", "Wait");
      const startup = orchestrator.startTask(queued.id);
      const stoppedStartup = expect(startup).rejects.toThrow("Task startup was cancelled");
      await vi.advanceTimersByTimeAsync(0);
      let releaseCheckpoint!: (sha: string) => void;
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "snapshotHead")
        .mockReturnValueOnce(new Promise(resolve => { releaseCheckpoint = resolve; }));
      const input = orchestrator.iterate(child.id, "Pending checkpoint input");
      const cancelledInput = expect(input).rejects.toThrow("Message cancelled before submission");
      await vi.advanceTimersByTimeAsync(0);
      let close!: () => void;
      vi.spyOn(mockAdapterInstances[0], "cancel").mockReturnValue(new Promise<void>(resolve => { close = resolve; }));
      const archive = orchestrator.archiveTask(parent.id);
      const snapshot = "a".repeat(40);
      try {
        await vi.advanceTimersByTimeAsync(0);
        await expect(orchestrator.iterate(parent.id, "Too late")).rejects.toThrow("being archived");
        expect(vi.mocked(removeWorktree).mock.calls).toEqual([]);
        close();
        await vi.advanceTimersByTimeAsync(0);
        expect(vi.mocked(removeWorktree).mock.calls).toEqual([]);
        releaseCheckpoint(snapshot);
        await Promise.all([archive, stoppedStartup, cancelledInput]);
        expect({ status: parent.status, childStatus: child.status, queuedStatus: queued.status, processes: mockAdapterInstances.length, removal: vi.mocked(removeWorktree).mock.calls }).toEqual({ status: "archived", childStatus: "stopped", queuedStatus: "stopped", processes: 1, removal: [[parent.repoPath, parent.id, { preserveBranch: true }]] });
      } finally {
        close();
        releaseCheckpoint(snapshot);
        await archive.catch(() => {});
        checkpoint.mockRestore();
        fs.rmSync(parent.worktreePath, { recursive: true, force: true });
      }
    });

    it.each(["primary", "existing"] as const)("archives only the %s conversation without modifying its shared workspace", async mode => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "crab-archive-borrowed-"));
      try {
        const task = await orchestrator.createTask("Work", "codex", workspace, "main", { worktreeMode: mode, ...(mode === "existing" ? { worktreePath: workspace } : {}) });
        await orchestrator.startTask(task.id);
        const peer = orchestrator.createChildTaskForAgent(task.id, "codex", "Independent work");
        await orchestrator.startTask(peer.id);
        execFileCalls = [];
        await orchestrator.archiveTask(task.id);
        expect({ status: task.status, stopped: mockAdapterInstances[0].cancelled, peerStatus: peer.status, peerStopped: mockAdapterInstances[1].cancelled, removed: vi.mocked(removeWorktree).mock.calls, git: execFileCalls, exists: fs.existsSync(workspace) }).toEqual({ status: "archived", stopped: true, peerStatus: "running", peerStopped: false, removed: [], git: [], exists: true });
      } finally { fs.rmSync(workspace, { recursive: true, force: true }); }
    });

    it("leaves the workspace intact when its snapshot cannot be saved", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Save work", "codex", "/tmp/repo", "main");
      failPostTurnGitAdd = true;
      try {
        await expect(orchestrator.archiveTask(task.id)).rejects.toThrow("Archive snapshot failed");
        expect({ status: task.status, exists: fs.existsSync(task.worktreePath), removed: vi.mocked(removeWorktree).mock.calls }).toEqual({ status: "stopped", exists: true, removed: [] });
      } finally { fs.rmSync(task.worktreePath, { recursive: true, force: true }); }
    });

    it("does not remove a workspace when durable archive intent fails", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Save work", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1000, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const save = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks").mockImplementationOnce(() => { throw new Error("Database unavailable"); });
      try {
        await expect(orchestrator.archiveTask(task.id)).rejects.toThrow("Database unavailable");
        expect({ status: task.status, exists: fs.existsSync(task.worktreePath), removed: vi.mocked(removeWorktree).mock.calls }).toEqual({ status: "reviewing", exists: true, removed: [] });
      } finally {
        save.mockRestore();
        fs.rmSync(task.worktreePath, { recursive: true, force: true });
      }
    });

    it("rolls back archive metadata when removing the saved worktree fails", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Save work", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1000, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      vi.mocked(removeWorktree).mockRejectedValueOnce(new Error("Workspace is locked"));
      try {
        await expect(orchestrator.archiveTask(task.id)).rejects.toThrow("Workspace is locked");
        const saved = new Orchestrator({ persistPath }).getTask(task.id);
        expect({ current: task.status, saved: saved?.status, exists: fs.existsSync(task.worktreePath) }).toEqual({ current: "reviewing", saved: "reviewing", exists: true });
      } finally { fs.rmSync(task.worktreePath, { recursive: true, force: true }); }
    });
  });

  describe("restoreTask", () => {
    it("does not recreate a missing saved conversation from its original base during startup", async () => {
      const { createWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Saved conversation", "codex", "/tmp/repo", "main");
      task.workspaceSetup = undefined;
      fs.rmSync(task.worktreePath, { recursive: true, force: true });
      const creations = vi.mocked(createWorktree).mock.calls.length;
      await expect(orchestrator.startTask(task.id)).rejects.toThrow("does not exist");
      expect({ creations: vi.mocked(createWorktree).mock.calls.length - creations, providers: mockAdapterInstances.length }).toEqual({ creations: 0, providers: 0 });
    });

    it.each(["created", "archived"] as const)("restores a missing shared workspace from its %s owner's saved branch before retrying its child's exact input", async status => {
      const { restoreWorktree } = await import("../electron/orchestrator-worktree");
      const parent = await orchestrator.createTask("Parent", "codex", "/tmp/repo", "main");
      parent.branch = "agent/saved-work";
      const previousPath = parent.worktreePath;
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child");
      parent.status = status;
      child.status = "failed";
      child.pendingInput = { id: "unsent", content: "Latest input", kind: "message", phase: "unsent", turnNumber: 1 };
      const restored = fs.mkdtempSync(path.join(os.tmpdir(), "crab-retry-workspace-"));
      fs.rmSync(parent.worktreePath, { recursive: true, force: true });
      vi.mocked(restoreWorktree).mockClear();
      vi.mocked(restoreWorktree).mockResolvedValueOnce(restored);
      try {
        await orchestrator.retryTask(child.id);
        expect({ restored: vi.mocked(restoreWorktree).mock.calls, paths: [parent.worktreePath, child.worktreePath],
          input: mockAdapterInstances.at(-1)?.spawnPrompt, cwd: mockAdapterInstances.at(-1)?.spawnCwd, ownerStatus: parent.status }).toEqual({
          restored: [[parent.repoPath, parent.id, "agent/saved-work", previousPath]], paths: [restored, restored], input: "Latest input", cwd: restored, ownerStatus: status === "archived" ? "reviewing" : "stopped",
        });
      } finally { fs.rmSync(restored, { recursive: true, force: true }); }
    });

    it.each(["primary", "existing"] as const)("never replaces a missing user-owned %s directory on Retry", async mode => {
      const { restoreWorktree, createWorktree } = await import("../electron/orchestrator-worktree");
      const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "crab-user-workspace-"));
      const task = await orchestrator.createTask("Keep user ownership", "claude-code", workspace, "main", { worktreeMode: mode, worktreePath: workspace });
      task.status = "failed";
      fs.rmSync(workspace, { recursive: true, force: true });
      vi.mocked(restoreWorktree).mockClear();
      vi.mocked(createWorktree).mockClear();
      await expect(orchestrator.retryTask(task.id)).rejects.toThrow("does not exist");
      expect({ restored: vi.mocked(restoreWorktree).mock.calls, created: vi.mocked(createWorktree).mock.calls, providers: mockAdapterInstances.length }).toEqual({ restored: [], created: [], providers: 0 });
    });

    it("does not revive a discarded owned workspace through a child's Retry", async () => {
      const { restoreWorktree } = await import("../electron/orchestrator-worktree");
      const parent = await orchestrator.createTask("Parent", "codex", "/tmp/repo", "main");
      const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child");
      parent.status = "discarded";
      child.status = "failed";
      fs.rmSync(parent.worktreePath, { recursive: true, force: true });
      vi.mocked(restoreWorktree).mockClear();
      await expect(orchestrator.retryTask(child.id)).rejects.toThrow("discarded");
      expect({ restores: vi.mocked(restoreWorktree).mock.calls, providers: mockAdapterInstances.length, parent: parent.status }).toEqual({ restores: [], providers: 0, parent: "discarded" });
    });

    it("reopens an applied conversation without dispatching input or changing Git", async () => {
      const task = await orchestrator.createTask("Applied work", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1000, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      await orchestrator.applyTask(task.id);
      execFileCalls = [];
      try {
        await orchestrator.restoreTask(task.id);
        expect({ status: task.status, processes: mockAdapterInstances.length, closed: mockAdapterInstances[0].cancelled, submitted: mockAdapterInstances[0].iterateCalls, git: execFileCalls }).toEqual({ status: "reviewing", processes: 1, closed: true, submitted: [], git: [] });
      } finally { fs.rmSync(task.worktreePath, { recursive: true, force: true }); }
    });

    it.each(["auto", "primary", "existing"] as const)("reopens an existing %s workspace without recreating it or resubmitting its prompt", async mode => {
      const { restoreWorktree } = await import("../electron/orchestrator-worktree");
      const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "crab-restore-"));
      let taskPath = workspace;
      try {
        const task = await orchestrator.createTask("Initial", "codex", workspace, "main", { worktreeMode: mode, ...(mode === "existing" ? { worktreePath: workspace } : {}) });
        taskPath = task.worktreePath;
        await orchestrator.startTask(task.id);
        const previous = mockAdapterInstances[0];
        task.status = "archived";
        vi.mocked(restoreWorktree).mockClear();
        await orchestrator.restoreTask(task.id);
        expect({ status: task.status, retired: previous.cancelled, processes: mockAdapterInstances.length, recreated: vi.mocked(restoreWorktree).mock.calls }).toEqual({ status: "reviewing", retired: true, processes: 1, recreated: [] });
      } finally {
        fs.rmSync(workspace, { recursive: true, force: true });
        if (taskPath !== workspace) fs.rmSync(taskPath, { recursive: true, force: true });
      }
    });

    it("rebinds borrowing conversations after their owner is recreated at a different path", async () => {
      const { restoreWorktree } = await import("../electron/orchestrator-worktree");
      const parent = await orchestrator.createTask("Initial", "codex", "/tmp/repo", "main");
      const previousPath = parent.worktreePath;
      const restoredPath = fs.mkdtempSync(path.join(os.tmpdir(), "crab-restored-owner-"));
      fs.mkdirSync(previousPath, { recursive: true });
      try {
        const child = orchestrator.createChildTaskForAgent(parent.id, "codex", "Review");
        await orchestrator.startTask(child.id);
        mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1, outcome: "completed" });
        await vi.advanceTimersByTimeAsync(0);
        parent.status = "archived";
        fs.rmSync(previousPath, { recursive: true, force: true });
        vi.mocked(restoreWorktree).mockResolvedValueOnce(restoredPath);
        await orchestrator.restoreTask(parent.id);
        expect({ parent: parent.worktreePath, child: child.worktreePath, ownership: child.worktreeMode, closed: mockAdapterInstances[0].cancelled, processes: mockAdapterInstances.length }).toEqual({ parent: restoredPath, child: restoredPath, ownership: "existing", closed: true, processes: 1 });
      } finally {
        fs.rmSync(previousPath, { recursive: true, force: true });
        fs.rmSync(restoredPath, { recursive: true, force: true });
      }
    });
  });

  describe("applyTask", () => {
    it.each(["archived", "applied", "discarded"] as const)("requires reopening a %s conversation before applying files", async status => {
      const task = await orchestrator.createTask("Closed", "codex", "/tmp/repo");
      task.status = status;
      execFileCalls = [];
      await expect(orchestrator.applyTask(task.id)).rejects.toThrow("Reopen this conversation before applying changes");
      expect({ status: task.status, git: execFileCalls }).toEqual({ status, git: [] });
    });

    it("updates status to applied", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      await orchestrator.applyTask(task.id);

      expect(task.status).toBe("applied");
    });
  });

  describe("discardTask", () => {
    it("removes worktree and updates status to discarded", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");

      await orchestrator.discardTask(task.id);

      expect(task.status).toBe("discarded");
      expect(removeWorktree).toHaveBeenCalledWith(task.repoPath, task.id);
    });

    it("cancels running adapter before discarding", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      await orchestrator.discardTask(task.id);

      expect(mockAdapterInstances[0].cancelled).toBe(true);
      expect(task.status).toBe("discarded");
    });

    it("does not remove the primary workspace when discarded", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const repoDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-primary-repo-"));
      try {
        const task = await orchestrator.createTask(
          "Direct workspace",
          "codex",
          repoDir,
          "main",
          { worktreeMode: "primary" },
        );

        await orchestrator.discardTask(task.id);

        expect(task.status).toBe("discarded");
        expect(removeWorktree).not.toHaveBeenCalled();
      } finally {
        fs.rmSync(repoDir, { recursive: true, force: true });
      }
    });
  });

  describe("full lifecycle", () => {
    it("create → start → complete → apply", async () => {
      const statusHistory: string[] = [];
      orchestrator.on("orchestrator:task-status", (e: any) => {
        statusHistory.push(e.status);
      });

      // Create.
      const task = await orchestrator.createTask(
        "Implement feature X",
        "claude-code",
        "/tmp/repo",
        "main",
      );
      expect(task.status).toBe("created");

      // Start.
      await orchestrator.startTask(task.id);
      expect(task.status).toBe("running");

      // Agent works and exits successfully.
      mockAdapterInstances[0].simulateEvent({
        type: "file_edit",
        timestamp: Date.now(),
        path: "src/feature.ts",
      });
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("reviewing");

      // Apply.
      await orchestrator.applyTask(task.id);
      expect(task.status).toBe("applied");

      expect(statusHistory).toEqual([
        "created",
        "running",
        "reviewing",
        "applied",
      ]);
    });

    it("create → start → fail → discard", async () => {
      const task = await orchestrator.createTask(
        "Broken task",
        "codex",
        "/tmp/repo",
        "main",
      );

      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateExit(1);
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("failed");

      await orchestrator.discardTask(task.id);
      expect(task.status).toBe("discarded");
    });

    it("create → start → cancel → preserves worktree", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      await orchestrator.cancelTask(task.id);

      expect(task.status).toBe("stopped");
      // cancelTask preserves the worktree — removeWorktree should NOT be called.
      // It was only called if discardTask is used.
      // Check that the worktree path is still set.
      expect(task.worktreePath).toBeDefined();
    });

    it("create → start → complete → iterate → complete → apply", async () => {
      const task = await orchestrator.createTask("Build it", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      // First completion.
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("reviewing");

      // Iterate with feedback.
      await orchestrator.iterate(task.id, "Add tests");
      expect(task.status).toBe("running");

      // Second completion.
      mockAdapterInstances[1].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("reviewing");

      // Apply.
      await orchestrator.applyTask(task.id);
      expect(task.status).toBe("applied");
    });
  });

  describe("getTask / getAllTasks", () => {
    it("returns task by ID", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      const found = orchestrator.getTask(task.id);
      expect(found).toBe(task);
    });

    it("returns undefined for unknown ID", () => {
      expect(orchestrator.getTask("unknown")).toBeUndefined();
    });

    it("returns all tasks", async () => {
      await orchestrator.createTask("Task 1", "claude-code", "/tmp/repo", "main");
      await orchestrator.createTask("Task 2", "codex", "/tmp/repo", "main");

      const all = orchestrator.getAllTasks();
      expect(all).toHaveLength(2);
    });
  });

  describe("remote task creation", () => {
    it("routes worktree creation to remote agent for ssh:// repos", async () => {
      const mockRouter = {
        route: vi.fn(async (_repoId: string, _method: string, _params: unknown) => {
          return { path: "/home/user/.crab/worktrees/swift-falcon" };
        }),
      } as any;

      const remoteOrch = new Orchestrator({
        persistPath,
        timeoutMs: 5000,
        rpcRouter: mockRouter,
      });

      const task = await remoteOrch.createTask(
        "Fix remote bug",
        "claude-code",
        "ssh://gpu-server/home/user/repos/ml-project",
        "HEAD",
      );

      expect(task.isRemote).toBe(true);
      expect(task.remoteProfileId).toBe("gpu-server");
      expect(task.worktreePath).toBe("/home/user/.crab/worktrees/swift-falcon");
      expect(task.repoPath).toBe("ssh://gpu-server/home/user/repos/ml-project");
      expect(task.status).toBe("created");

      expect(mockRouter.route).toHaveBeenCalledWith(
        "ssh://gpu-server/home/user/repos/ml-project",
        "worktree_create",
        expect.objectContaining({
          repo_id: "ssh://gpu-server/home/user/repos/ml-project",
          base_ref: "HEAD",
          branch: expect.stringMatching(/^agent\//),
        }),
      );
    });

    it("throws when no RPC router is configured for remote repos", async () => {
      // Default orchestrator has no rpcRouter.
      await expect(
        orchestrator.createTask(
          "Remote task",
          "claude-code",
          "ssh://server/path/to/repo",
          "HEAD",
        ),
      ).rejects.toThrow("SSH router not configured");
    });

    it("throws with descriptive error when remote worktree creation fails", async () => {
      const mockRouter = {
        route: vi.fn(async () => {
          throw new Error("Connection lost: write operations are unavailable during reconnection");
        }),
      } as any;

      const remoteOrch = new Orchestrator({
        persistPath,
        timeoutMs: 5000,
        rpcRouter: mockRouter,
      });

      await expect(
        remoteOrch.createTask(
          "Remote task",
          "claude-code",
          "ssh://my-server/home/user/repo",
          "main",
        ),
      ).rejects.toThrow("Failed to create worktree on remote (my-server)");
    });

    it("does not route local repos through the SSH router", async () => {
      const mockRouter = {
        route: vi.fn(),
      } as any;

      const remoteOrch = new Orchestrator({
        persistPath,
        timeoutMs: 5000,
        rpcRouter: mockRouter,
      });

      const task = await remoteOrch.createTask(
        "Local task",
        "claude-code",
        "/tmp/repo",
        "main",
      );

      expect(task.isRemote).toBeUndefined();
      expect(task.remoteProfileId).toBeUndefined();
      expect(mockRouter.route).not.toHaveBeenCalled();
    });

    it("persists remote task fields to disk", async () => {
      const mockRouter = {
        route: vi.fn(async () => ({ path: "/remote/worktree/path" })),
      } as any;

      const remoteOrch = new Orchestrator({
        persistPath,
        timeoutMs: 5000,
        rpcRouter: mockRouter,
      });

      await remoteOrch.createTask(
        "Remote persist test",
        "claude-code",
        "ssh://build-box/home/dev/project",
        "HEAD",
      );

      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const persisted = reloaded.getAllTasks()[0];
      expect(persisted.isRemote).toBe(true);
      expect(persisted.remoteProfileId).toBe("build-box");
      expect(persisted.worktreePath).toBe("/remote/worktree/path");
    });

    it("setRpcRouter allows setting the router after construction", async () => {
      const mockRouter = {
        route: vi.fn(async () => ({ path: "/remote/path" })),
      } as any;

      // Start without a router.
      const orch = new Orchestrator({ persistPath, timeoutMs: 5000 });

      // Should fail without router.
      await expect(
        orch.createTask("Test", "claude-code", "ssh://server/repo", "HEAD"),
      ).rejects.toThrow("SSH router not configured");

      // Set the router.
      orch.setRpcRouter(mockRouter);

      // Should now succeed.
      const task = await orch.createTask(
        "Test",
        "claude-code",
        "ssh://server/repo",
        "HEAD",
      );
      expect(task.isRemote).toBe(true);
    });

    it("emits task-status event for remote task creation", async () => {
      const mockRouter = {
        route: vi.fn(async () => ({ path: "/remote/worktree" })),
      } as any;

      const remoteOrch = new Orchestrator({
        persistPath,
        timeoutMs: 5000,
        rpcRouter: mockRouter,
      });

      const events: any[] = [];
      remoteOrch.on("orchestrator:task-status", (e) => events.push(e));

      const task = await remoteOrch.createTask(
        "Remote task",
        "claude-code",
        "ssh://server/repo",
        "HEAD",
      );

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ taskId: task.id, status: "created" });
    });
  });

  describe("Claude session continuation", () => {
    it("persists the session identity and resumes with only the user's follow-up", async () => {
      const task = await orchestrator.createTask("Original task", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "session_started", timestamp: 1000, sessionId: "claude-session" });
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);
      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(reloaded.getTask(task.id)?.claudeSessionId).toBe("claude-session");
      await orchestrator.iterate(task.id, "Continue the existing conversation");
      expect(mockAdapterInstances[1].spawnPrompt).toBe("Continue the existing conversation");
    });

    it("restores a lost later turn as interrupted instead of reusing its previous completion", async () => {
      const task = await orchestrator.createTask("Original task", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", outcome: "completed", timestamp: 1000, content: "Done" });
      await vi.advanceTimersByTimeAsync(0);
      await orchestrator.iterate(task.id, "Next turn");
      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect({ status: task.status, recovered: reloaded.getTask(task.id)?.status, completedAt: task.completedAt, calls: mockAdapterInstances[0].iterateCalls }).toEqual({
        status: "running", recovered: "interrupted", completedAt: undefined, calls: ["Next turn"],
      });
    });

    it("preserves stopped status when the cancelled process exits later", async () => {
      const task = await orchestrator.createTask("Stop this task", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      await orchestrator.cancelTask(task.id);
      mockAdapterInstances[0].simulateExit(1);
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("stopped");
    });
  });

  describe("Codex thread resume", () => {
    it("captures thread_id from a thread_started event and persists it", async () => {
      const task = await orchestrator.createTask("Refactor push", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      mockAdapterInstances[0].simulateEvent({
        type: "thread_started",
        timestamp: 1000,
        threadId: "thr-abc-123",
      });
      // Allow the async persist to flush.
      await vi.advanceTimersByTimeAsync(0);

      expect(task.codexThreadId).toBe("thr-abc-123");

      const reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(reloaded.getTask(task.id)?.codexThreadId).toBe("thr-abc-123");
    });

    it("retains the saved thread identity when a resume error is reported", async () => {
      const task = await orchestrator.createTask("Refactor", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      mockAdapterInstances[0].simulateEvent({
        type: "thread_started",
        timestamp: 1000,
        threadId: "thr-stale",
      });
      await vi.advanceTimersByTimeAsync(0);
      expect(task.codexThreadId).toBe("thr-stale");

      // A failed resume must remain retryable without discarding context.
      mockAdapterInstances[0].simulateEvent({
        type: "error",
        timestamp: 2000,
        error: "thread not found: thr-stale",
      });
      await vi.advanceTimersByTimeAsync(0);

      expect(task.codexThreadId).toBe("thr-stale");
    });

    it("iterate sends only the feedback when a Codex thread is available", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      mockAdapterInstances[0].simulateEvent({
        type: "thread_started",
        timestamp: 500,
        threadId: "thr-xyz",
      });
      await vi.advanceTimersByTimeAsync(0);

      // Finish the first turn.
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      await orchestrator.iterate(task.id, "Make it async");

      expect(mockAdapterInstances).toHaveLength(2);
      // With a thread id present, iterate must NOT use the legacy
      // "Previous task: …\n\nFeedback: …" wrapper. The prompt is the
      // bare feedback the resumed thread will append to its context.
      expect(mockAdapterInstances[1].spawnPrompt).toBe("Make it async");
    });

    it("sends only the new input without rewriting task identity when no native session is known", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      await orchestrator.iterate(task.id, "Add a test");

      expect({ input: mockAdapterInstances[1].spawnPrompt, original: task.prompt }).toEqual({ input: "Add a test", original: "Original" });
    });
  });

  describe("approval decisions", () => {
    it("uses the provider outcome even when a usage event follows a fatal error", async () => {
      const task = await orchestrator.createTask("Test", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "error", timestamp: 1000, error: "Provider failed" });
      adapter.simulateEvent({ type: "usage", timestamp: 1001, usage: { inputTokens: 1, outputTokens: 0, cacheReadInputTokens: 0, cacheCreationInputTokens: 0 } });
      adapter.simulateEvent({ type: "done", timestamp: 1002, outcome: "failed", content: "" });
      await vi.advanceTimersByTimeAsync(0);
      expect(task.status).toBe("failed");
    });

    it("marks a queued provider turn running after an earlier turn finishes", async () => {
      const task = await orchestrator.createTask("Test", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "done", timestamp: 1000, outcome: "completed", content: "Done" });
      await vi.advanceTimersByTimeAsync(0);
      adapter.simulateEvent({ type: "turn_started", timestamp: 1001 });
      await vi.advanceTimersByTimeAsync(0);
      expect({ status: task.status, completedAt: task.completedAt }).toEqual({ status: "running", completedAt: undefined });
    });

    it.each(["codex", "claude-code"] as const)("delivers a follow-up model selection to %s and persists the accepted model", async agent => {
      const task = await orchestrator.createTask("Test", agent, "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "done", timestamp: 1000, outcome: "completed", content: "Done" });
      await vi.advanceTimersByTimeAsync(0);
      const submit = vi.spyOn(adapter, "iterate");
      await orchestrator.iterate(task.id, "Follow-up", { model: "configured-model" });
      expect({ arguments: submit.mock.calls, model: task.model }).toEqual({ arguments: [["Follow-up", { model: "configured-model" }]], model: "configured-model" });
    });

    it("rejects a model change during an active turn before recording new feedback", async () => {
      const task = await orchestrator.createTask("Test", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      await expect(orchestrator.iterate(task.id, "Follow-up", { model: "configured-model" })).rejects.toThrow("Wait for the current turn");
      expect({ status: task.status, feedback: task.feedback, submissions: mockAdapterInstances[0].iterateCalls }).toEqual({ status: "running", feedback: [], submissions: [] });
    });

    it("surfaces a rejected live submission and preserves the existing task state", async () => {
      const task = await orchestrator.createTask("Test", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      adapter.simulateEvent({ type: "done", timestamp: 1000, outcome: "completed", content: "Done" });
      await vi.advanceTimersByTimeAsync(0);
      vi.spyOn(adapter, "iterate").mockImplementation(() => { throw new Error("Provider rejected input"); });
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "recordCheckpoint");
      try {
        await expect(orchestrator.iterate(task.id, "Follow-up")).rejects.toThrow("Provider rejected input");
        expect({ status: task.status, feedback: task.feedback, turn: task.turnNumber, checkpoints: checkpoint.mock.calls }).toEqual({
          status: "reviewing", feedback: [], turn: undefined, checkpoints: [],
        });
      } finally { checkpoint.mockRestore(); }
    });

    it("serializes concurrent bookkeeping and releases the next send after a rejection", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const adapter = mockAdapterInstances[0];
      let reject!: (error: Error) => void;
      const firstReply = new Promise<void>((_resolve, fail) => { reject = fail; });
      const dispatch = vi.spyOn(adapter, "iterate").mockImplementationOnce(() => firstReply);
      const first = orchestrator.iterate(task.id, "Rejected input");
      const rejected = expect(first).rejects.toThrow("Provider rejected input");
      const second = orchestrator.iterate(task.id, "Accepted input");
      await vi.advanceTimersByTimeAsync(0);
      expect(dispatch.mock.calls).toEqual([["Rejected input", undefined]]);
      reject(new Error("Provider rejected input"));
      await rejected;
      await second;
      expect({ calls: dispatch.mock.calls, feedback: task.feedback, turn: task.turnNumber }).toEqual({
        calls: [["Rejected input", undefined], ["Accepted input", undefined]], feedback: ["Accepted input"], turn: 1,
      });
    });

    it.each(["codex", "claude-code"] as const)("retries %s's latest accepted input after SQLite reload without overwriting its checkpoint", async agent => {
      const task = await orchestrator.createTask("Original", agent, "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      await orchestrator.iterate(task.id, "Latest accepted input");
      await orchestrator.cancelTask(task.id);
      const reloaded = new Orchestrator({ persistPath });
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "recordCheckpoint");
      try {
        await reloaded.retryTask(task.id);
        expect({ input: mockAdapterInstances.at(-1)?.spawnPrompt, original: reloaded.getTask(task.id)?.prompt,
          turn: reloaded.getTask(task.id)?.turnNumber, checkpoints: checkpoint.mock.calls.length }).toEqual({
          input: "Latest accepted input", original: "Original", turn: 1, checkpoints: 0,
        });
        await reloaded.iterate(task.id, "Next accepted input");
        expect(checkpoint.mock.calls.map(([, turn]) => turn)).toEqual([2]);
      } finally { checkpoint.mockRestore(); await reloaded.cancelTask(task.id); }
    });

    it("migrates a released task database's omitted checkpoint counter from its saved feedback", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      task.feedback = ["First follow-up", "Second follow-up"];
      task.status = "stopped";
      await orchestrator.persistTasks();
      const released = new Sqlite(persistPath);
      try { released.exec("ALTER TABLE tasks DROP COLUMN turn_number; ALTER TABLE tasks DROP COLUMN pending_input_json"); } finally { released.close(); }
      const upgraded = new Orchestrator({ persistPath });
      expect({ turn: upgraded.getTask(task.id)?.turnNumber, feedback: upgraded.getTask(task.id)?.feedback }).toEqual({
        turn: 2, feedback: ["First follow-up", "Second follow-up"],
      });
    });

    it.each([undefined, "accepted", "rejected"] as const)("recovers a durable %s input without native dispatch or duplicate feedback", async outcome => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      const id = "interrupted-input";
      const content = "Latest input";
      task.status = outcome === "accepted" ? "reviewing" : outcome === "rejected" ? "stopped" : "running";
      task.pendingInput = { id, content, kind: "message", phase: "prepared", turnNumber: 1, sha: "a".repeat(40), outcome };
      if (outcome === "accepted") { task.feedback = [content]; task.turnNumber = 1; }
      await orchestrator.persistTasks();
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "chat.sqlite"));
      const original = new ChatJournal(db, key => orchestrator.getTask(key));
      original.submission({ taskId: task.id, id, content, state: "pending" });
      const reloaded = new Orchestrator({ persistPath });
      const chat = new ChatJournal(db, key => reloaded.getTask(key));
      reloaded.on("orchestrator:submission", (submission: TaskSubmission) => chat.submission(submission));
      try {
        await reloaded.recoverTaskInput(task.id);
        await reloaded.recoverTaskInput(task.id);
        const restored = reloaded.getTask(task.id);
        const saved = new OrchestratorTaskDatabase(persistPath);
        try {
          expect({ feedback: restored?.feedback, turn: restored?.turnNumber, pending: saved.loadTasks()[0]?.pendingInput,
            processes: mockAdapterInstances.length, messages: chat.snapshot(task.id).messages.filter(message => message.id === id).map(message => ({
              content: message.content, error: message.deliveryError,
            })) }).toEqual({
            feedback: outcome === "rejected" ? [] : [content], turn: outcome === "rejected" ? undefined : 1,
            pending: undefined, processes: 0, messages: outcome === "rejected" ? [] : [{ content,
              error: outcome === "accepted" ? undefined : "Connection closed before message delivery was confirmed" }],
          });
        } finally { saved.close(); }
      } finally { db.close(); }
    });

    it("saves the exact input and snapshot pin before sending to the native provider", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      let prepared: unknown;
      vi.spyOn(mockAdapterInstances[0], "iterate").mockImplementation(() => {
        const saved = new OrchestratorTaskDatabase(persistPath);
        try { prepared = { input: saved.loadTasks()[0]?.pendingInput, pins: execFileCalls.filter(call => call.args[0] === "update-ref"
          && call.args[1] === `refs/crab/checkpoint-intents/${task.id}/1`).length }; } finally { saved.close(); }
      });
      await orchestrator.iterate(task.id, "Exact new input", undefined, "prepared-input");
      expect(prepared).toEqual({ input: { id: "prepared-input", content: "Exact new input", kind: "message", phase: "prepared", turnNumber: 1, sha: expect.any(String) }, pins: 1 });
    });

    it("does not send input when saving its durable preparation fails", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const replace = OrchestratorTaskDatabase.prototype.replaceTasks;
      let fail = true;
      const save = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks").mockImplementation(function(this: OrchestratorTaskDatabase, tasks) {
        if (fail && tasks.some(candidate => candidate.pendingInput && !candidate.pendingInput.outcome)) {
          fail = false;
          throw new Error("Preparation could not be saved");
        }
        return replace.call(this, tasks);
      });
      try {
        await expect(orchestrator.iterate(task.id, "Never sent")).rejects.toThrow("Preparation could not be saved");
        expect({ dispatched: mockAdapterInstances[0].iterateCalls, feedback: task.feedback, turn: task.turnNumber, pending: task.pendingInput }).toEqual({
          dispatched: [], feedback: [], turn: undefined, pending: undefined,
        });
      } finally { save.mockRestore(); }
    });

    it("preserves native acceptance and recovers once when its journal acknowledgement cannot be saved", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const failAcknowledgement = (submission: TaskSubmission) => {
        if (submission.state === "accepted") throw new Error("Acknowledgement could not be saved");
      };
      orchestrator.on("orchestrator:submission", failAcknowledgement);
      await expect(orchestrator.iterate(task.id, "Accepted native input")).rejects.toBeInstanceOf(AgentSubmissionUncertainError);
      orchestrator.off("orchestrator:submission", failAcknowledgement);
      await orchestrator.recoverTaskInput(task.id);
      expect({ feedback: task.feedback, turn: task.turnNumber, pending: task.pendingInput, calls: mockAdapterInstances[0].iterateCalls }).toEqual({
        feedback: ["Accepted native input"], turn: 1, pending: undefined, calls: ["Accepted native input"],
      });
    });

    it("reports checkpoint publication failure through the visible error channel while retaining native acceptance", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const errors: string[] = [];
      orchestrator.on("orchestrator:error", (error: { message: string }) => errors.push(error.message));
      const publication = vi.spyOn(CheckpointManager.prototype, "recordCheckpoint").mockRejectedValueOnce(new Error("Checkpoint ref could not be saved"));
      try {
        await orchestrator.iterate(task.id, "Accepted input");
        expect({ errors, feedback: task.feedback, pending: task.pendingInput?.outcome }).toEqual({
          errors: ["Input bookkeeping is incomplete: Checkpoint ref could not be saved"], feedback: ["Accepted input"], pending: "accepted",
        });
      } finally { publication.mockRestore(); }
    });

    it.each(["accepted", "rejected"] as const)("retains recoverable text when saving an %s input result fails", async outcome => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const db = new ChatDatabase(path.join(path.dirname(persistPath), "chat.sqlite"));
      const chat = new ChatJournal(db, id => orchestrator.getTask(id));
      orchestrator.on("orchestrator:submission", (submission: TaskSubmission) => chat.submission(submission));
      if (outcome === "rejected") vi.spyOn(mockAdapterInstances[0], "iterate").mockImplementation(() => { throw new Error("Native rejection"); });
      const replace = OrchestratorTaskDatabase.prototype.replaceTasks;
      let blocked = true;
      const save = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks").mockImplementation(function(this: OrchestratorTaskDatabase, tasks) {
        if (blocked && tasks.some(candidate => candidate.pendingInput?.outcome)) throw new Error("Result could not be saved");
        return replace.call(this, tasks);
      });
      try {
        const submission = orchestrator.iterate(task.id, "Recoverable input", undefined, "result-failure");
        if (outcome === "accepted") await submission;
        else await expect(submission).rejects.toThrow("Native rejection");
        expect(chat.snapshot(task.id).messages.find(message => message.id === "result-failure")?.deliveryError).toBe(
          outcome === "accepted" ? undefined : "Input result could not be saved. Check the conversation before retrying.",
        );
        blocked = false;
        await orchestrator.recoverTaskInput(task.id);
        expect({ feedback: task.feedback, pending: task.pendingInput, text: chat.snapshot(task.id).messages.filter(message => message.id === "result-failure").map(message => message.content),
          status: task.status }).toEqual({ feedback: outcome === "accepted" ? ["Recoverable input"] : [], pending: undefined,
          text: outcome === "accepted" ? ["Recoverable input"] : [], status: "running" });
      } finally { save.mockRestore(); db.close(); }
    });

    it("retains an uncertain input for explicit retry without treating it as a rejection", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      vi.spyOn(mockAdapterInstances[0], "iterate").mockImplementation(() => { throw new AgentSubmissionUncertainError(new Error("Lost response")); });
      await expect(orchestrator.iterate(task.id, "Possibly accepted input")).rejects.toBeInstanceOf(AgentSubmissionUncertainError);
      const reloaded = new Orchestrator({ persistPath });
      expect({ feedback: reloaded.getTask(task.id)?.feedback, turn: reloaded.getTask(task.id)?.turnNumber }).toEqual({
        feedback: ["Possibly accepted input"], turn: 1,
      });
    });

    it("keeps native acceptance when saving the acknowledged configuration fails", async () => {
      const task = await orchestrator.createTask("Work", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1000, outcome: "completed" });
      await vi.advanceTimersByTimeAsync(0);
      const save = vi.spyOn(OrchestratorTaskDatabase.prototype, "replaceTasks");
      vi.spyOn(mockAdapterInstances[0], "iterate").mockImplementation(() => { save.mockImplementationOnce(() => { throw new Error("Database unavailable"); }); });
      const states: TaskSubmission["state"][] = [];
      orchestrator.on("orchestrator:submission", (submission: TaskSubmission) => states.push(submission.state));
      try {
        await orchestrator.iterate(task.id, "Accepted work", { model: "selected-model" });
        expect({ states, model: task.model }).toEqual({ states: ["pending", "accepted"], model: "selected-model" });
      } finally { save.mockRestore(); }
    });
    it("routes answers only to the live task adapter without creating another turn", async () => {
      const task = await orchestrator.createTask("Ask a question", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      const delivered = orchestrator.recordInput(task.id, "question-1", { kind: "questions", answers: { mode: ["Thorough"] } });
      expect({ delivered, calls: mockAdapterInstances[0].respondInputCalls, iterations: mockAdapterInstances[0].iterateCalls, feedback: task.feedback }).toEqual({
        delivered: true, calls: [{ requestId: "question-1", response: { kind: "questions", answers: { mode: ["Thorough"] } } }], iterations: [], feedback: [],
      });
    });
    it("recordApproval returns false when no live adapter with respondApproval exists", async () => {
      const task = await orchestrator.createTask("Run tests", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      // The MockAdapter does implement respondApproval now, but if the
      // adapter has exited, recordApproval should return false.
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      const ok = await orchestrator.recordApproval(task.id, {
        requestId: "req-1",
        decision: "allow",
        summary: "Run command: cargo test",
      });
      expect(ok).toBe(false);
    });

    it("recordApproval calls adapter.respondApproval directly (no persistence)", async () => {
      const task = await orchestrator.createTask("Run tests", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      // Adapter is still alive — recordApproval should deliver directly.
      const ok = await orchestrator.recordApproval(task.id, {
        requestId: "req-42",
        decision: "allow_once",
        summary: "Execute: npm install",
      });

      expect(ok).toBe(true);
      // The adapter's respondApproval should have been called with the
      // exact requestId and decision.
      expect(mockAdapterInstances[0].respondApprovalCalls).toEqual([
        { requestId: "req-42", decision: "allow_once" },
      ]);
    });
  });
});
