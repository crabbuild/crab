/**
 * Tests for SSH connection recovery — handling remote task interruption
 * when SSH connections drop and recovery on reconnection.
 *
 * Verifies:
 * - Running remote tasks are marked "interrupted" on disconnection
 * - On reconnection, orphaned tasks are detected via RPC
 * - Worktree exists + agent running → re-attach
 * - Worktree exists + agent stopped → emit recovery prompt
 * - Worktree gone → mark task as failed
 * - Resume and Discard actions work correctly
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task } from "../electron/orchestrator";
import { SshConnectionRecovery, RecoveryPrompt } from "../electron/ssh-connection-recovery";

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

// ─── Mock SSH Session Manager ────────────────────────────────────────────────

class MockSshSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
    this.emit("ssh:status-changed", { profileId, status });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(_profileId: string): unknown {
    return { channels: { agent: {} } };
  }
}

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
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createTempPersistPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-recovery-test-"));
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

function injectRemoteTask(orchestrator: Orchestrator, overrides: Partial<Task> = {}): Task {
  const id = overrides.id ?? crypto.randomUUID();
  const task: Task = {
    id,
    prompt: "Fix the remote bug",
    agentType: "claude-code",
    status: "running",
    branch: `agent/${id.slice(0, 8)}`,
    worktreePath: `/home/user/worktrees/agent-${id.slice(0, 8)}`,
    baseRef: "abc123",
    repoPath: "ssh://gpu-server/home/user/repos/ml-project",
    createdAt: Date.now(),
    startedAt: Date.now(),
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

describe("SSH Connection Recovery", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;
  let mockRouter: MockRpcRouter;
  let mockSessionManager: MockSshSessionManager;
  let recovery: SshConnectionRecovery;

  beforeEach(() => {
    persistPath = createTempPersistPath();
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(path.dirname(persistPath), "settings.json"));
    mockRouter = new MockRpcRouter();
    mockSessionManager = new MockSshSessionManager();
    orchestrator = new Orchestrator({
      persistPath,
      timeoutMs: 60_000,
      rpcRouter: mockRouter as any,
    });
    recovery = new SshConnectionRecovery(
      mockSessionManager as any,
      orchestrator,
      mockRouter as any,
    );
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    cleanupPath(persistPath);
  });

  describe("handleDisconnection", () => {
    it("marks running remote tasks as interrupted on disconnection", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      await recovery.handleDisconnection("gpu-server");

      expect(task.status).toBe("interrupted");
      expect(statusEvents).toContainEqual({
        taskId: task.id,
        status: "interrupted",
      });
    });

    it("marks iterating remote tasks as interrupted", async () => {
      const task = injectRemoteTask(orchestrator, { status: "iterating" });

      await recovery.handleDisconnection("gpu-server");

      expect(task.status).toBe("interrupted");
    });

    it("marks all affected tasks before persistence can yield to reconnect", async () => {
      const first = injectRemoteTask(orchestrator, { status: "running" });
      const second = injectRemoteTask(orchestrator, { status: "iterating" });
      let finish!: () => void;
      vi.spyOn(orchestrator, "persistTasks").mockReturnValueOnce(new Promise<void>(resolve => { finish = resolve; }));
      const events: Array<{ taskId: string; status: string }> = [];
      orchestrator.on("orchestrator:task-status", payload => events.push(payload));
      const interrupted = recovery.handleDisconnection("gpu-server");
      const statuses = [first.status, second.status];
      second.status = "reviewing"; finish(); await interrupted;
      expect({ statuses, events }).toEqual({ statuses: ["interrupted", "interrupted"], events: [{ taskId: first.id, status: "interrupted" }] });
    });

    it("does not affect tasks in other statuses", async () => {
      const reviewingTask = injectRemoteTask(orchestrator, { status: "reviewing" });
      const failedTask = injectRemoteTask(orchestrator, { status: "failed" });
      const createdTask = injectRemoteTask(orchestrator, { status: "created" });

      await recovery.handleDisconnection("gpu-server");

      expect(reviewingTask.status).toBe("reviewing");
      expect(failedTask.status).toBe("failed");
      expect(createdTask.status).toBe("created");
    });

    it("does not affect tasks for other profiles", async () => {
      const task = injectRemoteTask(orchestrator, {
        status: "running",
        remoteProfileId: "other-server",
      });

      await recovery.handleDisconnection("gpu-server");

      expect(task.status).toBe("running");
    });

    it("does not affect local tasks", async () => {
      const localTask: Task = {
        id: crypto.randomUUID(),
        prompt: "Local task",
        agentType: "claude-code",
        status: "running",
        branch: "agent/local",
        worktreePath: "/tmp/local-worktree",
        baseRef: "abc123",
        repoPath: "/home/user/repos/local",
        createdAt: Date.now(),
        events: [],
        feedback: [],
        todos: [],
        isRemote: false,
      };
      (orchestrator as any).tasks.set(localTask.id, localTask);

      await recovery.handleDisconnection("gpu-server");

      expect(localTask.status).toBe("running");
    });

    it("handles multiple running tasks for the same profile", async () => {
      const task1 = injectRemoteTask(orchestrator, { status: "running" });
      const task2 = injectRemoteTask(orchestrator, { status: "running" });

      await recovery.handleDisconnection("gpu-server");

      expect(task1.status).toBe("interrupted");
      expect(task2.status).toBe("interrupted");
    });
  });

  describe("handleReconnection", () => {
    it("sends task_check_orphaned RPC for interrupted tasks", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.nextResult = {
        taskId: task.id,
        worktreePath: task.worktreePath,
        branch: task.branch,
        worktreeExists: true,
        agentRunning: false,
      };

      await recovery.handleReconnection("gpu-server");

      expect(mockRouter.calls).toHaveLength(1);
      expect(mockRouter.calls[0]).toEqual({
        profileId: "gpu-server",
        method: "task_check_orphaned",
        params: {
          task_id: task.id,
          worktree_path: task.worktreePath,
          branch: task.branch,
        },
      });
    });

    it("re-attaches task when agent is still running", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.nextResult = {
        taskId: task.id,
        worktreePath: task.worktreePath,
        branch: task.branch,
        worktreeExists: true,
        agentRunning: true,
      };

      const reattachEvents: any[] = [];
      recovery.on("ssh:task-reattached", (e) => reattachEvents.push(e));

      await recovery.handleReconnection("gpu-server");

      expect(task.status).toBe("running");
      expect(reattachEvents).toHaveLength(1);
      expect(reattachEvents[0]).toEqual({
        taskId: task.id,
        profileId: "gpu-server",
      });
    });

    it("emits recovery prompt when worktree exists but agent stopped", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.nextResult = {
        taskId: task.id,
        worktreePath: task.worktreePath,
        branch: task.branch,
        worktreeExists: true,
        agentRunning: false,
      };

      const prompts: RecoveryPrompt[] = [];
      recovery.on("ssh:task-recovery-prompt", (p) => prompts.push(p));

      await recovery.handleReconnection("gpu-server");

      expect(prompts).toHaveLength(1);
      expect(prompts[0].taskId).toBe(task.id);
      expect(prompts[0].profileId).toBe("gpu-server");
      expect(prompts[0].options).toEqual(["resume", "discard"]);
      expect(prompts[0].worktreePath).toBe(task.worktreePath);
      // Task stays interrupted until user decides.
      expect(task.status).toBe("interrupted");
    });

    it("marks task as failed when worktree is gone", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.nextResult = {
        taskId: task.id,
        worktreePath: task.worktreePath,
        branch: task.branch,
        worktreeExists: false,
        agentRunning: false,
      };

      const lostEvents: any[] = [];
      recovery.on("ssh:task-worktree-lost", (e) => lostEvents.push(e));

      const statusEvents: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      await recovery.handleReconnection("gpu-server");

      expect(task.status).toBe("failed");
      expect(task.completedAt).toBeGreaterThan(0);
      expect(lostEvents).toHaveLength(1);
      expect(statusEvents).toContainEqual({
        taskId: task.id,
        status: "failed",
      });
    });

    it("keeps task as interrupted if RPC fails", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.shouldFail = true;
      mockRouter.failError = "Agent not ready";

      await recovery.handleReconnection("gpu-server");

      expect(task.status).toBe("interrupted");
    });

    it("does nothing if no interrupted tasks exist", async () => {
      injectRemoteTask(orchestrator, { status: "running" });
      injectRemoteTask(orchestrator, { status: "reviewing" });

      await recovery.handleReconnection("gpu-server");

      expect(mockRouter.calls).toHaveLength(0);
    });

    it("handles multiple interrupted tasks independently", async () => {
      const task1 = injectRemoteTask(orchestrator, { status: "interrupted" });
      const task2 = injectRemoteTask(orchestrator, { status: "interrupted" });

      // First call: worktree exists, agent running (re-attach)
      // Second call: worktree gone (mark failed)
      let callCount = 0;
      mockRouter.sendToRemoteAgent = vi.fn(async () => {
        callCount++;
        if (callCount === 1) {
          return {
            taskId: task1.id,
            worktreeExists: true,
            agentRunning: true,
          };
        }
        return {
          taskId: task2.id,
          worktreeExists: false,
          agentRunning: false,
        };
      });

      await recovery.handleReconnection("gpu-server");

      expect(task1.status).toBe("running");
      expect(task2.status).toBe("failed");
    });
  });

  describe("event-driven disconnection/reconnection", () => {
    it.each(["disconnected", "reconnecting"])("interrupts tasks when ssh:status-changed emits %s", async status => {
      const task = injectRemoteTask(orchestrator, { status: "running" });

      // Simulate disconnection event
      mockSessionManager.setStatus("gpu-server", status);

      // Allow async handler to complete
      await new Promise((resolve) => setTimeout(resolve, 10));

      expect(task.status).toBe("interrupted");
    });

    it("interrupts tasks when ssh:status-changed emits error", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });

      mockSessionManager.setStatus("gpu-server", "error");

      await new Promise((resolve) => setTimeout(resolve, 10));

      expect(task.status).toBe("interrupted");
    });

    it("checks orphaned tasks when ssh:status-changed emits connected", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.nextResult = {
        taskId: task.id,
        worktreeExists: true,
        agentRunning: true,
      };

      mockSessionManager.setStatus("gpu-server", "connected");

      await new Promise((resolve) => setTimeout(resolve, 10));

      expect(task.status).toBe("running");
    });
  });

  describe("resumeTask", () => {
    it("resumes through the shared lifecycle with its saved workspace and native session", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted", claudeSessionId: "saved-native-session" });
      const workspace = task.worktreePath;
      let admittedStatus: string | undefined;
      const start = vi.spyOn(orchestrator, "startTask").mockImplementation(async id => { admittedStatus = orchestrator.getTask(id)?.status; });
      try {
        await recovery.resumeTask(task.id);
        const saved = new Orchestrator({ persistPath }).getTask(task.id);
        expect({ calls: start.mock.calls, admittedStatus, workspace: saved?.worktreePath, session: saved?.claudeSessionId, persistedStatus: saved?.status, remoteCalls: mockRouter.calls }).toEqual({
          calls: [[task.id]], admittedStatus: "created", workspace, session: "saved-native-session", persistedStatus: "created", remoteCalls: [],
        });
      } finally { start.mockRestore(); }
    });

    it("throws if task is not in interrupted status", async () => {
      const task = injectRemoteTask(orchestrator, { status: "running" });

      await expect(recovery.resumeTask(task.id)).rejects.toThrow(
        'Cannot resume task in status "running"',
      );
    });

    it("throws if task does not exist", async () => {
      await expect(recovery.resumeTask("nonexistent")).rejects.toThrow(
        "Task not found",
      );
    });
  });

  describe("discardTask", () => {
    it("removes the worktree on the remote and marks task discarded", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });
      mockRouter.nextResult = { discarded: true, worktree_removed: true, branch_deleted: true };

      await recovery.discardTask(task.id);

      expect(task.status).toBe("discarded");
      // Should have sent task_discard RPC
      const discardCall = mockRouter.calls.find((c) => c.method === "task_discard");
      expect(discardCall).toBeDefined();
    });

    it("throws if task is not in interrupted status", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });

      await expect(recovery.discardTask(task.id)).rejects.toThrow(
        'Cannot discard task in status "reviewing"',
      );
    });
  });

  describe("TaskStatus includes interrupted", () => {
    it("interrupted status is persisted correctly", async () => {
      const task = injectRemoteTask(orchestrator, { status: "interrupted" });

      await orchestrator.persistTasks();

      const reloaded = new Orchestrator({ persistPath, timeoutMs: 60_000 });
      const persisted = reloaded.getTask(task.id);
      expect(persisted).toBeDefined();
      expect(persisted!.status).toBe("interrupted");
    });
  });
});
