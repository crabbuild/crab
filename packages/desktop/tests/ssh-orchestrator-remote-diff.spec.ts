/**
 * Tests for remote diff computation in the orchestrator.
 *
 * Verifies that when a task has `isRemote: true`, the orchestrator
 * routes diff computation to the remote agent via `task_compute_diff`
 * RPC, and that the returned diff is parsed identically to local diffs.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import { Orchestrator, Task } from "../electron/orchestrator";
import { computeRemoteDiff, parseDiffOutput } from "../electron/orchestrator-diff";

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
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-remote-diff-test-"));
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

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Remote Diff Computation", () => {
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

  describe("computeRemoteDiff", () => {
    it("sends task_compute_diff RPC to the remote agent", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = {
        diff: "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n+    println!(\"hello\");\n }\n",
        name_status: "M\tsrc/main.rs\n",
        head_ref: "def456abc789",
      };

      await computeRemoteDiff(task, mockRouter as any);

      expect(mockRouter.calls).toHaveLength(1);
      expect(mockRouter.calls[0]).toEqual({
        profileId: "gpu-server",
        method: "task_compute_diff",
        params: {
          repo_id: task.repoPath,
          worktree_path: task.worktreePath,
          base_ref: task.baseRef,
          branch: task.branch,
        },
      });
    });

    it("returns a TaskDiff with parsed files", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = {
        diff: "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n+    println!(\"hello\");\n }\n",
        name_status: "M\tsrc/main.rs\n",
        head_ref: "def456abc789",
      };

      const diff = await computeRemoteDiff(task, mockRouter as any);

      expect(diff.taskId).toBe(task.id);
      expect(diff.baseRef).toBe(task.baseRef);
      expect(diff.headRef).toBe("def456abc789");
      expect(diff.files).toHaveLength(1);
      expect(diff.files[0].path).toBe("src/main.rs");
      expect(diff.files[0].status).toBe("modified");
      expect(diff.files[0].additions).toBe(1);
      expect(diff.files[0].deletions).toBe(0);
      expect(diff.totalAdditions).toBe(1);
      expect(diff.totalDeletions).toBe(0);
      expect(diff.rawDiff).toContain("println");
    });

    it("handles multiple files in the diff", async () => {
      const task = injectRemoteTask(orchestrator);

      const rawDiff = [
        "diff --git a/src/lib.rs b/src/lib.rs",
        "--- a/src/lib.rs",
        "+++ b/src/lib.rs",
        "@@ -1,2 +1,3 @@",
        " pub mod utils;",
        "+pub mod helpers;",
        "",
        "diff --git a/src/helpers.rs b/src/helpers.rs",
        "new file mode 100644",
        "--- /dev/null",
        "+++ b/src/helpers.rs",
        "@@ -0,0 +1,5 @@",
        "+pub fn add(a: i32, b: i32) -> i32 {",
        "+    a + b",
        "+}",
        "+",
        "+pub fn sub(a: i32, b: i32) -> i32 { a - b }",
      ].join("\n");

      mockRouter.nextResult = {
        diff: rawDiff,
        name_status: "M\tsrc/lib.rs\nA\tsrc/helpers.rs\n",
        head_ref: "aaa111bbb222",
      };

      const diff = await computeRemoteDiff(task, mockRouter as any);

      expect(diff.files).toHaveLength(2);
      expect(diff.files[0].path).toBe("src/lib.rs");
      expect(diff.files[0].status).toBe("modified");
      expect(diff.files[0].additions).toBe(1);
      expect(diff.files[1].path).toBe("src/helpers.rs");
      expect(diff.files[1].status).toBe("added");
      expect(diff.files[1].additions).toBe(5);
      expect(diff.totalAdditions).toBe(6);
    });

    it("handles empty diff (no changes)", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = {
        diff: "",
        name_status: "",
        head_ref: "abc123",
      };

      const diff = await computeRemoteDiff(task, mockRouter as any);

      expect(diff.files).toHaveLength(0);
      expect(diff.totalAdditions).toBe(0);
      expect(diff.totalDeletions).toBe(0);
      expect(diff.rawDiff).toBe("");
    });

    it("handles renamed files", async () => {
      const task = injectRemoteTask(orchestrator);

      const rawDiff = [
        "diff --git a/old_name.rs b/new_name.rs",
        "similarity index 95%",
        "rename from old_name.rs",
        "rename to new_name.rs",
        "--- a/old_name.rs",
        "+++ b/new_name.rs",
        "@@ -1,3 +1,3 @@",
        "-fn old() {}",
        "+fn new() {}",
      ].join("\n");

      mockRouter.nextResult = {
        diff: rawDiff,
        name_status: "R100\told_name.rs\tnew_name.rs\n",
        head_ref: "fff000",
      };

      const diff = await computeRemoteDiff(task, mockRouter as any);

      expect(diff.files).toHaveLength(1);
      expect(diff.files[0].path).toBe("new_name.rs");
      expect(diff.files[0].status).toBe("renamed");
      expect(diff.files[0].oldPath).toBe("old_name.rs");
    });

    it("throws if remoteProfileId is missing", async () => {
      const task = injectRemoteTask(orchestrator, { remoteProfileId: undefined });

      await expect(computeRemoteDiff(task, mockRouter as any)).rejects.toThrow(
        "no remoteProfileId",
      );
    });

    it("propagates RPC errors", async () => {
      const task = injectRemoteTask(orchestrator);
      mockRouter.shouldFail = true;
      mockRouter.failError = "SSH connection lost during diff";

      await expect(computeRemoteDiff(task, mockRouter as any)).rejects.toThrow(
        "SSH connection lost during diff",
      );
    });

    it("rejects an incomplete RPC response instead of inventing file identity", async () => {
      const task = injectRemoteTask(orchestrator);

      mockRouter.nextResult = {
        diff: "diff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -1 +1,2 @@\n hello\n+world\n",
      };

      await expect(computeRemoteDiff(task, mockRouter as any)).rejects.toThrow();
    });
  });

  describe("orchestrator.getRpcRouter()", () => {
    it("returns the router when set", () => {
      expect(orchestrator.getRpcRouter()).toBe(mockRouter);
    });

    it("returns undefined when no router is configured", () => {
      const noRouterOrch = new Orchestrator({ persistPath: createTempPersistPath() });
      expect(noRouterOrch.getRpcRouter()).toBeUndefined();
    });
  });

  describe("RemoteOrchestrator.computeDiff integration", () => {
    it("computeDiff in ssh-orchestrator-remote.ts sends correct RPC", async () => {
      // Import the RemoteOrchestrator to verify its computeDiff method
      // routes correctly through the router.
      const { RemoteOrchestrator } = await import("../electron/ssh-orchestrator-remote");

      const remoteOrch = new RemoteOrchestrator(mockRouter as any, {} as any);

      // Create a task in the remote orchestrator.
      mockRouter.nextResult = { path: "/home/user/worktrees/task-abc", branch: "agent/remote-fixture", base_ref: "a".repeat(40) };
      const task = await remoteOrch.createTask({
        repoId: "ssh://gpu-server/home/user/repos/project",
        prompt: "Fix the bug",
        agentType: "claude-code",
        baseRef: "HEAD",
      });

      // Now compute the diff.
      mockRouter.calls = [];
      mockRouter.nextResult = { diff: "diff --git a/f.txt b/f.txt\n+new\n" };

      const diffText = await remoteOrch.computeDiff(task.id);

      const diffCall = mockRouter.calls.find((c) => c.method === "task_compute_diff");
      expect(diffCall).toBeDefined();
      expect(diffCall!.profileId).toBe("gpu-server");
      expect(diffCall!.params).toMatchObject({
        repo_id: task.repoId,
        task_id: task.id,
        worktree_path: task.worktreePath,
        branch: task.branch,
        base_ref: task.baseRef,
      });
      expect(diffText).toContain("diff --git");
    });
  });
});
