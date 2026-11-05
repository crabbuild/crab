import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  TerminalHost,
  SessionTombstonedError,
  SessionCapacityError,
  SessionNotFoundError,
} from "../electron/daemon/terminal-host";
import type { SubprocessHandle, StreamClient } from "../electron/daemon/types";
import type { SubprocessSpawner, SubprocessSpawnOptions } from "../electron/daemon/subprocess-handle";

// ---------------------------------------------------------------------------
// Mock SubprocessHandle
// ---------------------------------------------------------------------------

class MockSubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];
  private _disposed = false;

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

  pause(): void {}
  resume(): void {}

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

  // Test helpers
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
      _sessionId: string,
      _state: string,
      _previousState: string,
      _timestamp: number,
    ) {},
  };
  return client as unknown as StreamClient & typeof client;
}

// ---------------------------------------------------------------------------
// Mock SubprocessSpawner
// ---------------------------------------------------------------------------

function createMockSpawner(): {
  spawner: SubprocessSpawner;
  spawned: MockSubprocessHandle[];
  spawnCalls: SubprocessSpawnOptions[];
} {
  let nextPid = 1000;
  const spawned: MockSubprocessHandle[] = [];
  const spawnCalls: SubprocessSpawnOptions[] = [];

  const spawner: SubprocessSpawner = (opts: SubprocessSpawnOptions) => {
    spawnCalls.push(opts);
    const handle = new MockSubprocessHandle(nextPid++);
    spawned.push(handle);
    return handle;
  };

  return { spawner, spawned, spawnCalls };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("TerminalHost", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("uses the Windows command processor when no command is supplied", () => {
    const platformDescriptor = Object.getOwnPropertyDescriptor(process, "platform")!;
    const previousEnvironment = { SHELL: process.env.SHELL, COMSPEC: process.env.COMSPEC };
    const { spawner, spawnCalls } = createMockSpawner();
    const host = new TerminalHost({ spawnSubprocess: spawner });
    try {
      Object.defineProperty(process, "platform", { value: "win32" });
      delete process.env.SHELL;
      process.env.COMSPEC = "C:\\Windows\\System32\\cmd.exe";
      host.createOrAttach({ sessionId: "native-shell", cols: 80, rows: 24, streamClient: createMockClient() });
      expect(spawnCalls[0].file).toBe(process.env.COMSPEC);
    } finally {
      host.dispose();
      Object.defineProperty(process, "platform", platformDescriptor);
      for (const [key, value] of Object.entries(previousEnvironment)) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
    }
  });

  // -------------------------------------------------------------------------
  // createOrAttach idempotency
  // -------------------------------------------------------------------------

  describe("createOrAttach idempotency", () => {
    it("spawns a new session when none exists", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client = createMockClient();

      const result = host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client,
      });

      expect(result.isNew).toBe(true);
      expect(result.snapshot).toBeNull();
      expect(result.pid).toBe(1000);
      expect(spawned).toHaveLength(1);
      expect(host.sessionCount).toBe(1);
    });

    it("reattaches to existing live session (isNew: false)", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");

      // First attach — creates new session
      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client1,
      });

      // Second attach — reattaches to existing
      const result = host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client2,
      });

      expect(result.isNew).toBe(false);
      expect(result.pid).toBe(1000);
      // Only one subprocess was spawned
      expect(spawned).toHaveLength(1);
      expect(host.sessionCount).toBe(1);
    });

    it("detaches previous clients on reattach", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client1,
      });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client2,
      });

      // Emit data — only client2 should receive it
      spawned[0]!.emitData("hello");
      expect(client1.receivedData).toHaveLength(0);
      expect(client2.receivedData).toHaveLength(1);
    });

    it("treats exiting session as non-existent and spawns new", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client1,
      });

      // Put session into "exiting" state by calling kill on the session
      // We need to access the session through the host's kill method
      // but without tombstoning — let's use the session directly
      // Actually, the host.kill() adds a tombstone, so we need to
      // trigger the exiting state differently. Let's emit a kill on the
      // subprocess handle to put it in exiting state.
      // The Session.kill() transitions to "exiting" — we can trigger it
      // by having the host not tombstone. Let's just call session.kill()
      // through the spawned handle's signal.
      // Actually, let's just use the internal session. The design says
      // "exiting" state means kill is in flight. We can simulate this
      // by having the session receive a kill() call without tombstoning.
      // The simplest approach: directly call kill on the session via
      // a signal that triggers the exiting state.

      // Use a workaround: spawn, then trigger kill via the session's
      // own kill method (not through host.kill which adds tombstone)
      // We'll access the session through listSessions to verify state,
      // but we need to trigger exiting without tombstone.
      // Let's just test through the host.kill path but with a different
      // session ID for the reattach.

      // Better approach: create a host with a custom spawner that lets
      // us control the session state. The session transitions to "exiting"
      // when kill() is called on it. We can't call session.kill() directly
      // without the tombstone from host.kill(). But the design says:
      // "if session exists but is in exiting state → treat as non-existent,
      // dispose old, spawn new". This can happen if the session was killed
      // by a signal (not through host.kill) or if the kill was initiated
      // by the session itself.

      // For testing, let's just verify the behavior by having the session
      // enter exiting state through a direct signal. We'll use the fact
      // that Session.kill() sets state to "exiting".
      // We need to call kill on the session without going through host.kill.
      // Since we can't access the session directly, let's test this
      // differently: we'll kill through the host (which adds tombstone),
      // then verify that a new session with a DIFFERENT ID works.

      // Actually, re-reading the design: the "exiting" state scenario
      // happens when a session is being killed (SIGTERM sent, waiting for
      // exit) and a new createOrAttach comes in for the same ID.
      // The tombstone is only added by host.kill(). But what if the
      // session enters "exiting" through some other path? In practice,
      // the only way is through Session.kill(). And host.kill() always
      // adds a tombstone before calling session.kill().

      // However, the design explicitly says to handle this case. It could
      // happen if the tombstone was evicted (LRU) but the session is still
      // in exiting state. Let's test with a small tombstone limit.

      // Simpler: let's create a host with maxTombstones=0 so tombstones
      // are immediately evicted, then kill a session and try to reattach.
      const { spawner: spawner2, spawned: spawned2 } = createMockSpawner();
      const host2 = new TerminalHost({
        spawnSubprocess: spawner2,
        maxTombstones: 1,
      });
      const clientA = createMockClient("cA");
      const clientB = createMockClient("cB");
      const clientC = createMockClient("cC");

      host2.createOrAttach({
        sessionId: "sess-A",
        cols: 80,
        rows: 24,
        streamClient: clientA,
      });

      // Kill sess-A (adds tombstone, session enters "exiting")
      host2.kill("sess-A");

      // Add another tombstone to evict sess-A's tombstone (maxTombstones=1)
      // We need another session to kill
      host2.createOrAttach({
        sessionId: "sess-B",
        cols: 80,
        rows: 24,
        streamClient: clientB,
      });
      host2.kill("sess-B");

      // Now sess-A's tombstone is evicted, but session is still in "exiting" state
      // createOrAttach should treat it as non-existent
      const result = host2.createOrAttach({
        sessionId: "sess-A",
        cols: 80,
        rows: 24,
        streamClient: clientC,
      });

      expect(result.isNew).toBe(true);
      // A new subprocess was spawned (3rd one: sess-A, sess-B, new sess-A)
      expect(spawned2).toHaveLength(3);
    });

    it("returns snapshot on reattach to existing session", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client1,
      });

      // Emit some data so the emulator has content
      spawned[0]!.emitData("hello world\r\n");

      // Reattach
      const result = host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client2,
      });

      expect(result.isNew).toBe(false);
      expect(result.snapshot).not.toBeNull();
    });

    it("clamps dimensions to minimum 1×1 on createOrAttach", () => {
      const { spawner, spawnCalls } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client = createMockClient();

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 0,
        rows: -5,
        streamClient: client,
      });

      expect(spawnCalls[0]!.cols).toBe(1);
      expect(spawnCalls[0]!.rows).toBe(1);
    });

    it("clamps NaN dimensions to 1", () => {
      const { spawner, spawnCalls } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client = createMockClient();

      host.createOrAttach({
        sessionId: "sess-1",
        cols: NaN,
        rows: NaN,
        streamClient: client,
      });

      expect(spawnCalls[0]!.cols).toBe(1);
      expect(spawnCalls[0]!.rows).toBe(1);
    });
  });

  // -------------------------------------------------------------------------
  // Tombstone rejection
  // -------------------------------------------------------------------------

  describe("tombstone rejection", () => {
    it("rejects createOrAttach for tombstoned session", () => {
      const { spawner } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });
      const client1 = createMockClient("c1");
      const client2 = createMockClient("c2");

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: client1,
      });

      // Kill adds tombstone
      host.kill("sess-1");

      // Attempt to recreate — should throw
      expect(() =>
        host.createOrAttach({
          sessionId: "sess-1",
          cols: 80,
          rows: 24,
          streamClient: client2,
        }),
      ).toThrow(SessionTombstonedError);
    });

    it("evicts oldest tombstone when LRU limit is reached", () => {
      const { spawner } = createMockSpawner();
      const host = new TerminalHost({
        spawnSubprocess: spawner,
        maxTombstones: 3,
      });

      // Create and kill 3 sessions to fill tombstone capacity
      for (let i = 0; i < 3; i++) {
        const client = createMockClient(`c${i}`);
        host.createOrAttach({
          sessionId: `sess-${i}`,
          cols: 80,
          rows: 24,
          streamClient: client,
        });
        host.kill(`sess-${i}`);
      }

      expect(host.tombstoneCount).toBe(3);

      // Create and kill a 4th — should evict sess-0's tombstone
      const client4 = createMockClient("c4");
      host.createOrAttach({
        sessionId: "sess-4",
        cols: 80,
        rows: 24,
        streamClient: client4,
      });
      host.kill("sess-4");

      expect(host.tombstoneCount).toBe(3);

      // sess-0's tombstone was evicted — can now create with that ID
      const client0 = createMockClient("c0-new");
      const result = host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: client0,
      });
      expect(result.isNew).toBe(true);

      // sess-1's tombstone still exists — should reject
      expect(() =>
        host.createOrAttach({
          sessionId: "sess-1",
          cols: 80,
          rows: 24,
          streamClient: createMockClient(),
        }),
      ).toThrow(SessionTombstonedError);
    });
  });

  // -------------------------------------------------------------------------
  // Capacity limit
  // -------------------------------------------------------------------------

  describe("capacity limit", () => {
    it("rejects createOrAttach when at max capacity", () => {
      const { spawner } = createMockSpawner();
      const host = new TerminalHost({
        spawnSubprocess: spawner,
        maxSessions: 3,
      });

      // Fill to capacity
      for (let i = 0; i < 3; i++) {
        host.createOrAttach({
          sessionId: `sess-${i}`,
          cols: 80,
          rows: 24,
          streamClient: createMockClient(`c${i}`),
        });
      }

      expect(host.sessionCount).toBe(3);

      // Next one should fail
      expect(() =>
        host.createOrAttach({
          sessionId: "sess-overflow",
          cols: 80,
          rows: 24,
          streamClient: createMockClient("overflow"),
        }),
      ).toThrow(SessionCapacityError);
    });

    it("allows new sessions after existing ones exit", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({
        spawnSubprocess: spawner,
        maxSessions: 2,
      });

      host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c0"),
      });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c1"),
      });

      // At capacity — can't create more
      expect(() =>
        host.createOrAttach({
          sessionId: "sess-2",
          cols: 80,
          rows: 24,
          streamClient: createMockClient("c2"),
        }),
      ).toThrow(SessionCapacityError);

      // Exit one session
      spawned[0]!.emitExit(0);
      expect(host.sessionCount).toBe(1);

      // Natural exits are pruned from the host map immediately so long-lived
      // daemons do not fill capacity with dead sessions.
      const result = host.createOrAttach({
        sessionId: "sess-2",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c2"),
      });

      expect(result.isNew).toBe(true);
      expect(host.sessionCount).toBe(2);
    });

    it("reattach to existing session does not count against capacity", () => {
      const { spawner } = createMockSpawner();
      const host = new TerminalHost({
        spawnSubprocess: spawner,
        maxSessions: 2,
      });

      host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c0"),
      });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c1"),
      });

      // Reattach to existing — should succeed even at capacity
      const result = host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c0-reattach"),
      });

      expect(result.isNew).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // dispose releases fds
  // -------------------------------------------------------------------------

  describe("dispose releases fds", () => {
    it("forceKillAndDisposeSubprocess on all sessions", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c0"),
      });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c1"),
      });

      host.createOrAttach({
        sessionId: "sess-2",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c2"),
      });

      expect(host.sessionCount).toBe(3);

      host.dispose();

      // All subprocesses should be force-killed and disposed
      for (const handle of spawned) {
        expect(handle.forceKillCalled).toBe(true);
        expect(handle.disposed).toBe(true);
      }

      expect(host.sessionCount).toBe(0);
    });

    it("dispose is idempotent", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c0"),
      });

      host.dispose();
      // Second call should not throw
      host.dispose();

      expect(spawned[0]!.disposed).toBe(true);
    });

    it("clears tombstones on dispose", () => {
      const { spawner } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-0",
        cols: 80,
        rows: 24,
        streamClient: createMockClient("c0"),
      });
      host.kill("sess-0");

      expect(host.tombstoneCount).toBe(1);

      host.dispose();
      expect(host.tombstoneCount).toBe(0);
    });
  });

  // -------------------------------------------------------------------------
  // Session operations routing
  // -------------------------------------------------------------------------

  describe("session operations", () => {
    it("write forwards to session", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient(),
        shellReadySupported: false,
      });

      host.write("sess-1", "hello\n");
      expect(spawned[0]!.writtenData).toEqual(["hello\n"]);
    });

    it("resize forwards to session with clamping", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient(),
      });

      host.resize("sess-1", 0, -1);
      expect(spawned[0]!.resizes).toEqual([{ cols: 1, rows: 1 }]);
    });

    it("signal forwards to session", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient(),
      });

      host.signal("sess-1", "SIGINT");
      expect(spawned[0]!.signals).toContain("SIGINT");
    });

    it("throws SessionNotFoundError for unknown session", () => {
      const { spawner } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      expect(() => host.write("nonexistent", "data")).toThrow(SessionNotFoundError);
      expect(() => host.resize("nonexistent", 80, 24)).toThrow(SessionNotFoundError);
      expect(() => host.kill("nonexistent")).toThrow(SessionNotFoundError);
      expect(() => host.signal("nonexistent", "SIGINT")).toThrow(SessionNotFoundError);
      expect(() => host.getCwd("nonexistent")).toThrow(SessionNotFoundError);
      expect(() => host.getSnapshot("nonexistent")).toThrow(SessionNotFoundError);
      expect(() => host.clearScrollback("nonexistent")).toThrow(SessionNotFoundError);
    });

    it("listSessions returns info for all sessions", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        cwd: "/workspace/main",
        streamClient: createMockClient(),
      });

      host.createOrAttach({
        sessionId: "sess-2",
        cols: 120,
        rows: 40,
        streamClient: createMockClient(),
        agentMeta: {
          taskId: "task-1",
          agentType: "claude",
          worktreePath: "/path",
        },
      });

      spawned[0]!.emitData("\x1b]7;file:///workspace/main/src\x07");

      const sessions = host.listSessions();
      expect(sessions).toHaveLength(2);

      const s1 = sessions.find((s) => s.sessionId === "sess-1");
      expect(s1).toBeDefined();
      expect(s1!.state).toBe("running");
      expect(s1!.isAlive).toBe(true);
      expect(s1!.cols).toBe(80);
      expect(s1!.rows).toBe(24);
      expect(s1!.initialCwd).toBe("/workspace/main");
      expect(s1!.cwd).toBe("/workspace/main/src");
      expect(s1!.agentMeta).toBeUndefined();

      const s2 = sessions.find((s) => s.sessionId === "sess-2");
      expect(s2).toBeDefined();
      expect(s2!.cols).toBe(120);
      expect(s2!.rows).toBe(40);
      expect(s2!.agentMeta).toBeDefined();
      expect(s2!.agentMeta!.taskId).toBe("task-1");

      const liveSessions = host.getLiveSessionInfo();
      const liveS1 = liveSessions.find((s) => s.id === "sess-1");
      expect(liveS1!.initialCwd).toBe("/workspace/main");
      expect(liveS1!.cwd).toBe("/workspace/main/src");
    });

    it("getCwd returns null initially, then cwd after OSC-7", () => {
      const { spawner, spawned } = createMockSpawner();
      const host = new TerminalHost({ spawnSubprocess: spawner });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient(),
      });

      expect(host.getCwd("sess-1")).toBeNull();

      spawned[0]!.emitData("\x1b]7;file:///home/user/project\x07");
      expect(host.getCwd("sess-1")).toBe("/home/user/project");
    });

    it("onSessionExit callback is invoked when session exits", () => {
      const { spawner, spawned } = createMockSpawner();
      const onExit = vi.fn();
      const host = new TerminalHost({
        spawnSubprocess: spawner,
        onSessionExit: onExit,
      });

      host.createOrAttach({
        sessionId: "sess-1",
        cols: 80,
        rows: 24,
        streamClient: createMockClient(),
      });

      spawned[0]!.emitExit(42);
      expect(onExit).toHaveBeenCalledWith("sess-1", 42);
    });
  });
});
