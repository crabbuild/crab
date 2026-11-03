/**
 * Tests for remote task iteration in the orchestrator.
 *
 * Verifies that when a remote task is iterated (user sends feedback),
 * the orchestrator sends a `task_iterate` RPC to the remote agent
 * instead of re-spawning locally. The remote agent re-spawns the
 * agent CLI in the same worktree with the feedback as context.
 * Events stream back the same way as the initial spawn.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task } from "../electron/orchestrator";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";

// ─── Mocks ───────────────────────────────────────────────────────────────────

vi.mock("../electron/orchestrator-worktree", () => ({
  createWorktree: vi.fn(async (_repoPath: string, taskId: string, _baseRef: string) => {
    return path.join(os.tmpdir(), `crab-agent-${taskId}`);
  }),
  removeWorktree: vi.fn(async () => {}),
  readTaskMetadata: vi.fn(() => undefined),
}));

vi.mock("../electron/orchestrator-adapters", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../electron/orchestrator-adapters")>();
  return {
    ...actual,
    createAdapter: vi.fn(() => {
      throw new Error("Should not create local adapter for remote tasks");
    }),
  };
});

vi.mock("node:child_process", () => ({
  execFile: vi.fn(),
}));

vi.mock("node:util", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:util")>();
  return {
    ...actual,
    promisify: vi.fn(() => {
      return async (...args: any[]) => {
        const cmdArgs = args[1] as string[];
        if (cmdArgs.includes("rev-parse")) {
          return { stdout: "abc123def456789\n", stderr: "" };
        }
        return { stdout: "", stderr: "" };
      };
    }),
  };
});

// ─── Mock RPC Router ─────────────────────────────────────────────────────────

class MockRpcRouter extends EventEmitter {
  calls: Array<{ profileId: string; method: string; params: unknown }> = [];
  nextResult: unknown = {};
  shouldFail = false;
  failError = "Connection lost";

  async sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    this.calls.push({ profileId, method, params });
    if (this.shouldFail) {
      throw new Error(this.failError);
    }
    return this.nextResult;
  }

  async route(repoId: string, method: string, params: unknown): Promise<unknown> {
    const withoutScheme = repoId.slice("ssh://".length);
    const slashIdx = withoutScheme.indexOf("/");
    const profileId = slashIdx >= 0 ? withoutScheme.slice(0, slashIdx) : withoutScheme;
    return this.sendToRemoteAgent(profileId, method, params);
  }

  getStatus(_profileId: string): string {
    return "connected";
  }

  getConnection(_profileId: string): unknown {
    return { channels: { agent: {} } };
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createTempPersistPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-remote-iterate-test-"));
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

/**
 * Injects a remote task directly into the orchestrator's internal state.
 */
