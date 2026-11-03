import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  orchestratorReattach,
  type RunningTask,
  type ReattachDeps,
  type ReattachOptions,
  type ReattachResult,
} from "../electron/daemon/orchestrator-reattach";
import type { SessionInfo } from "../electron/daemon/types";
import type { CreateOrAttachResult } from "../electron/daemon/protocol";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeAgentSession(
  sessionId: string,
  taskId: string,
  overrides: Partial<SessionInfo> = {},
): SessionInfo {
  return {
    sessionId,
    state: "running",
    shellState: "ready",
    isAlive: true,
    pid: 12345,
    cwd: "/tmp/worktree",
    cols: 120,
    rows: 30,
    createdAt: Date.now(),
    agentMeta: {
      taskId,
      agentType: "claude-code",
      worktreePath: "/tmp/worktree",
      activityState: "working",
      stateChangedAt: Date.now(),
    },
    ...overrides,
  };
}

function makeExitedSession(sessionId: string, taskId: string): SessionInfo {
  return makeAgentSession(sessionId, taskId, {
    state: "exited",
    isAlive: false,
    pid: null,
    agentMeta: {
      taskId,
      agentType: "claude-code",
      worktreePath: "/tmp/worktree",
      activityState: "exited",
      stateChangedAt: Date.now(),
    },
  });
}

function makeRunningTask(id: string): RunningTask {
  return { id, status: "running", worktreePath: "/tmp/worktree" };
}

function makeReviewingTask(id: string): RunningTask {
  return { id, status: "reviewing", worktreePath: "/tmp/worktree" };
}

