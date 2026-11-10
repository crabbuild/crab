/**
 * End-to-end integration test for the remote SSH orchestrator lifecycle.
 *
 * Exercises the complete workflow in sequence:
 * 1. Create task → worktree_create RPC → task created with isRemote: true
 * 2. Start task → setup script (if configured) → task_spawn RPC → running
 * 3. Agent works → task_event notifications stream back → events forwarded
 * 4. Task completes → task_completed notification → reviewing
 * 5. Review diff → task_compute_diff RPC → diff parsed correctly
 * 6. Apply → task_apply RPC → applied
 * 7. Verify merge → task in "applied" status with correct metadata
 *
 * Also tests alternative paths:
 * - Create → Start → Complete → Review → Discard
 * - Create → Start → Connection drops → Reconnect → Resume → Complete → Apply
 * - Create → Start → Complete → Review → Iterate → Complete → Apply
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task } from "../electron/orchestrator";
import { SshConnectionRecovery } from "../electron/ssh-connection-recovery";
import { computeRemoteDiff } from "../electron/orchestrator-diff";

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

/**
 * Mock RPC router that records all calls and allows configuring responses
 * per-method for realistic E2E simulation.
 */
class MockRpcRouter extends EventEmitter {
  calls: Array<{ profileId: string; method: string; params: unknown }> = [];
  private methodResults = new Map<string, unknown>();
  shouldFail = false;
  failError = "Connection lost";

  /** Set the result for a specific RPC method. */
  setMethodResult(method: string, result: unknown): void {
    this.methodResults.set(method, result);
  }

  async sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    this.calls.push({ profileId, method, params });
    if (this.shouldFail) {
      throw new Error(this.failError);
    }
    const result = this.methodResults.get(method);
    return result ?? {};
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

  /** Reset recorded calls (useful between lifecycle phases). */
  resetCalls(): void {
    this.calls = [];
  }

  /** Get calls for a specific method. */
  getCallsForMethod(method: string): Array<{ profileId: string; method: string; params: unknown }> {
    return this.calls.filter((c) => c.method === method);
  }
}

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

// ─── Helpers ─────────────────────────────────────────────────────────────────

const PROFILE_ID = "gpu-server";
const REMOTE_REPO = "ssh://gpu-server/home/user/repos/ml-project";
const REMOTE_WORKTREE = "/home/user/worktrees/agent-task-abc";

function createTempPersistPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-e2e-test-"));
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
 * Simulate the remote agent streaming events back to the orchestrator.
 * This mimics what the SSH activity bridge does in production.
 */
