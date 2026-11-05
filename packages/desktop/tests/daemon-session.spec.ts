import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { Session } from "../electron/daemon/session";
import type { SubprocessHandle, StreamClient } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Mock SubprocessHandle — enables testability without node-pty
// ---------------------------------------------------------------------------

class MockSubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];
  private _disposed = false;
  private _paused = false;

  // Test inspection
  writtenData: string[] = [];
  resizes: Array<{ cols: number; rows: number }> = [];
  signals: string[] = [];
  killCalled = false;
  forceKillCalled = false;

  constructor(pid = 1234) {
    this.pid = pid;
    this.fd = pid + 100;
  }

  get disposed(): boolean {
    return this._disposed;
  }

  get paused(): boolean {
    return this._paused;
  }

  write(data: string): void {
    if (this._disposed) return;
    this.writtenData.push(data);
  }

  resize(cols: number, rows: number): void {
    if (this._disposed) return;
    this.resizes.push({ cols, rows });
  }

  kill(signal?: string): void {
    if (this._disposed) return;
    this.killCalled = true;
    this.signals.push(signal ?? "SIGTERM");
  }

  forceKill(): void {
    this.forceKillCalled = true;
  }

  signal(sig: string): void {
    if (this._disposed) return;
    this.signals.push(sig);
  }

  pause(): void {
    if (this._disposed) return;
    this._paused = true;
  }

  resume(): void {
    if (this._disposed) return;
    this._paused = false;
  }

  onData(cb: (data: string) => void): void {
    this.dataCallbacks.push(cb);
  }

  onExit(cb: (exitCode: number) => void): void {
    this.exitCallbacks.push(cb);
  }

  dispose(): void {
    this._disposed = true;
    this.dataCallbacks = [];
    this.exitCallbacks = [];
  }

  // Test helpers: simulate PTY events
  emitData(data: string): void {
    for (const cb of [...this.dataCallbacks]) {
      cb(data);
    }
  }

  emitExit(code: number): void {
    for (const cb of [...this.exitCallbacks]) {
      cb(code);
    }
  }
}

// ---------------------------------------------------------------------------
// Mock StreamClient
// ---------------------------------------------------------------------------

