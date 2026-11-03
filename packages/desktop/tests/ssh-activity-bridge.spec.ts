/**
 * Integration test for the SSH Activity Bridge.
 *
 * Verifies the full flow: remote agent emits a JSON-RPC notification
 * over the SSH channel → RPC router parses it and emits "notification"
 * → bridge forwards to orchestrator.handleRemoteTaskNotification() →
 * orchestrator emits orchestrator:event / orchestrator:task-status →
 * (in production, the IPC relay sends these to the renderer).
 *
 * This test simulates the entire pipeline without real SSH connections,
 * proving that remote task activity displays identically to local tasks.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task } from "../electron/orchestrator";
import { RpcRouter } from "../electron/ssh-router";
import { bridgeRemoteActivityFeed } from "../electron/ssh-activity-bridge";
import type { ClientChannel } from "ssh2";

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

// ─── Mock SSH Channel ────────────────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();

  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

class MockSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockChannel } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { channels: { agent?: MockChannel } } | undefined {
    return this.connections.get(profileId);
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createTempPersistPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-bridge-test-"));
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
    status: "running",
    branch: `agent/${id.slice(0, 8)}`,
    worktreePath: `/home/user/worktrees/agent-${id.slice(0, 8)}`,
    baseRef: "abc123",
    repoPath: "ssh://gpu-server/home/user/repos/project",
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

/**
 * Simulate a JSON-RPC notification arriving on the SSH channel
 * (as if the remote agent wrote it to stdout).
 */
