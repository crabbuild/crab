/**
 * Tests for remote apply/discard in the orchestrator.
 *
 * Verifies that when a task has `isRemote: true`, the orchestrator
 * routes apply and discard operations to the remote agent via
 * `task_apply` and `task_discard` RPCs.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task } from "../electron/orchestrator";

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
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-remote-apply-test-"));
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
    prompt: "Implement the feature",
    agentType: "claude-code",
    status: "reviewing",
    branch: `agent/${id.slice(0, 8)}`,
    worktreePath: `/home/user/worktrees/agent-${id.slice(0, 8)}`,
    baseRef: "abc123def456",
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

function injectLocalTask(orchestrator: Orchestrator, overrides: Partial<Task> = {}): Task {
  const id = overrides.id ?? crypto.randomUUID();
  const task: Task = {
    id,
    prompt: "Local task",
    agentType: "claude-code",
    status: "reviewing",
    branch: `agent/${id.slice(0, 8)}`,
    worktreePath: path.join(os.tmpdir(), `crab-agent-${id.slice(0, 8)}`),
    baseRef: "abc123def456",
    repoPath: "/home/user/repos/local-project",
    createdAt: Date.now(),
    events: [],
    feedback: [],
    todos: [],
    ...overrides,
  };

  (orchestrator as any).tasks.set(id, task);
  return task;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Remote Apply/Discard", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;
  let mockRouter: MockRpcRouter;

  beforeEach(() => {
    persistPath = createTempPersistPath();
    mockRouter = new MockRpcRouter();
    orchestrator = new Orchestrator({
      persistPath,
      timeoutMs: 60_000,
      rpcRouter: mockRouter as any,
    });
  });

  afterEach(() => {
    cleanupPath(persistPath);
  });

  describe("applyTask (remote)", () => {
    it("sends task_apply RPC to the remote agent", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = {
        applied: true,
        target_branch: "main",
        worktree_removed: true,
        branch_deleted: true,
      };

      await orchestrator.applyTask(task.id);

      expect(mockRouter.calls).toHaveLength(1);
      expect(mockRouter.calls[0]).toEqual({
        profileId: "gpu-server",
        method: "task_apply",
        params: {
          repo_id: task.repoPath,
          task_id: task.id,
          branch: task.branch,
          base_ref: task.baseRef,
          worktree_path: task.worktreePath,
        },
      });
    });

    it("updates task status to applied on success", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = { applied: true };

      await orchestrator.applyTask(task.id);

      expect(task.status).toBe("applied");
      expect(task.completedAt).toBeDefined();
    });

    it("emits orchestrator:task-status event on apply", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.nextResult = { applied: true };

      const events: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => events.push(e));

      await orchestrator.applyTask(task.id);

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ taskId: task.id, status: "applied" });
    });

    it("throws if the remote RPC fails", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.shouldFail = true;
      mockRouter.failError = "Merge conflict on remote";

      await expect(orchestrator.applyTask(task.id)).rejects.toThrow(
        "Remote apply failed: Merge conflict on remote",
      );

      // Status should NOT be updated on failure.
      expect(task.status).toBe("reviewing");
    });

    it("throws if task is not found", async () => {
      await expect(orchestrator.applyTask("nonexistent-id")).rejects.toThrow(
        "Task not found: nonexistent-id",
      );
    });

    it("does not send RPC for local tasks", async () => {
      const task = injectLocalTask(orchestrator);

      await orchestrator.applyTask(task.id);

      expect(mockRouter.calls).toHaveLength(0);
      expect(task.status).toBe("applied");
    });
  });

  describe("discardTask (remote)", () => {
    it("sends task_discard RPC to the remote agent", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = {
        discarded: true,
        worktree_removed: true,
        branch_deleted: true,
      };

      await orchestrator.discardTask(task.id);

      expect(mockRouter.calls).toHaveLength(1);
      expect(mockRouter.calls[0]).toEqual({
        profileId: "gpu-server",
        method: "task_discard",
        params: {
          repo_id: task.repoPath,
          task_id: task.id,
          branch: task.branch,
          worktree_path: task.worktreePath,
        },
      });
    });

    it("updates task status to discarded on success", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.nextResult = { discarded: true };

      await orchestrator.discardTask(task.id);

      expect(task.status).toBe("discarded");
      expect(task.completedAt).toBeDefined();
    });

    it("emits orchestrator:task-status event on discard", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.nextResult = { discarded: true };

      const events: any[] = [];
      orchestrator.on("orchestrator:task-status", (e) => events.push(e));

      await orchestrator.discardTask(task.id);

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ taskId: task.id, status: "discarded" });
    });

    it("succeeds even if the remote RPC fails (best-effort)", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.shouldFail = true;
      mockRouter.failError = "SSH connection lost";

      // Discard should NOT throw — it's best-effort for cleanup.
      await orchestrator.discardTask(task.id);

      expect(task.status).toBe("discarded");
    });

    it("throws if task is not found", async () => {
      await expect(orchestrator.discardTask("nonexistent-id")).rejects.toThrow(
        "Task not found: nonexistent-id",
      );
    });

    it("does not send RPC for local tasks", async () => {
      const task = injectLocalTask(orchestrator);

      await orchestrator.discardTask(task.id);

      expect(mockRouter.calls).toHaveLength(0);
      expect(task.status).toBe("discarded");
    });

    it("does not call removeWorktree for remote tasks", async () => {
      const { removeWorktree } = await import("../electron/orchestrator-worktree");
      vi.mocked(removeWorktree).mockClear();

      const task = injectRemoteTask(orchestrator);
      mockRouter.nextResult = { discarded: true };

      await orchestrator.discardTask(task.id);

      expect(removeWorktree).not.toHaveBeenCalled();
    });
  });

  describe("RemoteOrchestrator apply/discard", () => {
    it("applyTask sends correct RPC via RemoteOrchestrator", async () => {
      const { RemoteOrchestrator } = await import("../electron/ssh-orchestrator-remote");

      const remoteOrch = new RemoteOrchestrator(mockRouter as any, {} as any);

      // Create a task.
      mockRouter.nextResult = { path: "/home/user/worktrees/task-abc" };
      const task = await remoteOrch.createTask({
        repoId: "ssh://gpu-server/home/user/repos/project",
        prompt: "Fix the bug",
        agentType: "claude-code",
        baseRef: "main",
      });

      // Apply the task.
      mockRouter.calls = [];
      mockRouter.nextResult = { applied: true };

      await remoteOrch.applyTask(task.id);

      const applyCall = mockRouter.calls.find((c) => c.method === "task_apply");
      expect(applyCall).toBeDefined();
      expect(applyCall!.profileId).toBe("gpu-server");
      expect(applyCall!.params).toMatchObject({
        repo_id: task.repoId,
        task_id: task.id,
        branch: task.branch,
        base_ref: task.baseRef,
      });

      expect(task.status).toBe("applied");
      expect(task.completedAt).toBeDefined();
    });

    it("discardTask sends correct RPC via RemoteOrchestrator", async () => {
      const { RemoteOrchestrator } = await import("../electron/ssh-orchestrator-remote");

      const remoteOrch = new RemoteOrchestrator(mockRouter as any, {} as any);

      // Create a task.
      mockRouter.nextResult = { path: "/home/user/worktrees/task-xyz" };
      const task = await remoteOrch.createTask({
        repoId: "ssh://gpu-server/home/user/repos/project",
        prompt: "Add feature",
        agentType: "codex",
        baseRef: "develop",
      });

      // Discard the task.
      mockRouter.calls = [];
      mockRouter.nextResult = { discarded: true };

      await remoteOrch.discardTask(task.id);

      const discardCall = mockRouter.calls.find((c) => c.method === "task_discard");
      expect(discardCall).toBeDefined();
      expect(discardCall!.profileId).toBe("gpu-server");
      expect(discardCall!.params).toMatchObject({
        repo_id: task.repoId,
        task_id: task.id,
        branch: task.branch,
        worktree_path: task.worktreePath,
      });

      expect(task.status).toBe("discarded");
      expect(task.completedAt).toBeDefined();
    });

    it("applyTask throws if remote RPC fails", async () => {
      const { RemoteOrchestrator } = await import("../electron/ssh-orchestrator-remote");

      const remoteOrch = new RemoteOrchestrator(mockRouter as any, {} as any);

      // Create a task.
      mockRouter.nextResult = { path: "/home/user/worktrees/task-fail" };
      const task = await remoteOrch.createTask({
        repoId: "ssh://gpu-server/home/user/repos/project",
        prompt: "Risky change",
        agentType: "claude-code",
      });

      // Make the apply fail.
      mockRouter.calls = [];
      mockRouter.shouldFail = true;
      mockRouter.failError = "Merge conflict detected";

      await expect(remoteOrch.applyTask(task.id)).rejects.toThrow(
        "Merge conflict detected",
      );

      // Status should not be updated on failure.
      expect(task.status).not.toBe("applied");
    });

    it("discardTask throws if remote RPC fails", async () => {
      const { RemoteOrchestrator } = await import("../electron/ssh-orchestrator-remote");

      const remoteOrch = new RemoteOrchestrator(mockRouter as any, {} as any);

      // Create a task.
      mockRouter.nextResult = { path: "/home/user/worktrees/task-disc" };
      const task = await remoteOrch.createTask({
        repoId: "ssh://gpu-server/home/user/repos/project",
        prompt: "Experiment",
        agentType: "claude-code",
      });

      // Make the discard fail.
      mockRouter.calls = [];
      mockRouter.shouldFail = true;
      mockRouter.failError = "SSH connection lost";

      await expect(remoteOrch.discardTask(task.id)).rejects.toThrow(
        "SSH connection lost",
      );
    });
  });
});