function createMockClient(id = "client-1"): StreamClient & {
  receivedData: Array<{ sessionId: string; data: Buffer }>;
  receivedExits: Array<{ sessionId: string; code: number }>;
  receivedStateChanges: Array<{
    sessionId: string;
    state: string;
    previousState: string;
    timestamp: number;
  }>;
} {
  const client = {
    id,
    receivedData: [] as Array<{ sessionId: string; data: Buffer }>,
    receivedExits: [] as Array<{ sessionId: string; code: number }>,
    receivedStateChanges: [] as Array<{
      sessionId: string;
      state: string;
      previousState: string;
      timestamp: number;
    }>,
    sendData(sessionId: string, data: Buffer) {
      client.receivedData.push({ sessionId, data });
    },
    sendExit(sessionId: string, code: number) {
      client.receivedExits.push({ sessionId, code });
    },
    sendStateChanged(
      sessionId: string,
      state: string,
      previousState: string,
      timestamp: number,
    ) {
      client.receivedStateChanges.push({ sessionId, state, previousState, timestamp });
    },
  };
  return client as unknown as StreamClient & typeof client;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Session", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // -------------------------------------------------------------------------
  // State transitions
  // -------------------------------------------------------------------------

  describe("state transitions", () => {
    it("starts in running state after construction", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      expect(session.state).toBe("running");
      expect(session.isAlive).toBe(true);
    });

    it("transitions to exiting on kill()", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      expect(session.state).toBe("exiting");
    });

    it("transitions to exited on natural subprocess exit", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      subprocess.emitExit(0);
      expect(session.state).toBe("exited");
      expect(session.exitCode).toBe(0);
      expect(session.isAlive).toBe(false);
    });

    it("transitions to exited after kill timeout (SIGKILL)", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      expect(session.state).toBe("exiting");

      // Advance past the 5s kill timer
      vi.advanceTimersByTime(5000);

      expect(session.state).toBe("exited");
      expect(subprocess.forceKillCalled).toBe(true);
      expect(session.exitCode).toBe(-1);
    });

    it("kill() is a no-op when not in running state", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      subprocess.emitExit(0);
      session.kill(); // should not throw or change state
      expect(session.state).toBe("exited");
    });

    it("natural exit during exiting state clears kill timer", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      expect(session.state).toBe("exiting");

      // Process exits before the 5s timer
      subprocess.emitExit(143); // SIGTERM exit code

      expect(session.state).toBe("exited");
      expect(session.exitCode).toBe(143);
      expect(subprocess.forceKillCalled).toBe(false);

      // Advancing time should not trigger forceKill
      vi.advanceTimersByTime(5000);
      expect(subprocess.forceKillCalled).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // Shell-ready queuing
  // -------------------------------------------------------------------------

  describe("shell-ready queuing", () => {
    it("queues stdin writes when shellReadySupported and state is pending", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: true,
      });

      session.write("echo hello\n");
      session.write("ls\n");

      // Data should be queued, not written to subprocess
      expect(subprocess.writtenData).toHaveLength(0);
      expect(session.shellState).toBe("pending");
    });

    it("flushes stdin queue after shell-ready marker + gate delay", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: true,
      });

      session.write("echo hello\n");

      // Emit the shell-ready marker
      subprocess.emitData("\x1b]777;crab-shell-ready\x07");
      expect(session.shellState).toBe("ready");

      // Queue not flushed yet — gate is armed, waiting for prompt data + delay
      expect(subprocess.writtenData).toHaveLength(0);

      // Simulate prompt data arriving
      subprocess.emitData("$ ");

      // Still waiting for 30ms delay
      vi.advanceTimersByTime(29);
      expect(subprocess.writtenData).toHaveLength(0);

      // 30ms after data — flush fires
      vi.advanceTimersByTime(1);
      expect(subprocess.writtenData).toEqual(["echo hello\n"]);
    });

    it("flushes stdin queue on shell-ready timeout (15s)", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: true,
      });

      session.write("echo hello\n");

      // Advance past the 15s shell-ready timeout
      vi.advanceTimersByTime(15_000);

      expect(session.shellState).toBe("timed_out");
      expect(subprocess.writtenData).toEqual(["echo hello\n"]);
    });

    it("writes directly when shellReadySupported is false", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.write("echo hello\n");
      expect(subprocess.writtenData).toEqual(["echo hello\n"]);
      expect(session.shellState).toBe("unsupported");
    });

    it("flushes via gate fallback when no data arrives after marker", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: true,
      });

      session.write("cmd\n");

      // Emit marker (prompt is in the same chunk — no separate data event)
      subprocess.emitData("\x1b]777;crab-shell-ready\x07");

      // Gate fallback fires at 50ms
      vi.advanceTimersByTime(50);
      expect(subprocess.writtenData).toEqual(["cmd\n"]);
    });
  });

  // -------------------------------------------------------------------------
  // Kill timer and fd release
  // -------------------------------------------------------------------------

  describe("kill timer and fd release", () => {
    it("sends SIGTERM on kill()", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      expect(subprocess.killCalled).toBe(true);
      expect(subprocess.signals).toContain("SIGTERM");
    });

    it("sends SIGKILL after 5s timeout if process does not exit", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      vi.advanceTimersByTime(4999);
      expect(subprocess.forceKillCalled).toBe(false);

      vi.advanceTimersByTime(1);
      expect(subprocess.forceKillCalled).toBe(true);
    });

    it("disposes subprocess on natural exit (fd release)", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      subprocess.emitExit(0);
      expect(subprocess.disposed).toBe(true);
    });

    it("disposes subprocess on kill timeout (fd release)", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      vi.advanceTimersByTime(5000);
      expect(subprocess.disposed).toBe(true);
    });

    it("forceKillAndDisposeSubprocess releases fd immediately", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.forceKillAndDisposeSubprocess();
      expect(subprocess.forceKillCalled).toBe(true);
      expect(subprocess.disposed).toBe(true);
      expect(session.state).toBe("exited");
    });

    it("forceKill is called BEFORE dispose on kill timeout path", () => {
      const subprocess = new MockSubprocessHandle();
      const callOrder: string[] = [];

      // Track call order
      const origForceKill = subprocess.forceKill.bind(subprocess);
      subprocess.forceKill = () => {
        callOrder.push("forceKill");
        origForceKill();
      };
      const origDispose = subprocess.dispose.bind(subprocess);
      subprocess.dispose = () => {
        callOrder.push("dispose");
        origDispose();
      };

      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.kill();
      vi.advanceTimersByTime(5000);

      expect(callOrder).toEqual(["forceKill", "dispose"]);
    });

    it("forceKill is called BEFORE dispose on graceful shutdown path", () => {
      const subprocess = new MockSubprocessHandle();
      const callOrder: string[] = [];

      const origForceKill = subprocess.forceKill.bind(subprocess);
      subprocess.forceKill = () => {
        callOrder.push("forceKill");
        origForceKill();
      };
      const origDispose = subprocess.dispose.bind(subprocess);
      subprocess.dispose = () => {
        callOrder.push("dispose");
        origDispose();
      };

      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.forceKillAndDisposeSubprocess();
      expect(callOrder).toEqual(["forceKill", "dispose"]);
    });
  });

  // -------------------------------------------------------------------------
  // Client attach/detach
  // -------------------------------------------------------------------------

  describe("client attach/detach", () => {
    it("attaches a client and returns a symbol token", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const client = createMockClient();
      const token = session.attachClient(client);

      expect(typeof token).toBe("symbol");
      expect(session.clientCount).toBe(1);
    });

    it("broadcasts data to all attached clients", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");
      session.attachClient(client1);
      session.attachClient(client2);

      subprocess.emitData("hello");

      expect(client1.receivedData).toHaveLength(1);
      expect(client2.receivedData).toHaveLength(1);
      expect(client1.receivedData[0]!.data.toString()).toBe("hello");
      expect(client2.receivedData[0]!.data.toString()).toBe("hello");
    });

    it("detaches a specific client by token", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");
      const token1 = session.attachClient(client1);
      session.attachClient(client2);

      session.detachClient(token1);
      expect(session.clientCount).toBe(1);

      subprocess.emitData("after detach");
      expect(client1.receivedData).toHaveLength(0);
      expect(client2.receivedData).toHaveLength(1);
    });

    it("detachAllClients removes all clients", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.attachClient(createMockClient("c1"));
      session.attachClient(createMockClient("c2"));
      expect(session.clientCount).toBe(2);

      session.detachAllClients();
      expect(session.clientCount).toBe(0);
    });

    it("notifies attached clients on exit", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const client = createMockClient();
      session.attachClient(client);

      subprocess.emitExit(42);

      expect(client.receivedExits).toHaveLength(1);
      expect(client.receivedExits[0]).toEqual({ sessionId: "test-1", code: 42 });
    });
  });

  // -------------------------------------------------------------------------
  // Session operations
  // -------------------------------------------------------------------------

  describe("session operations", () => {
    it("resize updates dimensions and forwards to subprocess", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.resize(120, 40);
      expect(session.cols).toBe(120);
      expect(session.rows).toBe(40);
      expect(subprocess.resizes).toEqual([{ cols: 120, rows: 40 }]);
    });

    it("resize clamps to minimum 1×1", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.resize(0, -5);
      expect(session.cols).toBe(1);
      expect(session.rows).toBe(1);
      expect(subprocess.resizes).toEqual([{ cols: 1, rows: 1 }]);
    });

    it("signal forwards to subprocess", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.signal("SIGINT");
      expect(subprocess.signals).toContain("SIGINT");
    });

    it("write is a no-op when session is not running", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      subprocess.emitExit(0);
      session.write("should not arrive");
      expect(subprocess.writtenData).toHaveLength(0);
    });

    it("resize is a no-op when session is not running", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      subprocess.emitExit(0);
      session.resize(200, 50);
      expect(subprocess.resizes).toHaveLength(0);
    });

    it("stores agentMeta", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
        agentMeta: {
          taskId: "task-123",
          agentType: "claude",
          worktreePath: "/path/to/worktree",
        },
      });

      expect(session.agentMeta).toEqual({
        taskId: "task-123",
        agentType: "claude",
        worktreePath: "/path/to/worktree",
      });
    });

    it("getCwd returns null initially", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      expect(session.getCwd()).toBeNull();
    });

    it("getCwd returns cwd after OSC-7 is received", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const cwd = resolve("project");
      subprocess.emitData(`\x1b]7;${pathToFileURL(cwd)}\x07`);

      // HeadlessEmulator processes OSC-7 synchronously in scanOsc7
      expect(session.getCwd()).toBe(cwd);
    });
  });

  // -------------------------------------------------------------------------
  // onExit callback
  // -------------------------------------------------------------------------

  describe("onExit callback", () => {
    it("invokes onExit callback on natural exit", () => {
      const subprocess = new MockSubprocessHandle();
      const onExit = vi.fn();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
        onExit,
      });

      subprocess.emitExit(0);
      expect(onExit).toHaveBeenCalledWith("test-1", 0);
    });

    it("invokes onExit callback on kill timeout", () => {
      const subprocess = new MockSubprocessHandle();
      const onExit = vi.fn();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
        onExit,
      });

      session.kill();
      vi.advanceTimersByTime(5000);
      expect(onExit).toHaveBeenCalledWith("test-1", -1);
    });

    it("invokes onExit callback on graceful shutdown", () => {
      const subprocess = new MockSubprocessHandle();
      const onExit = vi.fn();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
        onExit,
      });

      session.forceKillAndDisposeSubprocess();
      expect(onExit).toHaveBeenCalledWith("test-1", -1);
    });
  });

  // -------------------------------------------------------------------------
  // Subprocess mock injection
  // -------------------------------------------------------------------------

  describe("subprocess mock injection", () => {
    it("accepts any SubprocessHandle implementation", () => {
      const subprocess = new MockSubprocessHandle(9999);
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      expect(session.pid).toBe(9999);
      expect(session.state).toBe("running");
    });

    it("data flows through mock subprocess to clients", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const client = createMockClient();
      session.attachClient(client);

      subprocess.emitData("test output");
      expect(client.receivedData[0]!.data.toString()).toBe("test output");
    });

    it("exit flows through mock subprocess to session state", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      subprocess.emitExit(130);
      expect(session.state).toBe("exited");
      expect(session.exitCode).toBe(130);
    });
  });

  // -------------------------------------------------------------------------
  // Edge cases
  // -------------------------------------------------------------------------

  describe("edge cases", () => {
    it("forceKillAndDisposeSubprocess is idempotent", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      session.forceKillAndDisposeSubprocess();
      // Second call should not throw
      session.forceKillAndDisposeSubprocess();
      expect(session.state).toBe("exited");
    });

    it("data after exit is ignored", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: false,
      });

      const client = createMockClient();
      session.attachClient(client);

      subprocess.emitExit(0);

      // Simulate late data (should not crash or deliver)
      subprocess.emitData("late data");
      // Client only got the exit, no data after exit
      expect(client.receivedData).toHaveLength(0);
    });

    it("multiple writes are queued and flushed in order", () => {
      const subprocess = new MockSubprocessHandle();
      const session = new Session({
        sessionId: "test-1",
        cols: 80,
        rows: 24,
        subprocess,
        shellReadySupported: true,
      });

      session.write("first\n");
      session.write("second\n");
      session.write("third\n");

      // Trigger shell-ready
      subprocess.emitData("\x1b]777;crab-shell-ready\x07");
      // Trigger prompt data + wait for gate
      subprocess.emitData("$ ");
      vi.advanceTimersByTime(30);

      expect(subprocess.writtenData).toEqual(["first\n", "second\n", "third\n"]);
    });
  });
});