function createMockDeps(sessions: SessionInfo[] = []): {
  deps: ReattachDeps;
  getAgentSessions: ReturnType<typeof vi.fn>;
  createOrAttach: ReturnType<typeof vi.fn>;
  onSessionExited: ReturnType<typeof vi.fn>;
  onSessionMissing: ReturnType<typeof vi.fn>;
  onReattached: ReturnType<typeof vi.fn>;
  log: ReturnType<typeof vi.fn>;
} {
  const getAgentSessions = vi.fn().mockResolvedValue(sessions);
  const createOrAttach = vi.fn().mockResolvedValue({
    isNew: false,
    snapshot: null,
    pid: 12345,
    shellState: "ready",
  } satisfies CreateOrAttachResult);
  const onSessionExited = vi.fn();
  const onSessionMissing = vi.fn();
  const onReattached = vi.fn();
  const log = vi.fn();

  const deps: ReattachDeps = {
    getAgentSessions,
    createOrAttach,
    onSessionExited,
    onSessionMissing,
    onReattached,
    log,
  };

  return { deps, getAgentSessions, createOrAttach, onSessionExited, onSessionMissing, onReattached, log };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("orchestratorReattach", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  // -------------------------------------------------------------------------
  // No running tasks
  // -------------------------------------------------------------------------

  describe("no running tasks", () => {
    it("returns empty result when no tasks are running", async () => {
      const { deps } = createMockDeps();

      const result = await orchestratorReattach([], deps);

      expect(result).toEqual({
        reattached: [],
        exited: [],
        missing: [],
        daemonReachable: true,
      });
    });

    it("does not query the daemon when no tasks are running", async () => {
      const { deps, getAgentSessions } = createMockDeps();

      await orchestratorReattach([], deps);

      expect(getAgentSessions).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Successful reattach (session alive)
  // -------------------------------------------------------------------------

  describe("session alive — reattach", () => {
    it("reattaches a running task to its live daemon session", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1");
      const { deps, createOrAttach, onReattached } = createMockDeps([session]);

      const result = await orchestratorReattach([task], deps);

      expect(result.reattached).toEqual([{ taskId: "task-1", sessionId: "session-1" }]);
      expect(result.exited).toEqual([]);
      expect(result.missing).toEqual([]);
      expect(result.daemonReachable).toBe(true);
      expect(createOrAttach).toHaveBeenCalledWith("session-1", 120, 30);
      expect(onReattached).toHaveBeenCalledWith("task-1", "session-1");
    });

    it("reattaches a reviewing task to its live daemon session", async () => {
      const task = makeReviewingTask("task-2");
      const session = makeAgentSession("session-2", "task-2");
      const { deps, onReattached } = createMockDeps([session]);

      const result = await orchestratorReattach([task], deps);

      expect(result.reattached).toEqual([{ taskId: "task-2", sessionId: "session-2" }]);
      expect(onReattached).toHaveBeenCalledWith("task-2", "session-2");
    });

    it("reattaches multiple tasks to their respective sessions", async () => {
      const tasks = [makeRunningTask("task-1"), makeRunningTask("task-2")];
      const sessions = [
        makeAgentSession("session-1", "task-1"),
        makeAgentSession("session-2", "task-2"),
      ];
      const { deps, onReattached } = createMockDeps(sessions);

      const result = await orchestratorReattach(tasks, deps);

      expect(result.reattached).toHaveLength(2);
      expect(onReattached).toHaveBeenCalledTimes(2);
    });

    it("uses custom cols/rows from options", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1");
      const { deps, createOrAttach } = createMockDeps([session]);

      await orchestratorReattach([task], deps, { cols: 200, rows: 50 });

      expect(createOrAttach).toHaveBeenCalledWith("session-1", 200, 50);
    });
  });

  // -------------------------------------------------------------------------
  // Session already exited
  // -------------------------------------------------------------------------

  describe("session already exited", () => {
    it("reports exit code for a session that already exited", async () => {
      const task = makeRunningTask("task-1");
      const session = makeExitedSession("session-1", "task-1");
      const { deps, onSessionExited, createOrAttach } = createMockDeps([session]);

      const result = await orchestratorReattach([task], deps);

      expect(result.exited).toEqual([{ taskId: "task-1", sessionId: "session-1", exitCode: 1 }]);
      expect(result.reattached).toEqual([]);
      expect(result.missing).toEqual([]);
      expect(onSessionExited).toHaveBeenCalledWith("task-1", 1);
      expect(createOrAttach).not.toHaveBeenCalled();
    });

    it("handles session that is not alive but not in exited state", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1", {
        state: "exiting",
        isAlive: false,
        pid: null,
      });
      const { deps, onSessionExited } = createMockDeps([session]);

      const result = await orchestratorReattach([task], deps);

      expect(result.exited).toEqual([{ taskId: "task-1", sessionId: "session-1", exitCode: 1 }]);
      expect(onSessionExited).toHaveBeenCalledWith("task-1", 1);
    });
  });

  // -------------------------------------------------------------------------
  // Session missing — transition to failed
  // -------------------------------------------------------------------------

  describe("session missing", () => {
    it("marks task as missing when no matching session exists", async () => {
      const task = makeRunningTask("task-1");
      const { deps, onSessionMissing } = createMockDeps([]); // No sessions

      const result = await orchestratorReattach([task], deps);

      expect(result.missing).toEqual([{ taskId: "task-1" }]);
      expect(result.reattached).toEqual([]);
      expect(result.exited).toEqual([]);
      expect(onSessionMissing).toHaveBeenCalledWith("task-1");
    });

    it("marks task as missing when daemon has sessions but none match", async () => {
      const task = makeRunningTask("task-1");
      const unrelatedSession = makeAgentSession("session-other", "task-other");
      const { deps, onSessionMissing } = createMockDeps([unrelatedSession]);

      const result = await orchestratorReattach([task], deps);

      expect(result.missing).toEqual([{ taskId: "task-1" }]);
      expect(onSessionMissing).toHaveBeenCalledWith("task-1");
    });

    it("marks task as missing when createOrAttach fails", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1");
      const { deps, createOrAttach, onSessionMissing } = createMockDeps([session]);
      createOrAttach.mockRejectedValue(new Error("Connection refused"));

      const result = await orchestratorReattach([task], deps);

      expect(result.missing).toEqual([{ taskId: "task-1" }]);
      expect(onSessionMissing).toHaveBeenCalledWith("task-1");
    });
  });

  // -------------------------------------------------------------------------
  // Mixed scenarios
  // -------------------------------------------------------------------------

  describe("mixed scenarios", () => {
    it("handles a mix of alive, exited, and missing sessions", async () => {
      const tasks = [
        makeRunningTask("task-alive"),
        makeRunningTask("task-exited"),
        makeRunningTask("task-missing"),
      ];
      const sessions = [
        makeAgentSession("session-alive", "task-alive"),
        makeExitedSession("session-exited", "task-exited"),
        // No session for task-missing
      ];
      const { deps, onReattached, onSessionExited, onSessionMissing } = createMockDeps(sessions);

      const result = await orchestratorReattach(tasks, deps);

      expect(result.reattached).toEqual([{ taskId: "task-alive", sessionId: "session-alive" }]);
      expect(result.exited).toEqual([{ taskId: "task-exited", sessionId: "session-exited", exitCode: 1 }]);
      expect(result.missing).toEqual([{ taskId: "task-missing" }]);
      expect(onReattached).toHaveBeenCalledWith("task-alive", "session-alive");
      expect(onSessionExited).toHaveBeenCalledWith("task-exited", 1);
      expect(onSessionMissing).toHaveBeenCalledWith("task-missing");
    });
  });

  // -------------------------------------------------------------------------
  // Retry logic — daemon unreachable
  // -------------------------------------------------------------------------

  describe("retry logic", () => {
    it("retries 3 times with 2s intervals when daemon is unreachable", async () => {
      const task = makeRunningTask("task-1");
      const { deps, getAgentSessions, onSessionMissing } = createMockDeps();
      getAgentSessions.mockRejectedValue(new Error("Connection refused"));

      const promise = orchestratorReattach([task], deps, { retryDelayMs: 2000 });

      // First attempt fails immediately
      await vi.advanceTimersByTimeAsync(0);
      // Wait for first retry delay
      await vi.advanceTimersByTimeAsync(2000);
      // Wait for second retry delay
      await vi.advanceTimersByTimeAsync(2000);

      const result = await promise;

      expect(getAgentSessions).toHaveBeenCalledTimes(3);
      expect(result.daemonReachable).toBe(false);
      expect(result.missing).toEqual([{ taskId: "task-1" }]);
      expect(onSessionMissing).toHaveBeenCalledWith("task-1");
    });

    it("succeeds on second attempt after first failure", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1");
      const { deps, getAgentSessions, onReattached } = createMockDeps([session]);
      getAgentSessions
        .mockRejectedValueOnce(new Error("Connection refused"))
        .mockResolvedValueOnce([session]);

      const promise = orchestratorReattach([task], deps, { retryDelayMs: 2000 });

      // First attempt fails, wait for retry
      await vi.advanceTimersByTimeAsync(2000);

      const result = await promise;

      expect(getAgentSessions).toHaveBeenCalledTimes(2);
      expect(result.daemonReachable).toBe(true);
      expect(result.reattached).toEqual([{ taskId: "task-1", sessionId: "session-1" }]);
      expect(onReattached).toHaveBeenCalledWith("task-1", "session-1");
    });

    it("succeeds on third attempt after two failures", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1");
      const { deps, getAgentSessions, onReattached } = createMockDeps([session]);
      getAgentSessions
        .mockRejectedValueOnce(new Error("Connection refused"))
        .mockRejectedValueOnce(new Error("Timeout"))
        .mockResolvedValueOnce([session]);

      const promise = orchestratorReattach([task], deps, { retryDelayMs: 2000 });

      await vi.advanceTimersByTimeAsync(2000);
      await vi.advanceTimersByTimeAsync(2000);

      const result = await promise;

      expect(getAgentSessions).toHaveBeenCalledTimes(3);
      expect(result.daemonReachable).toBe(true);
      expect(result.reattached).toHaveLength(1);
      expect(onReattached).toHaveBeenCalled();
    });

    it("respects custom maxRetries option", async () => {
      const task = makeRunningTask("task-1");
      const { deps, getAgentSessions } = createMockDeps();
      getAgentSessions.mockRejectedValue(new Error("Connection refused"));

      const promise = orchestratorReattach([task], deps, { maxRetries: 5, retryDelayMs: 100 });

      for (let i = 0; i < 4; i++) {
        await vi.advanceTimersByTimeAsync(100);
      }

      const result = await promise;

      expect(getAgentSessions).toHaveBeenCalledTimes(5);
      expect(result.daemonReachable).toBe(false);
    });

    it("marks all tasks as missing when daemon is unreachable", async () => {
      const tasks = [
        makeRunningTask("task-1"),
        makeRunningTask("task-2"),
        makeRunningTask("task-3"),
      ];
      const { deps, getAgentSessions, onSessionMissing } = createMockDeps();
      getAgentSessions.mockRejectedValue(new Error("Connection refused"));

      const promise = orchestratorReattach(tasks, deps, { retryDelayMs: 100 });

      await vi.advanceTimersByTimeAsync(100);
      await vi.advanceTimersByTimeAsync(100);

      const result = await promise;

      expect(result.missing).toHaveLength(3);
      expect(onSessionMissing).toHaveBeenCalledTimes(3);
      expect(onSessionMissing).toHaveBeenCalledWith("task-1");
      expect(onSessionMissing).toHaveBeenCalledWith("task-2");
      expect(onSessionMissing).toHaveBeenCalledWith("task-3");
    });
  });

  // -------------------------------------------------------------------------
  // AgentAdapter contract preserved
  // -------------------------------------------------------------------------

  describe("adapter contract", () => {
    it("does not modify the task objects passed in", async () => {
      const task = makeRunningTask("task-1");
      const originalTask = { ...task };
      const session = makeAgentSession("session-1", "task-1");
      const { deps } = createMockDeps([session]);

      await orchestratorReattach([task], deps);

      expect(task).toEqual(originalTask);
    });

    it("calls onReattached with taskId and sessionId for adapter wiring", async () => {
      const task = makeRunningTask("task-1");
      const session = makeAgentSession("session-1", "task-1");
      const { deps, onReattached } = createMockDeps([session]);

      await orchestratorReattach([task], deps);

      // The orchestrator uses onReattached to wire the adapter's event stream
      expect(onReattached).toHaveBeenCalledWith("task-1", "session-1");
      expect(onReattached).toHaveBeenCalledTimes(1);
    });
  });
});
