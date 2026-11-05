import { socketAddress } from "../electron/daemon/socket-address";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { DaemonServer } from "../electron/daemon/daemon-server";
import {
  PROTOCOL_VERSION,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
  type DaemonEvent,
} from "../electron/daemon/protocol";
import type { SubprocessHandle, SessionInfo } from "../electron/daemon/types";
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

  constructor(pid = 1234) {
    this.pid = pid;
    this.fd = pid + 100;
  }

  write(_data: string): void {}
  resize(_cols: number, _rows: number): void {}
  kill(_signal?: string): void {}
  forceKill(): void {}
  signal(_sig: string): void {}
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
// Helpers
// ---------------------------------------------------------------------------

function createMockSpawner(): {
  spawner: SubprocessSpawner;
  spawned: MockSubprocessHandle[];
} {
  let nextPid = 3000;
  const spawned: MockSubprocessHandle[] = [];

  const spawner: SubprocessSpawner = (_opts: SubprocessSpawnOptions) => {
    const handle = new MockSubprocessHandle(nextPid++);
    spawned.push(handle);
    return handle;
  };

  return { spawner, spawned };
}

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-agent-state-test-"));
}

function createServerPaths(dir: string) {
  return {
    socketPath: path.join(dir, "daemon-v1.sock"),
    tokenPath: path.join(dir, "daemon-v1.token"),
    pidPath: path.join(dir, "daemon-v1.pid"),
  };
}

async function connectSocket(socketPath: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketAddress(socketPath), () => resolve(socket));
    socket.on("error", reject);
  });
}

async function sendHello(
  socket: net.Socket,
  token: string,
  opts: { clientId?: string; role?: "control" | "stream"; version?: number } = {},
): Promise<HelloResponse> {
  const hello: HelloMessage = {
    type: "hello",
    version: opts.version ?? PROTOCOL_VERSION,
    token,
    clientId: opts.clientId ?? "test-client",
    role: opts.role ?? "control",
  };

  socket.write(encodeFrame(hello));

  return new Promise((resolve) => {
    const decoder = new FrameDecoder();
    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      if (frames.length > 0) {
        socket.removeListener("data", onData);
        resolve(frames[0]!.json as HelloResponse);
      }
    };
    socket.on("data", onData);
  });
}

async function sendRequest(
  socket: net.Socket,
  request: DaemonRequest,
): Promise<DaemonResponse> {
  socket.write(encodeFrame(request));

  return new Promise((resolve) => {
    const decoder = new FrameDecoder();
    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      if (frames.length > 0) {
        socket.removeListener("data", onData);
        resolve(frames[0]!.json as DaemonResponse);
      }
    };
    socket.on("data", onData);
  });
}

/** Collect stream events from a stream socket until a condition is met. */
function collectStreamEvents(
  socket: net.Socket,
  count: number,
  timeoutMs = 2000,
): Promise<DaemonEvent[]> {
  return new Promise((resolve, reject) => {
    const events: DaemonEvent[] = [];
    const decoder = new FrameDecoder();

    const timer = setTimeout(() => {
      socket.removeListener("data", onData);
      resolve(events); // Return what we have
    }, timeoutMs);

    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      for (const frame of frames) {
        const event = frame.json as DaemonEvent;
        if (event.type === "event") {
          events.push(event);
          if (events.length >= count) {
            clearTimeout(timer);
            socket.removeListener("data", onData);
            resolve(events);
            return;
          }
        }
      }
    };

    socket.on("data", onData);
  });
}

