import { describe, it, expect, vi, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type * as child_process from "node:child_process";

import {
  prepareUpgrade,
  sendUpgradeAck,
  sendUpgradeNak,
  bindSocketWithRetry,
  UPGRADE_ACK_TIMEOUT_MS,
} from "../electron/daemon/prepare-upgrade";
import type { LiveSessionInfo } from "../electron/daemon/fd-handoff";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "prepare-upgrade-test-"));
}

function makeSession(overrides: Partial<LiveSessionInfo> = {}): LiveSessionInfo {
  return {
    id: overrides.id ?? "repo::/wt/test@@abc123",
    pid: overrides.pid ?? 12345,
    cwd: "cwd" in overrides ? overrides.cwd! : "/home/user/project",
    cols: overrides.cols ?? 80,
    rows: overrides.rows ?? 24,
    agentMeta: overrides.agentMeta,
    pendingBuffer: overrides.pendingBuffer,
  };
}

// ---------------------------------------------------------------------------
// Mock child process
// ---------------------------------------------------------------------------

interface MockChildProcess {
  pid: number;
  killed: boolean;
  unrefCalled: boolean;
  disconnectCalled: boolean;
  channel: object | null;
  emitMessage: (msg: unknown) => void;
  emitError: (err: Error) => void;
  emitExit: (code: number | null) => void;
  on: (event: string, cb: (...args: unknown[]) => void) => MockChildProcess;
  removeListener: (event: string, cb: (...args: unknown[]) => void) => MockChildProcess;
  kill: (signal?: string) => boolean;
  unref: () => void;
  disconnect: () => void;
}

function createMockChild(): MockChildProcess {
  const listeners: Map<string, Set<(...args: unknown[]) => void>> = new Map();

  const mock: MockChildProcess = {
    pid: 99999,
    killed: false,
    unrefCalled: false,
    disconnectCalled: false,
    channel: {},

    emitMessage(msg: unknown) {
      const cbs = listeners.get("message");
      if (cbs) {
        for (const cb of cbs) cb(msg);
      }
    },

    emitError(err: Error) {
      const cbs = listeners.get("error");
      if (cbs) {
        for (const cb of cbs) cb(err);
      }
    },

    emitExit(code: number | null) {
      const cbs = listeners.get("exit");
      if (cbs) {
        for (const cb of cbs) cb(code);
      }
    },

    on(event: string, cb: (...args: unknown[]) => void) {
      if (!listeners.has(event)) listeners.set(event, new Set());
      listeners.get(event)!.add(cb);
      return mock;
    },

    removeListener(event: string, cb: (...args: unknown[]) => void) {
      listeners.get(event)?.delete(cb);
      return mock;
    },

    kill(_signal?: string) {
      mock.killed = true;
      return true;
    },

    unref() {
      mock.unrefCalled = true;
    },

    disconnect() {
      mock.disconnectCalled = true;
    },
  };

  return mock;
}

/**
 * Create a mock fork function that returns the given mock child.
 * Also records the arguments it was called with.
 */
function createMockFork(mockChild: MockChildProcess) {
  const calls: Array<{
    modulePath: string;
    args: string[];
    options: child_process.ForkOptions;
  }> = [];

  const forkFn = (
    modulePath: string,
    args?: readonly string[],
    options?: child_process.ForkOptions,
  ) => {
    calls.push({
      modulePath,
      args: args as string[],
      options: options ?? {},
    });
    return mockChild as unknown as child_process.ChildProcess;
  };

  return { forkFn: forkFn as typeof child_process.fork, calls };
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

const tmpDirs: string[] = [];

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  for (const dir of tmpDirs) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
  tmpDirs.length = 0;
});

// ---------------------------------------------------------------------------
// prepareUpgrade
// ---------------------------------------------------------------------------