async function simulateRemoteEvents(
  orchestrator: Orchestrator,
  taskId: string,
  events: Array<{ method: string; params: Record<string, unknown> }>,
): Promise<void> {
  for (const event of events) {
    await orchestrator.handleRemoteTaskNotification({
      profileId: PROFILE_ID,
      method: event.method,
      params: { task_id: taskId, ...event.params },
    });
  }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("SSH Orchestrator E2E — Full Lifecycle", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;
  let mockRouter: MockRpcRouter;

  beforeEach(() => {
    vi.useFakeTimers();
    persistPath = createTempPersistPath();
    mockRouter = new MockRpcRouter();

    // Configure default method results for the happy path.
    mockRouter.setMethodResult("worktree_create", {
      path: REMOTE_WORKTREE,
      branch: "agent/task-abc",
      base_ref: "a".repeat(40),
    });
    mockRouter.setMethodResult("read_text", {}); // No remote scripts config
    mockRouter.setMethodResult("task_spawn", { started: true });
    mockRouter.setMethodResult("task_compute_diff", {
      diff: [
        "diff --git a/src/model.py b/src/model.py",
        "--- a/src/model.py",
        "+++ b/src/model.py",
        "@@ -10,6 +10,8 @@ class Model:",
        "     def forward(self, x):",
        "+        # Add dropout for regularization",
        "+        x = self.dropout(x)",
        "         return self.linear(x)",
      ].join("\n"),
      name_status: "M\tsrc/model.py\n",
      head_ref: "def789abc123",
    });
    mockRouter.setMethodResult("task_apply", {
      applied: true,
      target_branch: "main",
      worktree_removed: true,
      branch_deleted: true,
    });
    mockRouter.setMethodResult("task_discard", {
      discarded: true,
      worktree_removed: true,
      branch_deleted: true,
    });
    mockRouter.setMethodResult("task_iterate", { started: true });
    mockRouter.setMethodResult("task_check_orphaned", {
      worktreeExists: true,
      agentRunning: false,
    });

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

  // ─── Happy Path: Create → Start → Work → Complete → Diff → Apply ──────

  describe("Happy path: create → start → agent works → complete → diff → apply", () => {
    it("exercises the full remote task lifecycle end-to-end", async () => {
      const statusEvents: Array<{ taskId: string; status: string }> = [];
      const activityEvents: Array<{ taskId: string; event: any }> = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));
      orchestrator.on("orchestrator:event", (e) => activityEvents.push(e));

      // ── Step 1: Create task ──────────────────────────────────────────
      const task = await orchestrator.createTask(
        "Add dropout regularization to the model",
        "claude-code",
        REMOTE_REPO,
        "HEAD",
      );

      expect(task.isRemote).toBe(true);
      expect(task.remoteProfileId).toBe(PROFILE_ID);
      expect(task.status).toBe("created");
      expect(task.worktreePath).toBe(REMOTE_WORKTREE);
      expect(task.branch).toContain("agent/");

      // Verify worktree_create RPC was sent.
      const worktreeCall = mockRouter.getCallsForMethod("worktree_create");
      expect(worktreeCall).toHaveLength(1);
      expect(worktreeCall[0].profileId).toBe(PROFILE_ID);
      expect(worktreeCall[0].params).toMatchObject({
        repo_id: REMOTE_REPO,
        base_ref: "HEAD",
        task_id: task.id,
      });

      // ── Step 2: Start task ───────────────────────────────────────────
      mockRouter.resetCalls();
      await orchestrator.startTask(task.id);

      expect(task.status).toBe("running");
      expect(task.startedAt).toBeGreaterThan(0);

      // Verify task_spawn RPC was sent.
      const spawnCall = mockRouter.getCallsForMethod("task_spawn");
      expect(spawnCall).toHaveLength(1);
      expect(spawnCall[0].params).toMatchObject({
        task_id: task.id,
        prompt: "Add dropout regularization to the model",
        agent_type: "claude-code",
        worktree_path: REMOTE_WORKTREE,
      });

      // ── Step 3: Agent works (simulate event stream) ──────────────────
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: {
            event: { type: "thinking", timestamp: 1000, content: "Analyzing model architecture..." },
          },
        },
        {
          method: "task_event",
          params: {
            event: { type: "file_edit", timestamp: 2000, path: "src/model.py", diff: "+x = self.dropout(x)" },
          },
        },
        {
          method: "task_event",
          params: {
            event: { type: "command", timestamp: 3000, command: "python -m pytest tests/" },
          },
        },
      ]);

      // Verify events were forwarded to the event system.
      expect(activityEvents).toHaveLength(3);
      expect(activityEvents[0].event.type).toBe("thinking");
      expect(activityEvents[1].event.type).toBe("file_edit");
      expect(activityEvents[2].event.type).toBe("command");
      expect(task.events).toHaveLength(3);

      // ── Step 4: Task completes ───────────────────────────────────────
      await simulateRemoteEvents(orchestrator, task.id, [
        { method: "task_completed", params: { exit_code: 0 } },
      ]);

      expect(task.status).toBe("reviewing");
      expect(task.exitCode).toBe(0);
      expect(task.completedAt).toBeGreaterThan(0);

      // ── Step 5: Review diff ──────────────────────────────────────────
      mockRouter.resetCalls();
      const diff = await computeRemoteDiff(task, mockRouter as any);

      expect(diff.taskId).toBe(task.id);
      expect(diff.baseRef).toBe(task.baseRef);
      expect(diff.headRef).toBe("def789abc123");
      expect(diff.files).toHaveLength(1);
      expect(diff.files[0].path).toBe("src/model.py");
      expect(diff.files[0].status).toBe("modified");
      expect(diff.files[0].additions).toBe(2);
      expect(diff.rawDiff).toContain("dropout");

      // Verify task_compute_diff RPC was sent.
      const diffCall = mockRouter.getCallsForMethod("task_compute_diff");
      expect(diffCall).toHaveLength(1);
      expect(diffCall[0].params).toMatchObject({
        repo_id: REMOTE_REPO,
        worktree_path: REMOTE_WORKTREE,
        base_ref: task.baseRef,
        branch: task.branch,
      });

      // ── Step 6: Apply ────────────────────────────────────────────────
      mockRouter.resetCalls();
      await orchestrator.applyTask(task.id);

      expect(task.status).toBe("applied");
      expect(task.completedAt).toBeGreaterThan(0);

      // Verify task_apply RPC was sent.
      const applyCall = mockRouter.getCallsForMethod("task_apply");
      expect(applyCall).toHaveLength(1);
      expect(applyCall[0].params).toMatchObject({
        repo_id: REMOTE_REPO,
        task_id: task.id,
        branch: task.branch,
        base_ref: task.baseRef,
        worktree_path: REMOTE_WORKTREE,
      });

      // ── Step 7: Verify final state ───────────────────────────────────
      const finalTask = orchestrator.getTask(task.id);
      expect(finalTask).toBeDefined();
      expect(finalTask!.status).toBe("applied");
      expect(finalTask!.isRemote).toBe(true);
      expect(finalTask!.remoteProfileId).toBe(PROFILE_ID);
      expect(finalTask!.events).toHaveLength(3);
      expect(finalTask!.prompt).toBe("Add dropout regularization to the model");

      // Verify the full status transition sequence.
      const taskStatuses = statusEvents
        .filter((e) => e.taskId === task.id)
        .map((e) => e.status);
      expect(taskStatuses).toEqual([
        "created",
        "running",
        "reviewing",
        "applied",
      ]);
    });
  });


  // ─── Alternative Path: Create → Start → Complete → Review → Discard ────

  describe("Alternative path: create → start → complete → review → discard", () => {
    it("discards a completed task instead of applying", async () => {
      const statusEvents: Array<{ taskId: string; status: string }> = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      // Create and start.
      const task = await orchestrator.createTask(
        "Experiment with new loss function",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);
      expect(task.status).toBe("running");

      // Agent works and completes.
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: { event: { type: "file_edit", timestamp: 1000, path: "src/loss.py", diff: "+new loss" } },
        },
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");

      // Review diff.
      mockRouter.resetCalls();
      const diff = await computeRemoteDiff(task, mockRouter as any);
      expect(diff.files).toHaveLength(1);

      // Discard instead of apply.
      mockRouter.resetCalls();
      await orchestrator.discardTask(task.id);

      expect(task.status).toBe("discarded");
      expect(task.completedAt).toBeGreaterThan(0);

      // Verify task_discard RPC was sent.
      const discardCall = mockRouter.getCallsForMethod("task_discard");
      expect(discardCall).toHaveLength(1);
      expect(discardCall[0].params).toMatchObject({
        repo_id: REMOTE_REPO,
        task_id: task.id,
        branch: task.branch,
        worktree_path: REMOTE_WORKTREE,
      });

      // Verify the full status transition sequence.
      const taskStatuses = statusEvents
        .filter((e) => e.taskId === task.id)
        .map((e) => e.status);
      expect(taskStatuses).toEqual([
        "created",
        "running",
        "reviewing",
        "discarded",
      ]);
    });
  });

  // ─── Alternative Path: Connection Drop → Reconnect → Resume ───────────

  describe("Alternative path: connection drops → reconnect → resume → complete → apply", () => {
    it("recovers from SSH disconnection and resumes the task", async () => {
      const mockSessionManager = new MockSshSessionManager();
      const recovery = new SshConnectionRecovery(
        mockSessionManager as any,
        orchestrator,
        mockRouter as any,
      );

      const statusEvents: Array<{ taskId: string; status: string }> = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

      // Create and start.
      const task = await orchestrator.createTask(
        "Train the model with new hyperparameters",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);
      expect(task.status).toBe("running");

      // Simulate some events before disconnection.
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: { event: { type: "thinking", timestamp: 1000, content: "Setting up training..." } },
        },
      ]);
      expect(task.events).toHaveLength(1);

      // ── Connection drops ─────────────────────────────────────────────
      await recovery.handleDisconnection(PROFILE_ID);
      expect(task.status).toBe("interrupted");

      // ── Reconnect ────────────────────────────────────────────────────
      // Configure the orphaned check to report worktree exists, agent stopped.
      mockRouter.setMethodResult("task_check_orphaned", {
        taskId: task.id,
        worktreePath: task.worktreePath,
        branch: task.branch,
        worktreeExists: true,
        agentRunning: false,
      });

      const recoveryPrompts: any[] = [];
      recovery.on("ssh:task-recovery-prompt", (p) => recoveryPrompts.push(p));

      await recovery.handleReconnection(PROFILE_ID);

      // Should emit a recovery prompt since agent is stopped.
      expect(recoveryPrompts).toHaveLength(1);
      expect(recoveryPrompts[0].taskId).toBe(task.id);
      expect(recoveryPrompts[0].options).toEqual(["resume", "discard"]);

      // ── User chooses Resume ──────────────────────────────────────────
      mockRouter.resetCalls();
      await recovery.resumeTask(task.id);
      expect(task.status).toBe("running");

      // Verify task_spawn RPC was sent for the resume.
      const spawnCalls = mockRouter.getCallsForMethod("task_spawn");
      expect(spawnCalls).toHaveLength(1);
      expect(spawnCalls[0].params).toMatchObject({
        task_id: task.id,
        worktree_path: task.worktreePath,
      });

      // ── Agent continues and completes ────────────────────────────────
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: { event: { type: "file_edit", timestamp: 5000, path: "train.py", diff: "+epochs=100" } },
        },
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");
      expect(task.events).toHaveLength(2); // thinking + file_edit (from after resume)

      // ── Apply ────────────────────────────────────────────────────────
      mockRouter.resetCalls();
      await orchestrator.applyTask(task.id);
      expect(task.status).toBe("applied");

      // Verify the full status transition sequence includes interrupted.
      const taskStatuses = statusEvents
        .filter((e) => e.taskId === task.id)
        .map((e) => e.status);
      expect(taskStatuses).toContain("interrupted");
      expect(taskStatuses).toContain("applied");
    });

    it("handles reconnection when agent is still running (re-attach)", async () => {
      const mockSessionManager = new MockSshSessionManager();
      const recovery = new SshConnectionRecovery(
        mockSessionManager as any,
        orchestrator,
        mockRouter as any,
      );

      // Create and start.
      const task = await orchestrator.createTask(
        "Long-running training job",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);

      // Connection drops.
      await recovery.handleDisconnection(PROFILE_ID);
      expect(task.status).toBe("interrupted");

      // Reconnect — agent is still running on remote.
      mockRouter.setMethodResult("task_check_orphaned", {
        taskId: task.id,
        worktreePath: task.worktreePath,
        branch: task.branch,
        worktreeExists: true,
        agentRunning: true,
      });

      const reattachEvents: any[] = [];
      recovery.on("ssh:task-reattached", (e) => reattachEvents.push(e));

      await recovery.handleReconnection(PROFILE_ID);

      // Task should be re-attached (back to running) without user intervention.
      expect(task.status).toBe("running");
      expect(reattachEvents).toHaveLength(1);
      expect(reattachEvents[0].taskId).toBe(task.id);

      // Events continue streaming after re-attach.
      await simulateRemoteEvents(orchestrator, task.id, [
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");
    });
  });


  // ─── Alternative Path: Iterate → Agent Works Again → Complete → Apply ──

  describe("Alternative path: create → start → complete → review → iterate → complete → apply", () => {
    it("iterates on a task with feedback before applying", async () => {
      const statusEvents: Array<{ taskId: string; status: string }> = [];
      const activityEvents: Array<{ taskId: string; event: any }> = [];
      orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));
      orchestrator.on("orchestrator:event", (e) => activityEvents.push(e));

      // Create and start.
      const task = await orchestrator.createTask(
        "Implement data augmentation pipeline",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);

      // First agent run completes.
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: { event: { type: "file_edit", timestamp: 1000, path: "src/augment.py", diff: "+class Augmenter:" } },
        },
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");

      // Review diff — looks good but needs tests.
      mockRouter.resetCalls();
      const firstDiff = await computeRemoteDiff(task, mockRouter as any);
      expect(firstDiff.files).toHaveLength(1);

      // ── Iterate with feedback ────────────────────────────────────────
      mockRouter.resetCalls();
      await orchestrator.iterate(task.id, "Please also add unit tests for the augmentation pipeline");

      expect(task.status).toBe("running");
      expect(task.feedback).toContain("Please also add unit tests for the augmentation pipeline");

      // Verify task_iterate RPC was sent (not task_spawn).
      const iterateCalls = mockRouter.getCallsForMethod("task_iterate");
      expect(iterateCalls).toHaveLength(1);
      expect(iterateCalls[0].params).toMatchObject({
        task_id: task.id,
        feedback: "Please also add unit tests for the augmentation pipeline",
        agent_type: "claude-code",
        worktree_path: REMOTE_WORKTREE,
      });

      const spawnCalls = mockRouter.getCallsForMethod("task_spawn");
      expect(spawnCalls).toHaveLength(0);

      // ── Agent works again (second iteration) ─────────────────────────
      activityEvents.length = 0; // Reset for clarity.
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: { event: { type: "thinking", timestamp: 4000, content: "Adding tests..." } },
        },
        {
          method: "task_event",
          params: { event: { type: "file_edit", timestamp: 5000, path: "tests/test_augment.py", diff: "+def test_augmenter():" } },
        },
        { method: "task_completed", params: { exit_code: 0 } },
      ]);

      expect(task.status).toBe("reviewing");
      expect(activityEvents).toHaveLength(2); // thinking + file_edit (not task_completed)

      // ── Review updated diff ──────────────────────────────────────────
      // Update the mock to return a diff with both files.
      mockRouter.setMethodResult("task_compute_diff", {
        diff: [
          "diff --git a/src/augment.py b/src/augment.py",
          "--- /dev/null",
          "+++ b/src/augment.py",
          "@@ -0,0 +1,3 @@",
          "+class Augmenter:",
          "+    pass",
          "+",
          "diff --git a/tests/test_augment.py b/tests/test_augment.py",
          "--- /dev/null",
          "+++ b/tests/test_augment.py",
          "@@ -0,0 +1,3 @@",
          "+def test_augmenter():",
          "+    assert True",
          "+",
        ].join("\n"),
        name_status: "A\tsrc/augment.py\nA\ttests/test_augment.py\n",
        head_ref: "iteration2-sha",
      });

      mockRouter.resetCalls();
      const secondDiff = await computeRemoteDiff(task, mockRouter as any);
      expect(secondDiff.files).toHaveLength(2);
      expect(secondDiff.files.map((f) => f.path)).toContain("tests/test_augment.py");

      // ── Apply after iteration ────────────────────────────────────────
      mockRouter.resetCalls();
      await orchestrator.applyTask(task.id);
      expect(task.status).toBe("applied");

      // Verify the full status transition sequence.
      const taskStatuses = statusEvents
        .filter((e) => e.taskId === task.id)
        .map((e) => e.status);
      expect(taskStatuses).toEqual([
        "created",   // createTask
        "running",   // startTask
        "reviewing", // task_completed (first run)
        "iterating", // iterate()
        "running",   // iterate → running
        "reviewing", // task_completed (second run)
        "applied",   // applyTask
      ]);

      // Verify turn number was incremented.
      expect(task.turnNumber).toBe(1);
    });

    it("supports multiple iteration cycles before applying", async () => {
      // Create and start.
      const task = await orchestrator.createTask(
        "Build data loader",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);

      // First run completes.
      await simulateRemoteEvents(orchestrator, task.id, [
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");

      // First iteration.
      await orchestrator.iterate(task.id, "Add caching");
      await simulateRemoteEvents(orchestrator, task.id, [
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");
      expect(task.turnNumber).toBe(1);

      // Second iteration.
      await orchestrator.iterate(task.id, "Add prefetching");
      await simulateRemoteEvents(orchestrator, task.id, [
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      expect(task.status).toBe("reviewing");
      expect(task.turnNumber).toBe(2);

      // Apply after two iterations.
      await orchestrator.applyTask(task.id);
      expect(task.status).toBe("applied");
      expect(task.feedback).toEqual(["Add caching", "Add prefetching"]);

      // Verify all iterate RPCs were sent.
      const iterateCalls = mockRouter.getCallsForMethod("task_iterate");
      expect(iterateCalls).toHaveLength(2);
    });
  });

  // ─── Edge Cases ────────────────────────────────────────────────────────

  describe("Edge cases", () => {
    it("handles agent failure during task execution", async () => {
      const task = await orchestrator.createTask(
        "Risky refactor",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);

      // Agent fails.
      await simulateRemoteEvents(orchestrator, task.id, [
        {
          method: "task_event",
          params: { event: { type: "thinking", timestamp: 1000, content: "Starting..." } },
        },
        {
          method: "task_failed",
          params: { exit_code: 1, error: "Agent crashed: OOM killed" },
        },
      ]);

      expect(task.status).toBe("failed");
      expect(task.exitCode).toBe(1);
      // Error event should be in the events array.
      const errorEvent = task.events.find((e) => e.type === "error");
      expect(errorEvent).toBeDefined();
      expect(errorEvent!.error).toBe("Agent crashed: OOM killed");
    });

    it("task creation fails if worktree_create RPC fails", async () => {
      mockRouter.shouldFail = true;
      mockRouter.failError = "Disk full on remote";

      await expect(
        orchestrator.createTask("Some task", "claude-code", REMOTE_REPO),
      ).rejects.toThrow("Disk full on remote");
    });

    it("task start fails if task_spawn RPC fails", async () => {
      const task = await orchestrator.createTask(
        "Another task",
        "claude-code",
        REMOTE_REPO,
      );

      mockRouter.shouldFail = true;
      mockRouter.failError = "Agent binary not found";

      await expect(orchestrator.startTask(task.id)).rejects.toThrow(
        "Failed to spawn remote task: Agent binary not found",
      );
      expect(task.status).toBe("failed");
    });

    it("persists task state across the lifecycle", async () => {
      const task = await orchestrator.createTask(
        "Persistent task",
        "claude-code",
        REMOTE_REPO,
      );
      await orchestrator.startTask(task.id);
      await simulateRemoteEvents(orchestrator, task.id, [
        { method: "task_completed", params: { exit_code: 0 } },
      ]);
      await orchestrator.applyTask(task.id);

      const reloaded = new Orchestrator({ persistPath, timeoutMs: 60_000 });
      const persisted = reloaded.getTask(task.id);

      expect(persisted).toBeDefined();
      expect(persisted!.status).toBe("applied");
      expect(persisted!.isRemote).toBe(true);
      expect(persisted!.remoteProfileId).toBe(PROFILE_ID);
      expect(persisted!.worktreePath).toBe(REMOTE_WORKTREE);
    });

    it("multiple tasks can run concurrently on the same remote", async () => {
      const orch = new Orchestrator({
        persistPath: createTempPersistPath(),
        timeoutMs: 60_000,
        maxConcurrent: 3,
        rpcRouter: mockRouter as any,
      });

      const task1 = await orch.createTask("Task 1", "claude-code", REMOTE_REPO);
      const task2 = await orch.createTask("Task 2", "claude-code", REMOTE_REPO);

      await orch.startTask(task1.id);
      await orch.startTask(task2.id);

      expect(task1.status).toBe("running");
      expect(task2.status).toBe("running");

      // Complete task1, task2 still running.
      await orch.handleRemoteTaskNotification({
        profileId: PROFILE_ID,
        method: "task_completed",
        params: { task_id: task1.id, exit_code: 0 },
      });
      expect(task1.status).toBe("reviewing");
      expect(task2.status).toBe("running");
    });
  });
});