function simulateRemoteNotification(
  channel: MockChannel,
  method: string,
  params: unknown,
): void {
  const notification = JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n";
  channel.emit("data", Buffer.from(notification));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("SSH Activity Bridge — full pipeline integration", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;
  let router: RpcRouter;
  let sessionManager: MockSessionManager;
  let channel: MockChannel;

  beforeEach(() => {
    persistPath = createTempPersistPath();

    // Create the real components (no mocking of the bridge itself).
    sessionManager = new MockSessionManager();
    sessionManager.setStatus("gpu-server", "connected");
    channel = new MockChannel();
    sessionManager.setConnection("gpu-server", channel);

    const localAgent = { request: vi.fn(async () => ({})) };
    router = new RpcRouter(sessionManager as any, localAgent);
    router.attachChannel("gpu-server", channel.asChannel());

    orchestrator = new Orchestrator({ persistPath, timeoutMs: 60_000 });

    // Wire the bridge — this is the code under test.
    bridgeRemoteActivityFeed(router, orchestrator);
  });

  afterEach(() => {
    cleanupPath(persistPath);
  });

  it("task_event notifications flow from SSH channel to orchestrator:event", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));

    // Simulate the remote agent sending a task_event notification.
    simulateRemoteNotification(channel, "task_event", {
      task_id: task.id,
      event: {
        type: "text",
        timestamp: 1000,
        content: "Analyzing the codebase...",
      },
    });

    // Allow the async handler to complete.
    await new Promise((r) => setTimeout(r, 10));

    expect(events).toHaveLength(1);
    expect(events[0].taskId).toBe(task.id);
    expect(events[0].event.type).toBe("text");
    expect(events[0].event.content).toBe("Analyzing the codebase...");
    expect(events[0].event.timestamp).toBe(1000);
  });

  it("task_completed notifications transition task to reviewing", async () => {
    const task = injectRemoteTask(orchestrator);

    const statusEvents: Array<{ taskId: string; status: string }> = [];
    orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

    simulateRemoteNotification(channel, "task_completed", {
      task_id: task.id,
      exit_code: 0,
    });

    await new Promise((r) => setTimeout(r, 10));

    expect(task.status).toBe("reviewing");
    expect(task.exitCode).toBe(0);
    expect(task.completedAt).toBeGreaterThan(0);
    expect(statusEvents).toContainEqual({ taskId: task.id, status: "reviewing" });
  });

  it("task_failed notifications transition task to failed with error event", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    const statusEvents: Array<{ taskId: string; status: string }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));
    orchestrator.on("orchestrator:task-status", (e) => statusEvents.push(e));

    simulateRemoteNotification(channel, "task_failed", {
      task_id: task.id,
      exit_code: 137,
      error: "Process killed by OOM killer",
    });

    await new Promise((r) => setTimeout(r, 10));

    expect(task.status).toBe("failed");
    expect(task.exitCode).toBe(137);
    expect(statusEvents).toContainEqual({ taskId: task.id, status: "failed" });
    expect(events).toHaveLength(1);
    expect(events[0].event.type).toBe("error");
    expect(events[0].event.error).toBe("Process killed by OOM killer");
  });

  it("multiple events stream through in sequence", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));

    // Simulate a realistic sequence of events from the remote agent.
    simulateRemoteNotification(channel, "task_event", {
      task_id: task.id,
      event: { type: "thinking", timestamp: 1000, content: "Planning approach..." },
    });
    simulateRemoteNotification(channel, "task_event", {
      task_id: task.id,
      event: { type: "file_edit", timestamp: 2000, path: "src/main.rs", diff: "+fix" },
    });
    simulateRemoteNotification(channel, "task_event", {
      task_id: task.id,
      event: { type: "command", timestamp: 3000, command: "cargo test", stdout: "ok" },
    });
    simulateRemoteNotification(channel, "task_completed", {
      task_id: task.id,
      exit_code: 0,
    });

    await new Promise((r) => setTimeout(r, 10));

    expect(events).toHaveLength(3);
    expect(events[0].event.type).toBe("thinking");
    expect(events[1].event.type).toBe("file_edit");
    expect(events[2].event.type).toBe("command");
    expect(task.status).toBe("reviewing");
    expect(task.events).toHaveLength(3);
  });

  it("non-task notifications are not forwarded to the orchestrator", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));

    // Simulate a non-task notification (e.g., progress for file operations).
    simulateRemoteNotification(channel, "progress/update", {
      percent: 50,
      operation: "hydrate",
    });

    await new Promise((r) => setTimeout(r, 10));

    expect(events).toHaveLength(0);
  });

  it("bridge sets the RPC router on the orchestrator", () => {
    // The bridge should have called orchestrator.setRpcRouter(router).
    // Verify by checking that the orchestrator can route remote tasks.
    expect((orchestrator as any).rpcRouter).toBe(router);
  });

  it("events from remote tasks have the same shape as local task events", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));

    simulateRemoteNotification(channel, "task_event", {
      task_id: task.id,
      event: {
        type: "file_edit",
        timestamp: 5000,
        path: "src/lib.rs",
        diff: "+new code\n-old code",
        tool: "Write",
      },
    });

    await new Promise((r) => setTimeout(r, 10));

    // Verify the event payload matches the shape expected by the renderer.
    const emitted = events[0];
    expect(emitted).toHaveProperty("taskId");
    expect(emitted).toHaveProperty("event");
    expect(emitted.event).toHaveProperty("type");
    expect(emitted.event).toHaveProperty("timestamp");
    expect(emitted.event).toHaveProperty("path");
    expect(emitted.event).toHaveProperty("diff");
    // The renderer uses these exact fields — no transformation needed.
    expect(emitted.taskId).toBe(task.id);
    expect(emitted.event.type).toBe("file_edit");
    expect(emitted.event.path).toBe("src/lib.rs");
    expect(emitted.event.diff).toBe("+new code\n-old code");
    expect(emitted.event.tool).toBe("Write");
    expect(emitted.event.timestamp).toBe(5000);
  });

  it("handles notifications arriving as partial NDJSON chunks", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));

    // Send the notification in two chunks (simulating TCP fragmentation).
    const fullNotification = JSON.stringify({
      jsonrpc: "2.0",
      method: "task_event",
      params: {
        task_id: task.id,
        event: { type: "text", timestamp: 1000, content: "Hello from remote" },
      },
    }) + "\n";

    const mid = Math.floor(fullNotification.length / 2);
    channel.emit("data", Buffer.from(fullNotification.slice(0, mid)));
    channel.emit("data", Buffer.from(fullNotification.slice(mid)));

    await new Promise((r) => setTimeout(r, 10));

    expect(events).toHaveLength(1);
    expect(events[0].event.content).toBe("Hello from remote");
  });

  it("interleaved RPC responses and notifications both work correctly", async () => {
    const task = injectRemoteTask(orchestrator);

    const events: Array<{ taskId: string; event: any }> = [];
    orchestrator.on("orchestrator:event", (e) => events.push(e));

    // Start an RPC call (e.g., git_status on the remote).
    const rpcPromise = router.route(
      "ssh://gpu-server/home/user/repos/project",
      "git_status",
      { repo_id: "ssh://gpu-server/home/user/repos/project" },
    );

    // Remote agent sends a task notification before the RPC response.
    simulateRemoteNotification(channel, "task_event", {
      task_id: task.id,
      event: { type: "text", timestamp: 1000, content: "Still working..." },
    });

    // Now the RPC response arrives.
    const rpcResponse = JSON.stringify({ jsonrpc: "2.0", id: 1, result: { status: "clean" } }) + "\n";
    channel.emit("data", Buffer.from(rpcResponse));

    await new Promise((r) => setTimeout(r, 10));

    // Both should work: notification forwarded, RPC resolved.
    expect(events).toHaveLength(1);
    expect(events[0].event.content).toBe("Still working...");
    const rpcResult = await rpcPromise;
    expect(rpcResult).toEqual({ status: "clean" });
  });
});
