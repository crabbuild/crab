/**
 * Tests for remote workspace scripts execution.
 *
 * Verifies that setup/run/archive scripts execute on the remote machine
 * via the `task_run_script` RPC, that script output streams back as
 * notifications. These mocks do not qualify native sidecar script execution.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { EventEmitter } from "node:events";

import {
  RemoteScriptRunner,
  loadRemoteScripts,
  buildRemoteWorkspaceEnv,
} from "../electron/ssh-remote-scripts";
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
  results = new Map<string, unknown>();
  shouldFail = false;
  failError = "Connection lost";
  failMethods = new Set<string>();

  setResult(method: string, result: unknown): void {
    this.results.set(method, result);
  }

  async sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    this.calls.push({ profileId, method, params });

    if (this.shouldFail || this.failMethods.has(method)) {
      throw new Error(this.failError);
    }

    const result = this.results.get(method);
    if (result !== undefined) return result;

    // Default results for common methods.
    if (method === "task_run_script") {
      return { exit_code: 0, output: "Script completed successfully\n" };
    }
    if (method === "read_text") {
      throw new Error("File not found");
    }
    if (method === "task_spawn") {
      return {};
    }

    return {};
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
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-remote-scripts-test-"));
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
    prompt: "Set up ML environment and train model",
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

  (orchestrator as any).tasks.set(id, task);
  return task;
}

// ─── Tests: RemoteScriptRunner ───────────────────────────────────────────────

describe("RemoteScriptRunner", () => {
  let mockRouter: MockRpcRouter;
  let runner: RemoteScriptRunner;

  beforeEach(() => {
    mockRouter = new MockRpcRouter();
    runner = new RemoteScriptRunner(mockRouter as any);
  });

  describe("runScript", () => {
    it("sends task_run_script RPC with correct params", async () => {
      await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "setup",
        command: "pip install -r requirements.txt",
        worktreePath: "/home/user/worktrees/agent-abc",
        env: { CUDA_VISIBLE_DEVICES: "0,1" },
      });

      expect(mockRouter.calls).toHaveLength(1);
      expect(mockRouter.calls[0]).toEqual({
        profileId: "gpu-server",
        method: "task_run_script",
        params: {
          task_id: "task-1",
          script_type: "setup",
          command: "pip install -r requirements.txt",
          worktree_path: "/home/user/worktrees/agent-abc",
          env: { CUDA_VISIBLE_DEVICES: "0,1" },
        },
      });
    });

    it("returns success result when script exits with code 0", async () => {
      mockRouter.setResult("task_run_script", {
        exit_code: 0,
        output: "Successfully installed torch-2.0\n",
      });

      const result = await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "setup",
        command: "pip install torch",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      expect(result.success).toBe(true);
      expect(result.exitCode).toBe(0);
      expect(result.output).toBe("Successfully installed torch-2.0\n");
      expect(result.error).toBeUndefined();
    });

    it("returns failure result when script exits with non-zero code", async () => {
      mockRouter.setResult("task_run_script", {
        exit_code: 1,
        output: "ERROR: Could not find a version that satisfies the requirement\n",
        error: "pip install failed",
      });

      const result = await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "setup",
        command: "pip install nonexistent-package",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      expect(result.success).toBe(false);
      expect(result.exitCode).toBe(1);
      expect(result.error).toBe("pip install failed");
    });

    it("returns failure result when RPC call fails", async () => {
      mockRouter.shouldFail = true;
      mockRouter.failError = "SSH connection timeout";

      const result = await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "setup",
        command: "pip install torch",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      expect(result.success).toBe(false);
      expect(result.exitCode).toBe(-1);
      expect(result.error).toContain("RPC failed: SSH connection timeout");
    });

    it("skips empty scripts and returns success", async () => {
      const result = await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "setup",
        command: "   ",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      expect(result.success).toBe(true);
      expect(result.exitCode).toBe(0);
      expect(mockRouter.calls).toHaveLength(0);
    });

    it("emits script:start event", async () => {
      const events: any[] = [];
      runner.on("script:start", (e) => events.push(e));

      await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "setup",
        command: "conda activate ml",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({
        taskId: "task-1",
        scriptType: "setup",
        command: "conda activate ml",
      });
    });

    it("emits script:complete event with result", async () => {
      const events: any[] = [];
      runner.on("script:complete", (e) => events.push(e));

      mockRouter.setResult("task_run_script", {
        exit_code: 0,
        output: "done\n",
      });

      await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "archive",
        command: "tar czf artifacts.tar.gz ./output",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      expect(events).toHaveLength(1);
      expect(events[0].taskId).toBe("task-1");
      expect(events[0].scriptType).toBe("archive");
      expect(events[0].result.success).toBe(true);
    });

    it("passes empty env when none provided", async () => {
      await runner.runScript({
        taskId: "task-1",
        profileId: "gpu-server",
        scriptType: "run",
        command: "make test",
        worktreePath: "/home/user/worktrees/agent-abc",
      });

      const params = mockRouter.calls[0].params as any;
      expect(params.env).toEqual({});
    });
  });

  describe("handleScriptNotification", () => {
    it("emits script:output for task_script_output notifications", () => {
      const events: any[] = [];
      runner.on("script:output", (e) => events.push(e));

      runner.handleScriptNotification({
        profileId: "gpu-server",
        method: "task_script_output",
        params: {
          task_id: "task-1",
          script_type: "setup",
          stream: "stdout",
          data: "Installing dependencies...\n",
        },
      });

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({
        taskId: "task-1",
        scriptType: "setup",
        type: "stdout",
        data: "Installing dependencies...\n",
      });
    });

    it("handles stderr output", () => {
      const events: any[] = [];
      runner.on("script:output", (e) => events.push(e));

      runner.handleScriptNotification({
        profileId: "gpu-server",
        method: "task_script_output",
        params: {
          task_id: "task-1",
          script_type: "setup",
          stream: "stderr",
          data: "WARNING: pip is outdated\n",
        },
      });

      expect(events).toHaveLength(1);
      expect(events[0].type).toBe("stderr");
    });

    it("ignores non-script notifications", () => {
      const events: any[] = [];
      runner.on("script:output", (e) => events.push(e));

      runner.handleScriptNotification({
        profileId: "gpu-server",
        method: "task_event",
        params: { task_id: "task-1", event: { type: "text" } },
      });

      expect(events).toHaveLength(0);
    });

    it("ignores notifications with missing fields", () => {
      const events: any[] = [];
      runner.on("script:output", (e) => events.push(e));

      runner.handleScriptNotification({
        profileId: "gpu-server",
        method: "task_script_output",
        params: { task_id: "task-1" }, // missing script_type, stream, data
      });

      expect(events).toHaveLength(0);
    });
  });

  describe("convenience methods", () => {
    it("runSetupScript sends correct script_type", async () => {
      await runner.runSetupScript(
        "task-1", "gpu-server", "/worktree", "pip install -r requirements.txt",
      );

      const params = mockRouter.calls[0].params as any;
      expect(params.script_type).toBe("setup");
    });

    it("runRunScript sends correct script_type", async () => {
      await runner.runRunScript(
        "task-1", "gpu-server", "/worktree", "make test",
      );

      const params = mockRouter.calls[0].params as any;
      expect(params.script_type).toBe("run");
    });

    it("runArchiveScript sends correct script_type", async () => {
      await runner.runArchiveScript(
        "task-1", "gpu-server", "/worktree", "tar czf output.tar.gz ./results",
      );

      const params = mockRouter.calls[0].params as any;
      expect(params.script_type).toBe("archive");
    });
  });
});

// ─── Tests: loadRemoteScripts ────────────────────────────────────────────────

describe("loadRemoteScripts", () => {
  let mockRouter: MockRpcRouter;

  beforeEach(() => {
    mockRouter = new MockRpcRouter();
  });

  it("loads scripts from remote crab-orchestrator.json", async () => {
    mockRouter.setResult("read_text", {
      content: JSON.stringify({
        scripts: {
          setup: "pip install -r requirements.txt",
          run: "python train.py",
          archive: "tar czf model.tar.gz ./checkpoints",
        },
      }),
    });

    const scripts = await loadRemoteScripts(
      mockRouter as any,
      "gpu-server",
      "ssh://gpu-server/home/user/repos/ml",
      "/home/user/repos/ml",
      { setupScript: "", runScript: "", archiveScript: "" },
    );

    expect(scripts.setupScript).toBe("pip install -r requirements.txt");
    expect(scripts.runScript).toBe("python train.py");
    expect(scripts.archiveScript).toBe("tar czf model.tar.gz ./checkpoints");
  });

  it("falls back to defaults when config file is missing", async () => {
    // Default mock throws for read_text (file not found).
    const defaults = {
      setupScript: "default-setup",
      runScript: "default-run",
      archiveScript: "default-archive",
    };

    const scripts = await loadRemoteScripts(
      mockRouter as any,
      "gpu-server",
      "ssh://gpu-server/home/user/repos/ml",
      "/home/user/repos/ml",
      defaults,
    );

    expect(scripts).toEqual(defaults);
  });

  it("merges partial config with defaults", async () => {
    mockRouter.setResult("read_text", {
      content: JSON.stringify({
        scripts: {
          setup: "conda activate ml && pip install -r requirements.txt",
        },
      }),
    });

    const defaults = {
      setupScript: "default-setup",
      runScript: "default-run",
      archiveScript: "default-archive",
    };

    const scripts = await loadRemoteScripts(
      mockRouter as any,
      "gpu-server",
      "ssh://gpu-server/home/user/repos/ml",
      "/home/user/repos/ml",
      defaults,
    );

    expect(scripts.setupScript).toBe("conda activate ml && pip install -r requirements.txt");
    expect(scripts.runScript).toBe("default-run");
    expect(scripts.archiveScript).toBe("default-archive");
  });

  it("falls back to defaults on malformed JSON", async () => {
    mockRouter.setResult("read_text", {
      content: "not valid json {{{",
    });

    const defaults = {
      setupScript: "fallback",
      runScript: "",
      archiveScript: "",
    };

    const scripts = await loadRemoteScripts(
      mockRouter as any,
      "gpu-server",
      "ssh://gpu-server/home/user/repos/ml",
      "/home/user/repos/ml",
      defaults,
    );

    expect(scripts).toEqual(defaults);
  });
});

// ─── Tests: buildRemoteWorkspaceEnv ──────────────────────────────────────────

describe("buildRemoteWorkspaceEnv", () => {
  it("builds correct environment variables", () => {
    const env = buildRemoteWorkspaceEnv(
      "/home/user/worktrees/agent-abc",
      "agent/abc12345",
      "/home/user/repos/ml-project",
      "task-uuid-123",
    );

    expect(env).toEqual({
      CRAB_WORKSPACE_PATH: "/home/user/worktrees/agent-abc",
      CRAB_WORKSPACE_BRANCH: "agent/abc12345",
      CRAB_ROOT_PATH: "/home/user/repos/ml-project",
      CRAB_TASK_ID: "task-uuid-123",
      CRAB_REMOTE: "true",
    });
  });
});

// ─── Tests: Orchestrator Integration ─────────────────────────────────────────

describe("Orchestrator remote archive request mapping", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;
  let mockRouter: MockRpcRouter;

  beforeEach(() => {
    vi.useFakeTimers();
    persistPath = createTempPersistPath();
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(path.dirname(persistPath), "settings.json"));
    mockRouter = new MockRpcRouter();
    orchestrator = new Orchestrator({
      persistPath,
      timeoutMs: 60_000,
      rpcRouter: mockRouter as any,
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllEnvs();
    cleanupPath(persistPath);
  });

  describe("archive script", () => {
    it("runs archive script for remote tasks", async () => {
      mockRouter.setResult("read_text", {
        content: JSON.stringify({
          scripts: { archive: "tar czf artifacts.tar.gz ./output" },
        }),
      });
      mockRouter.setResult("task_run_script", {
        exit_code: 0,
        output: "Archive created\n",
      });

      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      task.status = "reviewing";

      const result = await orchestrator.runRemoteArchiveScript(task.id);

      expect(result).not.toBeNull();
      expect(result!.success).toBe(true);

      const scriptCall = mockRouter.calls.find((c) => c.method === "task_run_script");
      expect(scriptCall).toBeDefined();
      const params = scriptCall!.params as any;
      expect(params.script_type).toBe("archive");
      expect(params.command).toBe("tar czf artifacts.tar.gz ./output");
    });

    it("returns null when no archive script is configured", async () => {
      const task = injectRemoteTask(orchestrator, { status: "reviewing" });
      task.status = "reviewing";

      const result = await orchestrator.runRemoteArchiveScript(task.id);
      expect(result).toBeNull();
    });

    it("returns null for non-remote tasks", async () => {
      const task = injectRemoteTask(orchestrator, {
        isRemote: false,
        status: "reviewing",
      });
      task.status = "reviewing";

      const result = await orchestrator.runRemoteArchiveScript(task.id);
      expect(result).toBeNull();
    });
  });

});