describe("prepareUpgrade", () => {
  it("writes snapshot to disk before spawning successor", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const sessions = [makeSession({ id: "s1", pid: 1000 }), makeSession({ id: "s2", pid: 2000 })];

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-ack" }), 50);

    const result = await prepareUpgrade({
      successorPath: "/path/to/new-daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions,
      ptyFds: [10, 11],
      snapshotDir: dir,
      forkFn,
    });

    expect(result.success).toBe(true);

    // Verify snapshot was written
    const snapshotPath = path.join(dir, "handoff-snapshot.json");
    expect(fs.existsSync(snapshotPath)).toBe(true);

    const snapshot = JSON.parse(fs.readFileSync(snapshotPath, "utf-8"));
    expect(snapshot.sessions).toHaveLength(2);
    expect(snapshot.sessions[0].id).toBe("s1");
    expect(snapshot.sessions[1].id).toBe("s2");
  });

  it("spawns successor with correct stdio layout", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const sessions = [makeSession({ id: "s1", pid: 1000 })];
    const ptyFds = [42];

    const mockChild = createMockChild();
    const { forkFn, calls } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-ack" }), 50);

    await prepareUpgrade({
      successorPath: "/path/to/new-daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions,
      ptyFds,
      snapshotDir: dir,
      forkFn,
    });

    expect(calls).toHaveLength(1);
    expect(calls[0].modulePath).toBe("/path/to/new-daemon.js");
    expect(calls[0].args).toContain("--handoff");
    expect(calls[0].args).toContain("--socket");
    expect(calls[0].args).toContain(path.join(dir, "daemon-v1.sock"));
    expect(calls[0].args).toContain("--snapshot");
    expect(calls[0].options.stdio).toEqual(["ignore", "inherit", "inherit", "ipc", 42]);
    expect(calls[0].options.detached).toBe(true);
    expect((calls[0].options.env as Record<string, string>).ELECTRON_RUN_AS_NODE).toBe("1");
  });

  it("returns success when successor sends upgrade-ack", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-ack" }), 20);

    const onSuccess = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      onSuccess,
      forkFn,
    });

    expect(result.success).toBe(true);
    expect(result.error).toBeUndefined();
    expect(onSuccess).toHaveBeenCalled();
  });

  it("returns failure when successor sends upgrade-nak", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-nak", error: "adoption failed" }), 20);

    const onFailure = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      onFailure,
      forkFn,
    });

    expect(result.success).toBe(false);
    expect(result.error).toBe("adoption failed");
    expect(onFailure).toHaveBeenCalledWith("adoption failed");
    expect(mockChild.killed).toBe(true);
  });

  it("returns failure on timeout and kills successor", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    // Use a short timeout for testing
    const onFailure = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      onFailure,
      forkFn,
      ackTimeoutMs: 100, // Short timeout for test
    });

    expect(result.success).toBe(false);
    expect(result.error).toContain("timed out");
    expect(onFailure).toHaveBeenCalled();
    expect(mockChild.killed).toBe(true);
  });

  it("returns failure when successor exits prematurely", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitExit(1), 20);

    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      forkFn,
    });

    expect(result.success).toBe(false);
    expect(result.error).toContain("exited prematurely");
  });

  it("returns failure when session count does not match fd count", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const onFailure = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession(), makeSession({ id: "s2" })],
      ptyFds: [10], // Only 1 fd for 2 sessions
      snapshotDir: dir,
      onFailure,
    });

    expect(result.success).toBe(false);
    expect(result.error).toContain("does not match fd count");
    expect(onFailure).toHaveBeenCalled();
  });

  it("returns failure when fork throws", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const forkFn = (() => {
      throw new Error("ENOENT: no such file");
    }) as unknown as typeof child_process.fork;

    const result = await prepareUpgrade({
      successorPath: "/nonexistent/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      forkFn,
    });

    expect(result.success).toBe(false);
    expect(result.error).toContain("Failed to spawn successor");
  });

  it("handles zero sessions (no-op upgrade)", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-ack" }), 20);

    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [],
      ptyFds: [],
      snapshotDir: dir,
      forkFn,
    });

    expect(result.success).toBe(true);

    // Snapshot should still be written (empty sessions list)
    const snapshotPath = path.join(dir, "handoff-snapshot.json");
    const snapshot = JSON.parse(fs.readFileSync(snapshotPath, "utf-8"));
    expect(snapshot.sessions).toHaveLength(0);
  });

  it("returns failure when successor process emits error", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitError(new Error("spawn EACCES")), 20);

    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      forkFn,
    });

    expect(result.success).toBe(false);
    expect(result.error).toContain("Successor process error");
  });

  it("disconnects IPC and unrefs successor on success", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-ack" }), 20);

    await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      forkFn,
    });

    expect(mockChild.unrefCalled).toBe(true);
    expect(mockChild.disconnectCalled).toBe(true);
  });

  it("includes multiple PTY fds in stdio array", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const sessions = [
      makeSession({ id: "s1", pid: 100 }),
      makeSession({ id: "s2", pid: 200 }),
      makeSession({ id: "s3", pid: 300 }),
    ];
    const ptyFds = [7, 8, 9];

    const mockChild = createMockChild();
    const { forkFn, calls } = createMockFork(mockChild);

    setTimeout(() => mockChild.emitMessage({ type: "upgrade-ack" }), 20);

    await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions,
      ptyFds,
      snapshotDir: dir,
      forkFn,
    });

    // stdio: [ignore, inherit, inherit, ipc, fd7, fd8, fd9]
    expect(calls[0].options.stdio).toEqual(["ignore", "inherit", "inherit", "ipc", 7, 8, 9]);
  });
});