function injectRemoteTask(orchestrator: Orchestrator, overrides: Partial<Task> = {}): Task {
  const id = overrides.id ?? crypto.randomUUID();
  const task: Task = {
    id,
    prompt: "Implement the feature",
    agentType: "claude-code",
    status: "reviewing",
    branch: `agent/${id.slice(0, 8)}`,
    worktreePath: `/home/user/worktrees/agent-${id.slice(0, 8)}`,
    baseRef: "abc123",
    repoPath: "ssh://gpu-server/home/user/repos/ml-project",
    createdAt: Date.now(),
    events: [],
    feedback: [],
    todos: [],
    isRemote: true,
    remoteProfileId: "gpu-server",
    ...overrides,
  };

  (orchestrator as any).tasks.set(id, task);
  return task;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Remote Task Iteration", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;
  let mockRouter: MockRpcRouter;

  beforeEach(() => {
    vi.useFakeTimers();
    persistPath = createTempPersistPath();
    mockRouter = new MockRpcRouter();
    orchestrator = new Orchestrator({
      persistPath,
      timeoutMs: 60_000,
      rpcRouter: mockRouter as any,
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    cleanupPath(persistPath);
  });

  describe("iterate() with remote task", () => {
    it("sends task_iterate RPC to the remote agent with feedback", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      await orchestrator.iterate(task.id, "Please also add error handling");

      const iterateCall = mockRouter.calls.find((c) => c.method === "task_iterate");
      expect(iterateCall).toBeDefined();
      expect(iterateCall!.profileId).toBe("gpu-server");
      expect(iterateCall!.params).toEqual({
        task_id: task.id,
        feedback: "Please also add error handling",
        agent_type: "claude-code",
        worktree_path: task.worktreePath,
      });
    });

    it("does NOT send task_spawn for remote iteration", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      await orchestrator.iterate(task.id, "Fix the typo");

      const spawnCalls = mockRouter.calls.filter((c) => c.method === "task_spawn");
      expect(spawnCalls).toHaveLength(0);
    });

    it("transitions through iterating → running status", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      await orchestrator.iterate(task.id, "Add tests");

      // Should have emitted iterating then running.
      expect(statusEvents).toContainEqual({ taskId: task.id, status: "iterating" });
      expect(statusEvents).toContainEqual({ taskId: task.id, status: "running" });
      expect(task.status).toBe("running");
    });

    it("stores feedback in the task's feedback array", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      await orchestrator.iterate(task.id, "First feedback");
      // Simulate task completing and going back to reviewing.
      task.status = "reviewing";
      await orchestrator.iterate(task.id, "Second feedback");

      expect(task.feedback).toEqual(["First feedback", "Second feedback"]);
    });

    it("increments turn number on each iteration", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing", turnNumber: 0 });

      await orchestrator.iterate(task.id, "Iteration 1");
      expect(task.turnNumber).toBe(1);

      // Simulate completion and re-iterate.
      task.status = "reviewing";
      await orchestrator.iterate(task.id, "Iteration 2");
      expect(task.turnNumber).toBe(2);
    });

    it("emits checkpoint-created event before iteration", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      const checkpointEvents: any[] = [];
      orchestrator.on("orchestrator:checkpoint-created", (e) => checkpointEvents.push(e));

      await orchestrator.iterate(task.id, "Add logging");

      expect(checkpointEvents).toHaveLength(1);
      expect(checkpointEvents[0].taskId).toBe(task.id);
      expect(checkpointEvents[0].turnNumber).toBe(1);
    });

    it("waits for the checkpoint before sending remote iteration work", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      let release!: (checkpoint: { ref: string; sha: string }) => void;
      const snapshot = { ref: `refs/crab/checkpoints/${task.id}/1`, sha: "a".repeat(40) };
      const checkpoint = vi.spyOn(CheckpointManager.prototype, "createCheckpoint")
        .mockReturnValueOnce(new Promise(resolve => { release = resolve; }));
      const iteration = orchestrator.iterate(task.id, "Next turn");
      try {
        await vi.advanceTimersByTimeAsync(0);
        expect(mockRouter.calls.filter(call => call.method === "task_iterate")).toEqual([]);
        release(snapshot);
        await iteration;
        expect(mockRouter.calls.filter(call => call.method === "task_iterate")).toHaveLength(1);
      } finally {
        release(snapshot);
        await iteration;
        checkpoint.mockRestore();
      }
    });

    it("sets task to failed if task_iterate RPC fails", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      mockRouter.shouldFail = true;
      mockRouter.failError = "SSH connection dropped";

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      await expect(orchestrator.iterate(task.id, "Fix it")).rejects.toThrow(
        "Failed to iterate remote task: SSH connection dropped",
      );

      expect(task.status).toBe("failed");
      expect(task.completedAt).toBeGreaterThan(0);
    });

    it("emits error event when task_iterate RPC fails", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      mockRouter.shouldFail = true;
      mockRouter.failError = "Timeout";

      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await expect(orchestrator.iterate(task.id, "Fix it")).rejects.toThrow();

      const errorEvent = events.find((e) => e.event?.type === "error");
      expect(errorEvent).toBeDefined();
      expect(errorEvent.event.error).toContain("Remote task iteration failed");
    });

    it("throws if rpcRouter is not configured for remote iteration", async () => {
      const noRouterOrchestrator = new Orchestrator({ persistPath: createTempPersistPath() });
      const task = injectRemoteTask(noRouterOrchestrator, { status: "reviewing" });

      await expect(noRouterOrchestrator.iterate(task.id, "Fix it")).rejects.toThrow(
        "SSH router not configured",
      );
    });

    it("throws if remoteProfileId is missing for remote iteration", async () => {
      const task = injectRemoteTask(orchestrator, {
        status: "reviewing",
        remoteProfileId: undefined,
      });

      await expect(orchestrator.iterate(task.id, "Fix it")).rejects.toThrow(
        "missing remoteProfileId",
      );
    });

    it("emits iteration-warning after 5 iterations", async () => {
      const task = injectRemoteTask(orchestrator, {
        status: "reviewing",
        feedback: ["f1", "f2", "f3", "f4"],
      });

      const warnings: any[] = [];
      orchestrator.on("orchestrator:iteration-warning", (e) => warnings.push(e));

      await orchestrator.iterate(task.id, "Fifth feedback");

      expect(warnings).toHaveLength(1);
      expect(warnings[0].taskId).toBe(task.id);
      expect(warnings[0].iterations).toBe(5);
    });

    it("does not modify the task prompt for remote iteration", async () => {
      const task = injectRemoteTask(orchestrator, {
        status: "reviewing",
        prompt: "Original prompt",
      });

      await orchestrator.iterate(task.id, "Some feedback");

      // For remote tasks, the prompt should NOT be modified — the
      // feedback is sent separately via the task_iterate RPC.
      expect(task.prompt).toBe("Original prompt");
    });
  });

  describe("remote iteration event streaming", () => {
    it("streams events back during iteration same as initial spawn", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      await orchestrator.iterate(task.id, "Add error handling");

      // Simulate events streaming back from the remote agent.
      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "text", timestamp: 1000, content: "Adding error handling..." },
        },
      });

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "file_edit", timestamp: 2000, path: "src/lib.rs", diff: "+.map_err(|e| ...)" },
        },
      });

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_completed",
        params: { task_id: task.id, exit_code: 0 },
      });

      expect(events).toHaveLength(2);
      expect(events[0].event.type).toBe("text");
      expect(events[1].event.type).toBe("file_edit");
      expect(task.status).toBe("reviewing");
      expect(task.events).toHaveLength(2);
    });

    it("supports multiple iteration cycles", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      // First iteration.
      await orchestrator.iterate(task.id, "Add tests");
      expect(task.status).toBe("running");

      // Simulate completion.
      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_completed",
        params: { task_id: task.id, exit_code: 0 },
      });
      expect(task.status).toBe("reviewing");

      // Second iteration.
      await orchestrator.iterate(task.id, "Also add docs");
      expect(task.status).toBe("running");

      // Verify both iterate RPCs were sent.
      const iterateCalls = mockRouter.calls.filter((c) => c.method === "task_iterate");
      expect(iterateCalls).toHaveLength(2);
      expect((iterateCalls[0].params as any).feedback).toBe("Add tests");
      expect((iterateCalls[1].params as any).feedback).toBe("Also add docs");
    });

    it("handles iteration failure from remote agent", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      await orchestrator.iterate(task.id, "Refactor this");

      // Simulate the remote agent failing during iteration.
      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_failed",
        params: {
          task_id: task.id,
          exit_code: 1,
          error: "Agent crashed during iteration",
        },
      });

      expect(task.status).toBe("failed");
      expect(task.exitCode).toBe(1);
    });
  });

  describe("local task iteration still works", () => {
    it("does not send task_iterate RPC for local tasks", async () => {
      // Inject a local task (isRemote = false).
      const id = crypto.randomUUID();
      const task: Task = {
        id,
        prompt: "Fix the local bug",
        agentType: "claude-code",
        status: "reviewing",
        branch: `agent/${id.slice(0, 8)}`,
        worktreePath: path.join(os.tmpdir(), `crab-agent-${id}`),
        baseRef: "abc123",
        repoPath: "/home/user/repos/local-project",
        createdAt: Date.now(),
        events: [],
        feedback: [],
        todos: [],
        isRemote: false,
      };
      (orchestrator as any).tasks.set(id, task);

      // Make the worktree path exist so startTask doesn't fail.
      fs.mkdirSync(task.worktreePath, { recursive: true });

      // For local tasks, iterate() calls startTask() which tries to
      // create a local adapter. Since we mocked createAdapter to throw,
      // we expect an error — but the key assertion is that no
      // task_iterate RPC was sent.
      try {
        await orchestrator.iterate(id, "Local feedback");
      } catch {
        // Expected — local adapter mock throws.
      }

      const iterateCalls = mockRouter.calls.filter((c) => c.method === "task_iterate");
      expect(iterateCalls).toHaveLength(0);

      // Cleanup.
      fs.rmSync(task.worktreePath, { recursive: true, force: true });
    });
  });
});