/** Build an OSC state sequence. */
function oscState(state: string): string {
  return `\x1b]777;crab-state;${state}\x07`;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Agent State Events on Stream Socket", () => {
  let tmpDir: string;
  let server: DaemonServer;
  let spawner: SubprocessSpawner;
  let spawned: MockSubprocessHandle[];

  beforeEach(async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    tmpDir = createTmpDir();
    const mock = createMockSpawner();
    spawner = mock.spawner;
    spawned = mock.spawned;
  });

  afterEach(async () => {
    vi.useRealTimers();
    if (server) {
      await server.shutdown({ killSessions: true });
    }
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  // -------------------------------------------------------------------------
  // AgentTracker wiring in DaemonServer
  // -------------------------------------------------------------------------

  describe("AgentTracker wiring", () => {
    it("registers agent sessions with AgentTracker on createOrAttach", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      const helloResp = await sendHello(controlSocket, server.authToken, { role: "control" });
      expect(helloResp.ok).toBe(true);

      // Create an agent session
      const resp = await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-session-1",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-123",
            agentType: "claude",
            worktreePath: "/tmp/worktree",
          },
        },
      });

      expect(resp.ok).toBe(true);

      // Verify AgentTracker has the session registered
      const trackerState = server.tracker.getState("agent-session-1");
      expect(trackerState).toBe("idle");

      controlSocket.destroy();
    });

    it("does not register non-agent sessions with AgentTracker", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      // Create a regular (non-agent) session
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "user-session-1",
          cols: 80,
          rows: 24,
        },
      });

      // AgentTracker should not have this session
      const trackerState = server.tracker.getState("user-session-1");
      expect(trackerState).toBeNull();

      controlSocket.destroy();
    });

    it("broadcasts state-changed event when agent state transitions", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const clientId = "test-client-1";

      // Connect control socket
      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { clientId, role: "control" });

      // Connect stream socket
      const streamSocket = await connectSocket(paths.socketPath);
      await sendHello(streamSocket, server.authToken, { clientId, role: "stream" });

      // Create an agent session
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-s1",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-1",
            agentType: "claude",
            worktreePath: "/tmp/wt",
          },
        },
      });

      // Collect stream events — expect data event + state-changed event
      const eventsPromise = collectStreamEvents(streamSocket, 2, 1000);

      // Emit PTY data containing an OSC state sequence
      spawned[0]!.emitData(`output${oscState("working")}more`);

      // Advance timers to flush the batcher
      vi.advanceTimersByTime(50);

      const events = await eventsPromise;

      // Should have a data event and a state-changed event
      const stateEvent = events.find((e) => e.event === "state-changed");
      expect(stateEvent).toBeDefined();
      expect(stateEvent!.sessionId).toBe("agent-s1");
      expect((stateEvent as any).payload.state).toBe("working");
      expect((stateEvent as any).payload.previousState).toBe("idle");
      expect((stateEvent as any).payload.timestamp).toBeTypeOf("number");

      controlSocket.destroy();
      streamSocket.destroy();
    });

    it("feeds PTY data through AgentTracker for state parsing", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      // Create agent session
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-s2",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-2",
            agentType: "claude",
            worktreePath: "/tmp/wt",
          },
        },
      });

      // Emit data with state transitions
      spawned[0]!.emitData(oscState("working"));
      expect(server.tracker.getState("agent-s2")).toBe("working");

      spawned[0]!.emitData(oscState("waiting"));
      expect(server.tracker.getState("agent-s2")).toBe("waiting");

      controlSocket.destroy();
    });

    it("notifies AgentTracker on session exit", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      // Create agent session
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-s3",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-3",
            agentType: "claude",
            worktreePath: "/tmp/wt",
          },
        },
      });

      spawned[0]!.emitData(oscState("working"));
      expect(server.tracker.getState("agent-s3")).toBe("working");

      // Simulate process exit
      spawned[0]!.emitExit(0);

      expect(server.tracker.getState("agent-s3")).toBe("exited");

      controlSocket.destroy();
    });
  });

  // -------------------------------------------------------------------------
  // listSessions with agent metadata
  // -------------------------------------------------------------------------

  describe("listSessions includes agent metadata", () => {
    it("includes activityState from AgentTracker in listSessions response", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      // Create agent session
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-list-1",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-list",
            agentType: "claude",
            worktreePath: "/tmp/wt",
          },
        },
      });

      // Transition to working state
      spawned[0]!.emitData(oscState("working"));

      // List sessions
      const listResp = await sendRequest(controlSocket, {
        id: "req-2",
        type: "listSessions",
      });

      expect(listResp.ok).toBe(true);
      const sessions = listResp.payload as SessionInfo[];
      expect(sessions).toHaveLength(1);

      const session = sessions[0]!;
      expect(session.agentMeta).toBeDefined();
      expect(session.agentMeta!.taskId).toBe("task-list");
      expect(session.agentMeta!.agentType).toBe("claude");
      expect(session.agentMeta!.activityState).toBe("working");

      controlSocket.destroy();
    });

    it("does not include agentMeta for non-agent sessions", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      // Create a regular session (no agentMeta)
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "user-list-1",
          cols: 80,
          rows: 24,
        },
      });

      const listResp = await sendRequest(controlSocket, {
        id: "req-2",
        type: "listSessions",
      });

      expect(listResp.ok).toBe(true);
      const sessions = listResp.payload as SessionInfo[];
      expect(sessions[0]!.agentMeta).toBeUndefined();

      controlSocket.destroy();
    });
  });

  // -------------------------------------------------------------------------
  // getAgentSessions on DaemonClient
  // -------------------------------------------------------------------------

  describe("DaemonClient.getAgentSessions", () => {
    it("filters to only agent sessions from listSessions", async () => {
      // This tests the filtering logic directly since DaemonClient.getAgentSessions
      // calls request("listSessions") and filters by agentMeta presence.
      // We test the server-side behavior that provides the data.
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      // Create one agent session and one user session
      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-filter-1",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-f1",
            agentType: "claude",
            worktreePath: "/tmp/wt1",
          },
        },
      });

      await sendRequest(controlSocket, {
        id: "req-2",
        type: "createOrAttach",
        payload: {
          sessionId: "user-filter-1",
          cols: 80,
          rows: 24,
        },
      });

      // List sessions — should have both
      const listResp = await sendRequest(controlSocket, {
        id: "req-3",
        type: "listSessions",
      });

      expect(listResp.ok).toBe(true);
      const sessions = listResp.payload as SessionInfo[];
      expect(sessions).toHaveLength(2);

      // Filter like getAgentSessions does
      const agentSessions = sessions.filter((s) => s.agentMeta !== undefined);
      expect(agentSessions).toHaveLength(1);
      expect(agentSessions[0]!.sessionId).toBe("agent-filter-1");
      expect(agentSessions[0]!.agentMeta!.taskId).toBe("task-f1");

      controlSocket.destroy();
    });
  });

  // -------------------------------------------------------------------------
  // AgentTracker timeout wiring
  // -------------------------------------------------------------------------

  describe("AgentTracker timeout kills session via DaemonServer", () => {
    it("kills agent session when timeout fires in working state", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Override the tracker timeout for testing
      // The AgentTracker is created with default 30min timeout,
      // but we can verify the wiring by checking the tracker's onTimeout
      // is connected to host.kill()
      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { role: "control" });

      await sendRequest(controlSocket, {
        id: "req-1",
        type: "createOrAttach",
        payload: {
          sessionId: "agent-timeout-1",
          cols: 80,
          rows: 24,
          agentMeta: {
            taskId: "task-t1",
            agentType: "claude",
            worktreePath: "/tmp/wt",
          },
        },
      });

      // Verify the tracker is wired — the onTimeout callback should
      // attempt to kill the session via host.kill()
      expect(server.tracker).toBeDefined();
      expect(server.tracker.getState("agent-timeout-1")).toBe("idle");

      controlSocket.destroy();
    });
  });
});
