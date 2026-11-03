/**
 * Unit tests for the Orchestrator task lifecycle manager.
 *
 * Mocks git commands and agent adapters to test the state machine
 * transitions, persistence, and timeout behaviour without spawning
 * real processes.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { Orchestrator, Task } from "../electron/orchestrator";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock the worktree module — no real git operations.
vi.mock("../electron/orchestrator-worktree", () => ({
  createWorktree: vi.fn(async (_repoPath: string, taskId: string, _baseRef: string) => {
    return path.join(os.tmpdir(), `crab-agent-${taskId}`);
  }),
  removeWorktree: vi.fn(async () => {}),
  readTaskMetadata: vi.fn(() => undefined),
}));

// Track mock adapter instances for assertions.
let mockAdapterInstances: MockAdapter[] = [];
let hangPostTurnGitAdd = false;
let execFileCalls: Array<{ command: string; args: string[]; cwd?: string }> = [];
let mockCurrentBranch = "main";

class MockAdapter {
  name = "Mock Agent";
  binary = "mock-agent";
  private eventListeners = new Set<(event: any) => void>();
  private exitListeners = new Set<(code: number) => void>();
  spawned = false;
  cancelled = false;
  spawnPrompt = "";
  spawnCwd = "";

  // Track iterate() and respondApproval() calls.
  iterateCalls: string[] = [];
  respondApprovalCalls: Array<{ requestId: string; decision: string }> = [];

  spawn(prompt: string, cwd: string): void {
    this.spawned = true;
    this.spawnPrompt = prompt;
    this.spawnCwd = cwd;
  }

  sendMessage(_text: string): void {}

  cancel(): void {
    this.cancelled = true;
  }

  iterate(prompt: string): void {
    this.iterateCalls.push(prompt);
  }

  respondApproval(requestId: string, decision: string): void {
    this.respondApprovalCalls.push({ requestId, decision });
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

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Orchestrator", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    mockAdapterInstances = [];
    hangPostTurnGitAdd = false;
    execFileCalls = [];
    mockCurrentBranch = "main";
    persistPath = createTempPersistPath();
    orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000 });
  });

  afterEach(() => {
    vi.useRealTimers();
    cleanupPath(persistPath);
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

  describe("startTask", () => {
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

    it("transitions Codex app-server turns to reviewing on done without waiting for process exit", async () => {
      const task = await orchestrator.createTask("Test", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1000, content: "done" });
      await vi.advanceTimersByTimeAsync(0);

      expect(task.status).toBe("reviewing");
      expect(task.exitCode).toBe(0);
      expect(task.completedAt).toBeGreaterThan(0);
      expect(statusEvents.some((e) => e.status === "reviewing")).toBe(true);
    });

    it("transitions Codex app-server turns to failed when done follows a fatal error", async () => {
      const task = await orchestrator.createTask("Test", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      mockAdapterInstances[0].simulateEvent({ type: "error", timestamp: 1000, error: "turn failed" });
      mockAdapterInstances[0].simulateEvent({ type: "done", timestamp: 1001, content: "" });
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
      reloaded = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(reloaded.getTask(task.id)?.status).toBe("running");

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

  describe("iterate", () => {
    it("waits for the pre-turn snapshot before calling a live adapter", async () => {
      const task = await orchestrator.createTask("Original", "claude-code", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      let release!: (checkpoint: { ref: string; sha: string }) => void;
      const snapshot = { ref: `refs/crab/checkpoints/${task.id}/1`, sha: "a".repeat(40) };
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "createCheckpoint")
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

  describe("applyTask", () => {
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

    it("clears the thread id when an error event indicates resume failed", async () => {
      const task = await orchestrator.createTask("Refactor", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);

      mockAdapterInstances[0].simulateEvent({
        type: "thread_started",
        timestamp: 1000,
        threadId: "thr-stale",
      });
      await vi.advanceTimersByTimeAsync(0);
      expect(task.codexThreadId).toBe("thr-stale");

      // Subsequent error referencing thread/session — orchestrator should
      // forget the threadId so the next iterate starts a fresh session.
      mockAdapterInstances[0].simulateEvent({
        type: "error",
        timestamp: 2000,
        error: "thread not found: thr-stale",
      });
      await vi.advanceTimersByTimeAsync(0);

      expect(task.codexThreadId).toBeUndefined();
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

    it("iterate falls back to the combined prompt when no thread id is known", async () => {
      const task = await orchestrator.createTask("Original", "codex", "/tmp/repo", "main");
      await orchestrator.startTask(task.id);
      mockAdapterInstances[0].simulateExit(0);
      await vi.advanceTimersByTimeAsync(0);

      await orchestrator.iterate(task.id, "Add a test");

      // No thread id → legacy wrapper kicks in so the model sees the
      // earlier prompt restated.
      expect(mockAdapterInstances[1].spawnPrompt).toContain("Previous task:");
      expect(mockAdapterInstances[1].spawnPrompt).toContain("Add a test");
    });
  });

  describe("approval decisions", () => {
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