// ---------------------------------------------------------------------------
// sendUpgradeAck / sendUpgradeNak
// ---------------------------------------------------------------------------

describe("sendUpgradeAck", () => {
  it("sends upgrade-ack via process.send", () => {
    const originalSend = process.send;
    const mockSend = vi.fn();
    process.send = mockSend;

    try {
      sendUpgradeAck();
      expect(mockSend).toHaveBeenCalledWith({ type: "upgrade-ack" });
    } finally {
      process.send = originalSend;
    }
  });

  it("does nothing when process.send is not available", () => {
    const originalSend = process.send;
    process.send = undefined;

    try {
      expect(() => sendUpgradeAck()).not.toThrow();
    } finally {
      process.send = originalSend;
    }
  });
});

describe("sendUpgradeNak", () => {
  it("sends upgrade-nak with error via process.send", () => {
    const originalSend = process.send;
    const mockSend = vi.fn();
    process.send = mockSend;

    try {
      sendUpgradeNak("failed to adopt sessions");
      expect(mockSend).toHaveBeenCalledWith({
        type: "upgrade-nak",
        error: "failed to adopt sessions",
      });
    } finally {
      process.send = originalSend;
    }
  });
});

// ---------------------------------------------------------------------------
// bindSocketWithRetry
// ---------------------------------------------------------------------------

