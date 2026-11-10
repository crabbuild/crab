/**
 * Tests for remote task spawning in the orchestrator.
 *
 * Verifies that when a task has `isRemote: true`, the orchestrator
 * sends a `task_spawn` RPC to the remote agent instead of spawning
 * a local process, and that events streaming back as JSON-RPC
 * notifications are forwarded to the renderer via the event system.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task, type OrchestratorOptions } from "../electron/orchestrator";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock the worktree module — no real git operations.
vi.mock("../electron/orchestrator-worktree", () => ({
  createWorktree: vi.fn(async (_repoPath: string, taskId: string, _baseRef: string) => {
    return path.join(os.tmpdir(), `crab-agent-${taskId}`);
  }),
  removeWorktree: vi.fn(async () => {}),
  readTaskMetadata: vi.fn(() => undefined),
}));

// Mock the adapters module.
vi.mock("../electron/orchestrator-adapters", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../electron/orchestrator-adapters")>();
  return {
    ...actual,
    createAdapter: vi.fn(() => {
      throw new Error("Should not create local adapter for remote tasks");
    }),
  };
});

// Mock child_process + util.
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
    // Extract profileId from ssh:// URL
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
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-remote-orch-test-"));
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
 * Creates a remote task directly in the orchestrator's internal state
 * (bypasses createTask which would try to route worktree creation).
 */