describe("bindSocketWithRetry", () => {
  it("succeeds on first attempt", async () => {
    const bindFn = vi.fn().mockResolvedValue(undefined);

    await bindSocketWithRetry(bindFn, 3, 10);

    expect(bindFn).toHaveBeenCalledTimes(1);
  });

  it("retries on failure and succeeds eventually", async () => {
    const bindFn = vi.fn()
      .mockRejectedValueOnce(new Error("EADDRINUSE"))
      .mockRejectedValueOnce(new Error("EADDRINUSE"))
      .mockResolvedValue(undefined);

    await bindSocketWithRetry(bindFn, 5, 5);

    expect(bindFn).toHaveBeenCalledTimes(3);
  });

  it("throws after exhausting all retries", async () => {
    const bindFn = vi.fn().mockRejectedValue(new Error("EADDRINUSE"));

    await expect(bindSocketWithRetry(bindFn, 2, 5)).rejects.toThrow(
      "Failed to bind socket after 3 attempts: EADDRINUSE",
    );

    expect(bindFn).toHaveBeenCalledTimes(3); // initial + 2 retries
  });

  it("uses default retry parameters when none specified", async () => {
    const bindFn = vi.fn().mockResolvedValue(undefined);

    await bindSocketWithRetry(bindFn);

    expect(bindFn).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// Integration: sessions survive binary swap (simulated)
// ---------------------------------------------------------------------------

describe("integration: sessions survive binary swap", () => {
  it("full upgrade flow: serialize → spawn → ack → predecessor exits", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const sessions: LiveSessionInfo[] = [
      makeSession({ id: "wt1@@sess1", pid: 5001 }),
      makeSession({ id: "wt1@@sess2", pid: 5002 }),
      makeSession({ id: "wt2@@sess3", pid: 5003, agentMeta: { taskId: "t1", agentType: "claude", worktreePath: "/wt2" } }),
    ];
    const ptyFds = [20, 21, 22];

    const mockChild = createMockChild();
    const { forkFn, calls } = createMockFork(mockChild);

    // Simulate successor reading snapshot and sending ack
    setTimeout(() => {
      // Verify the snapshot file exists (successor would read it)
      const snapshotPath = path.join(dir, "handoff-snapshot.json");
      const snapshot = JSON.parse(fs.readFileSync(snapshotPath, "utf-8"));
      expect(snapshot.sessions).toHaveLength(3);
      expect(snapshot.sessions[0].fdIndex).toBe(4);
      expect(snapshot.sessions[1].fdIndex).toBe(5);
      expect(snapshot.sessions[2].fdIndex).toBe(6);

      // Successor sends ack
      mockChild.emitMessage({ type: "upgrade-ack" });
    }, 30);

    const onSuccess = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/new-daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions,
      ptyFds,
      snapshotDir: dir,
      onSuccess,
      forkFn,
    });

    expect(result.success).toBe(true);
    expect(onSuccess).toHaveBeenCalled();

    // Verify stdio layout includes all PTY fds
    expect(calls[0].options.stdio).toEqual(["ignore", "inherit", "inherit", "ipc", 20, 21, 22]);
  });

  it("predecessor continues on nak: sessions remain with predecessor", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const sessions = [makeSession({ id: "s1", pid: 3000 })];
    const ptyFds = [15];

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    // Successor fails to adopt and sends nak
    setTimeout(() => {
      mockChild.emitMessage({ type: "upgrade-nak", error: "snapshot version mismatch" });
    }, 20);

    const onFailure = vi.fn();
    const onSuccess = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions,
      ptyFds,
      snapshotDir: dir,
      onSuccess,
      onFailure,
      forkFn,
    });

    // Predecessor should continue (not exit)
    expect(result.success).toBe(false);
    expect(onSuccess).not.toHaveBeenCalled();
    expect(onFailure).toHaveBeenCalledWith("snapshot version mismatch");

    // Successor should be killed
    expect(mockChild.killed).toBe(true);
  });

  it("timeout scenario: predecessor continues after 5s", async () => {
    const dir = makeTmpDir();
    tmpDirs.push(dir);

    const mockChild = createMockChild();
    const { forkFn } = createMockFork(mockChild);

    // Successor never responds — use short timeout for test
    const onFailure = vi.fn();
    const onSuccess = vi.fn();
    const result = await prepareUpgrade({
      successorPath: "/path/to/daemon.js",
      socketPath: path.join(dir, "daemon-v1.sock"),
      sessions: [makeSession()],
      ptyFds: [10],
      snapshotDir: dir,
      onSuccess,
      onFailure,
      forkFn,
      ackTimeoutMs: 50, // Short timeout for test
    });

    expect(result.success).toBe(false);
    expect(result.error).toContain("timed out");
    expect(onSuccess).not.toHaveBeenCalled();
    expect(onFailure).toHaveBeenCalled();
    expect(mockChild.killed).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Constants verification
// ---------------------------------------------------------------------------

describe("constants", () => {
  it("UPGRADE_ACK_TIMEOUT_MS is 5 seconds", () => {
    expect(UPGRADE_ACK_TIMEOUT_MS).toBe(5000);
  });
});