function injectRemoteTask(orchestrator: Orchestrator, overrides: Partial<Task> = {}): Task {
  const id = overrides.id ?? crypto.randomUUID();
  const task: Task = {
    id,
    prompt: "Fix the remote bug",
    agentType: "claude-code",
    status: "created",
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

  // Access the internal tasks map via the public API workaround:
  // We use getAllTasks to verify, but inject via the internal map.
  (orchestrator as any).tasks.set(id, task);
  return task;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Remote Task Spawning", () => {
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

  describe("startTask with remote task", () => {
    it("sends task_spawn RPC to the remote agent", async () => {
      const task = injectRemoteTask(orchestrator);

      await orchestrator.startTask(task.id);

      // The orchestrator first tries to load remote scripts config (read_text),
      // then sends task_spawn. read_text may fail (no config file) which is fine.
      const spawnCall = mockRouter.calls.find((c) => c.method === "task_spawn");
      expect(spawnCall).toBeDefined();
      expect(spawnCall).toEqual({
        profileId: "gpu-server",
        method: "task_spawn",
        params: {
          task_id: task.id,
          prompt: "Fix the remote bug",
          agent_type: "claude-code",
          worktree_path: task.worktreePath,
        },
      });
    });

    it("transitions task to running status", async () => {
      const task = injectRemoteTask(orchestrator);
      const events: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => events.push(e));

      await orchestrator.startTask(task.id);

      expect(task.status).toBe("running");
      expect(task.startedAt).toBeGreaterThan(0);
      expect(events).toContainEqual({ taskId: task.id, status: "running" });
    });

    it("does not spawn a local adapter for remote tasks", async () => {
      const task = injectRemoteTask(orchestrator);

      // If createAdapter were called, it would throw (see mock above).
      // This test verifies no error is thrown.
      await orchestrator.startTask(task.id);

      expect(task.status).toBe("running");
    });

    it("sets task to failed if task_spawn RPC fails", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.shouldFail = true;
      mockRouter.failError = "SSH connection lost";

      const events: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => events.push(e));
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await expect(orchestrator.startTask(task.id)).rejects.toThrow(
        "Failed to spawn remote task: SSH connection lost",
      );

      expect(task.status).toBe("failed");
      expect(task.completedAt).toBeGreaterThan(0);
    });

    it("throws if rpcRouter is not configured", async () => {
      const noRouterOrchestrator = new Orchestrator({ persistPath: createTempPersistPath() });
      const task = injectRemoteTask(noRouterOrchestrator);

      await expect(noRouterOrchestrator.startTask(task.id)).rejects.toThrow(
        "SSH router not configured",
      );
    });

    it("throws if remoteProfileId is missing", async () => {
      const task = injectRemoteTask(orchestrator, { remoteProfileId: undefined });

      await expect(orchestrator.startTask(task.id)).rejects.toThrow(
        "missing remoteProfileId",
      );
    });

    it("keeps a queued start pending until remote capacity is released", async () => {
      const capacityPath = createTempPersistPath();
      const orch = new Orchestrator({ persistPath: capacityPath, timeoutMs: 60_000, maxConcurrent: 1,
        rpcRouter: mockRouter as unknown as NonNullable<OrchestratorOptions["rpcRouter"]> });
      let starting: Promise<void> | undefined;
      try {
        const first = injectRemoteTask(orch, { id: "task-1" }); await orch.startTask(first.id);
        const second = injectRemoteTask(orch, { id: "task-2" }); let acknowledged = false;
        starting = orch.startTask(second.id).then(() => { acknowledged = true; }); void starting.catch(() => {});
        await vi.waitFor(() => expect(second.status).toBe("queued"));
        expect({ acknowledged, spawns: mockRouter.calls.filter(call => call.method === "task_spawn").length }).toEqual({ acknowledged: false, spawns: 1 });
        await orch.handleRemoteTaskNotification({ profileId: "gpu-server", method: "task_completed", params: { task_id: first.id, exit_code: 0 } });
        await starting;
        expect({ acknowledged, status: second.status, spawns: mockRouter.calls.filter(call => call.method === "task_spawn").length }).toEqual({ acknowledged: true, status: "running", spawns: 2 });
      } finally { await orch.shutdown(); await starting?.catch(() => {}); cleanupPath(capacityPath); }
    });
  });

  describe("handleRemoteTaskNotification", () => {
    it("forwards task_event notifications to the event system", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: {
            type: "text",
            timestamp: 1000,
            content: "Working on the fix...",
          },
        },
      });

      expect(events).toHaveLength(1);
      expect(events[0].taskId).toBe(task.id);
      expect(events[0].event.type).toBe("text");
      expect(events[0].event.content).toBe("Working on the fix...");
    });

    it("appends events to the task's event array", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: {
            type: "file_edit",
            timestamp: 2000,
            path: "src/main.rs",
            diff: "+new line",
          },
        },
      });

      expect(task.events).toHaveLength(1);
      expect(task.events[0].type).toBe("file_edit");
      expect(task.events[0].path).toBe("src/main.rs");
    });

    it("handles task_completed notification", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_completed",
        params: {
          task_id: task.id,
          exit_code: 0,
        },
      });

      expect(task.status).toBe("reviewing");
      expect(task.exitCode).toBe(0);
      expect(task.completedAt).toBeGreaterThan(0);
      expect(statusEvents).toContainEqual({ taskId: task.id, status: "reviewing" });
    });

    it("handles task_completed with non-zero exit code as failed", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_completed",
        params: {
          task_id: task.id,
          exit_code: 1,
        },
      });

      expect(task.status).toBe("failed");
      expect(task.exitCode).toBe(1);
    });

    it("handles task_failed notification", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_failed",
        params: {
          task_id: task.id,
          exit_code: 137,
          error: "Agent process killed by OOM",
        },
      });

      expect(task.status).toBe("failed");
      expect(task.exitCode).toBe(137);
      // Should emit an error event.
      expect(events).toHaveLength(1);
      expect(events[0].event.type).toBe("error");
      expect(events[0].event.error).toBe("Agent process killed by OOM");
    });

    it("ignores notifications for unknown task IDs", async () => {
      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: "nonexistent-task-id",
          event: { type: "text", timestamp: 1000, content: "hello" },
        },
      });

      expect(events).toHaveLength(0);
    });

    it("ignores notifications for non-remote tasks", async () => {
      const task = injectRemoteTask(orchestrator, { isRemote: false });
      task.status = "running";

      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "text", timestamp: 1000, content: "hello" },
        },
      });

      expect(events).toHaveLength(0);
    });

    it("adds timestamp to events that lack one", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "text", content: "no timestamp" },
        },
      });

      expect(task.events[0].timestamp).toBeGreaterThan(0);
    });
  });

  describe("cancelTask with remote task", () => {
    it("sends task_cancel RPC to the remote agent", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      await orchestrator.cancelTask(task.id);

      const cancelCall = mockRouter.calls.find((c) => c.method === "task_cancel");
      expect(cancelCall).toBeDefined();
      expect(cancelCall!.params).toEqual({ task_id: task.id });
      expect(task.status).toBe("stopped");
    });

    it("still marks task as stopped even if cancel RPC fails", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";
      mockRouter.shouldFail = true;

      await orchestrator.cancelTask(task.id);

      expect(task.status).toBe("stopped");
      expect(task.completedAt).toBeGreaterThan(0);
    });
  });

  describe("remote task event streaming", () => {
    it("streams multiple events in sequence", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      // Simulate a sequence of events from the remote agent.
      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "thinking", timestamp: 1000, content: "Analyzing the code..." },
        },
      });

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "file_edit", timestamp: 2000, path: "src/lib.rs", diff: "+fix" },
        },
      });

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: { type: "command", timestamp: 3000, command: "cargo test" },
        },
      });

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_completed",
        params: { task_id: task.id, exit_code: 0 },
      });

      expect(events).toHaveLength(3);
      expect(events[0].event.type).toBe("thinking");
      expect(events[1].event.type).toBe("file_edit");
      expect(events[2].event.type).toBe("command");
      expect(task.status).toBe("reviewing");
      expect(task.events).toHaveLength(3);
    });

    it("renderer sees remote events identically to local events", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });
      task.status = "running";

      const events: any[] = [];
      orchestrator.on("orchestrator:event", (e) => events.push(e));

      await orchestrator.handleRemoteTaskNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: {
          task_id: task.id,
          event: {
            type: "file_edit",
            timestamp: 5000,
            path: "src/main.rs",
            diff: "+new code",
            tool: "Write",
          },
        },
      });

      // The event payload matches the same shape as local events.
      const emitted = events[0];
      expect(emitted.taskId).toBe(task.id);
      expect(emitted.event.type).toBe("file_edit");
      expect(emitted.event.path).toBe("src/main.rs");
      expect(emitted.event.diff).toBe("+new code");
      expect(emitted.event.tool).toBe("Write");
      expect(emitted.event.timestamp).toBe(5000);
    });
  });
});
