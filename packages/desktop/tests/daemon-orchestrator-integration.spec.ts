import { socketAddress } from "../electron/daemon/socket-address";
/**
 * Orchestrator Integration Test
 *
 * Tests the orchestrator-daemon integration using DaemonServer with mock
 * subprocesses. Verifies:
 * 1. Agent session creation with agentMeta → registered in AgentTracker
 * 2. PTY output streams to attached clients (simulating adapter receiving output)
 * 3. Kill → SIGTERM → exit → exit event emitted
 * 4. App restart simulation: listSessions → match by taskId → reattach via createOrAttach
 * 5. Agent exit while app is closed: exit from subprocess → detected on next listSessions
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
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
import type { SubprocessHandle } from "../electron/daemon/types";
import type {
  SubprocessSpawner,
  SubprocessSpawnOptions,
} from "../electron/daemon/subprocess-handle";

// ---------------------------------------------------------------------------
// Mock SubprocessHandle
// ---------------------------------------------------------------------------

class MockSubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];
  private _disposed = false;
  private _paused = false;

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
    this._paused = true;
  }

  resume(): void {
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
// Mock Spawner
// ---------------------------------------------------------------------------

function createMockSpawner(): {
  spawner: SubprocessSpawner;
  spawned: MockSubprocessHandle[];
} {
  let nextPid = 7000;
  const spawned: MockSubprocessHandle[] = [];

  const spawner: SubprocessSpawner = (_opts: SubprocessSpawnOptions) => {
    const handle = new MockSubprocessHandle(nextPid++);
    spawned.push(handle);
    return handle;
  };

  return { spawner, spawned };
}

// ---------------------------------------------------------------------------
// Test Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-orch-test-"));
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
  payload?: Buffer,
): Promise<DaemonResponse> {
  socket.write(encodeFrame(request, payload));

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

function collectStreamEvents(
  socket: net.Socket,
  count: number,
  timeoutMs = 2000,
): Promise<Array<{ json: DaemonEvent; payload: Buffer | null }>> {
  return new Promise((resolve) => {
    const events: Array<{ json: DaemonEvent; payload: Buffer | null }> = [];
    const decoder = new FrameDecoder();

    const timer = setTimeout(() => {
      socket.removeListener("data", onData);
      resolve(events);
    }, timeoutMs);

    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      for (const frame of frames) {
        const event = frame.json as DaemonEvent;
        if (event.type === "event") {
          events.push({ json: event, payload: frame.payload });
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

async function connectClient(
  socketPath: string,
  token: string,
  clientId: string,
): Promise<{ control: net.Socket; stream: net.Socket }> {
  const control = await connectSocket(socketPath);
  const controlResp = await sendHello(control, token, { clientId, role: "control" });
  expect(controlResp.ok).toBe(true);

  const stream = await connectSocket(socketPath);
  const streamResp = await sendHello(stream, token, { clientId, role: "stream" });
  expect(streamResp.ok).toBe(true);

  return { control, stream };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Orchestrator Integration Test", () => {
  let tmpDir: string;
  let server: DaemonServer;
  let spawner: SubprocessSpawner;
  let spawned: MockSubprocessHandle[];

  beforeEach(() => {
    tmpDir = createTmpDir();
    const mock = createMockSpawner();
    spawner = mock.spawner;
    spawned = mock.spawned;
  });

  afterEach(async () => {
    if (server) {
      await server.shutdown({ killSessions: true });
    }
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  // =========================================================================
  // 1. Create agent session with agentMeta → verify registered in AgentTracker
  // =========================================================================

  it("creates an agent session with agentMeta and registers it in AgentTracker", async () => {
    const paths = createServerPaths(tmpDir);
    server = new DaemonServer({
      ...paths,
      spawnSubprocess: spawner,
      idleTimeoutMs: 0,
    });
    await server.start();

    const { control, stream } = await connectClient(
      paths.socketPath,
      server.authToken,
      "orchestrator-client",
    );

    const agentMeta = {
      taskId: "task-abc-123",
      agentType: "coding-agent",
      worktreePath: "/workspace/my-project",
    };

    const response = await sendRequest(control, {
      id: "req-1",
      type: "createOrAttach",
      payload: {
        sessionId: "agent-sess-1",
        cols: 120,
        rows: 30,
        cwd: "/workspace/my-project",
        agentMeta,
      },
    });

    expect(response.ok).toBe(true);
    const result = response.payload as { isNew: boolean; pid: number };
    expect(result.isNew).toBe(true);
    expect(result.pid).toBe(7000);

    // Verify the session is registered in AgentTracker
    const agentSessions = server.tracker.listAgentSessions();
    expect(agentSessions.length).toBe(1);
    expect(agentSessions[0]!.sessionId).toBe("agent-sess-1");
    expect(agentSessions[0]!.taskId).toBe("task-abc-123");
    expect(agentSessions[0]!.agentType).toBe("coding-agent");
    expect(agentSessions[0]!.worktreePath).toBe("/workspace/my-project");
    expect(agentSessions[0]!.state).toBe("idle");

    control.destroy();
    stream.destroy();
  });

  // =========================================================================
  // 2. Emit PTY output → verify it streams to attached clients
  // =========================================================================

  it("streams PTY output to attached clients (simulating adapter receiving output)", async () => {
    const paths = createServerPaths(tmpDir);
    server = new DaemonServer({
      ...paths,
      spawnSubprocess: spawner,
      idleTimeoutMs: 0,
    });
    await server.start();

    const { control, stream } = await connectClient(
      paths.socketPath,
      server.authToken,
      "adapter-client",
    );

    await sendRequest(control, {
      id: "req-1",
      type: "createOrAttach",
      payload: {
        sessionId: "agent-output-sess",
        cols: 120,
        rows: 30,
        agentMeta: {
          taskId: "task-output-1",
          agentType: "coding-agent",
          worktreePath: "/workspace/project",
        },
      },
    });

    // Collect data events from the stream socket
    const eventsPromise = collectStreamEvents(stream, 1, 1000);

    // Simulate agent producing output
    spawned[0]!.emitData("Running tests...\r\nAll 42 tests passed.\r\n");

    // Wait for the batcher to flush (8ms timer + margin)
    await new Promise((r) => setTimeout(r, 50));

    const events = await eventsPromise;
    expect(events.length).toBeGreaterThan(0);

    const dataEvent = events.find((e) => e.json.event === "data");
    expect(dataEvent).toBeDefined();
    expect(dataEvent!.json.sessionId).toBe("agent-output-sess");
    expect(dataEvent!.payload!.toString()).toContain("Running tests...");
    expect(dataEvent!.payload!.toString()).toContain("All 42 tests passed.");

    control.destroy();
    stream.destroy();
  });

  // =========================================================================
  // 3. Kill session → SIGTERM → exit → exit event emitted
  // =========================================================================

  it("kill sends SIGTERM, simulates exit, and emits exit event to orchestrator", async () => {
    const paths = createServerPaths(tmpDir);
    server = new DaemonServer({
      ...paths,
      spawnSubprocess: spawner,
      idleTimeoutMs: 0,
    });
    await server.start();

    const { control, stream } = await connectClient(
      paths.socketPath,
      server.authToken,
      "cancel-client",
    );

    await sendRequest(control, {
      id: "req-1",
      type: "createOrAttach",
      payload: {
        sessionId: "agent-cancel-sess",
        cols: 120,
        rows: 30,
        agentMeta: {
          taskId: "task-cancel-1",
          agentType: "coding-agent",
          worktreePath: "/workspace/project",
        },
      },
    });

    // Verify the session is alive before kill
    expect(server.sessionCount).toBe(1);

    // Set up a raw data listener on the stream socket to capture ALL data
    const receivedBuffers: Buffer[] = [];
    stream.on("data", (chunk: Buffer) => {
      receivedBuffers.push(chunk);
    });

    // Orchestrator cancels the task → sends kill
    const killResp = await sendRequest(control, {
      id: "req-2",
      type: "kill",
      payload: { sessionId: "agent-cancel-sess" },
    });
    expect(killResp.ok).toBe(true);

    // Verify SIGTERM was sent to the subprocess
    expect(spawned[0]!.killCalled).toBe(true);
    expect(spawned[0]!.signals).toContain("SIGTERM");

    // Simulate the process exiting after receiving SIGTERM
    spawned[0]!.emitExit(143); // 128 + 15 (SIGTERM)

    // Wait for socket writes to propagate
    await new Promise((r) => setTimeout(r, 100));

    // Verify AgentTracker recorded the exit
    const agentState = server.tracker.getState("agent-cancel-sess");
    expect(agentState).toBe("exited");

    // Parse all received data on the stream socket
    const decoder = new FrameDecoder();
    for (const buf of receivedBuffers) {
      decoder.push(buf);
    }
    const frames = decoder.frames();
    const exitEvent = frames.find(
      (f) => (f.json as any).type === "event" && (f.json as any).event === "exit",
    );
    expect(exitEvent).toBeDefined();
    expect((exitEvent!.json as any).sessionId).toBe("agent-cancel-sess");
    expect((exitEvent!.json as any).payload.code).toBe(143);

    control.destroy();
    stream.destroy();
  });

  // =========================================================================
  // 4. Simulate app restart: listSessions → match by taskId → reattach
  // =========================================================================

  it("simulates app restart: lists sessions, matches by taskId, reattaches via createOrAttach", async () => {
    const paths = createServerPaths(tmpDir);
    server = new DaemonServer({
      ...paths,
      spawnSubprocess: spawner,
      idleTimeoutMs: 0,
    });
    await server.start();

    // Phase 1: Original client creates an agent session
    const { control: ctrl1, stream: stream1 } = await connectClient(
      paths.socketPath,
      server.authToken,
      "original-client",
    );

    await sendRequest(ctrl1, {
      id: "req-1",
      type: "createOrAttach",
      payload: {
        sessionId: "agent-restart-sess",
        cols: 120,
        rows: 30,
        cwd: "/workspace/project",
        agentMeta: {
          taskId: "task-restart-1",
          agentType: "coding-agent",
          worktreePath: "/workspace/project",
        },
      },
    });

    // Agent produces some output while running
    spawned[0]!.emitData("Working on task...\r\n");
    await new Promise((r) => setTimeout(r, 20));

    // Phase 2: Simulate app quit (disconnect original client)
    ctrl1.destroy();
    stream1.destroy();

    // Wait for disconnect to propagate
    await new Promise((r) => setTimeout(r, 100));

    // Phase 3: Simulate app restart — new client connects
    const { control: ctrl2, stream: stream2 } = await connectClient(
      paths.socketPath,
      server.authToken,
      "restarted-client",
    );

    // Step 1: List sessions to find running agent sessions
    const listResp = await sendRequest(ctrl2, {
      id: "req-list",
      type: "listSessions",
    });
    expect(listResp.ok).toBe(true);

    const sessions = listResp.payload as Array<{
      sessionId: string;
      state: string;
      isAlive: boolean;
      agentMeta?: { taskId: string; activityState: string };
    }>;

    // Find the session matching our task
    const matchingSession = sessions.find(
      (s) => s.agentMeta?.taskId === "task-restart-1",
    );
    expect(matchingSession).toBeDefined();
    expect(matchingSession!.isAlive).toBe(true);
    expect(matchingSession!.sessionId).toBe("agent-restart-sess");

    // Step 2: Reattach via createOrAttach
    const reattachResp = await sendRequest(ctrl2, {
      id: "req-reattach",
      type: "createOrAttach",
      payload: {
        sessionId: "agent-restart-sess",
        cols: 120,
        rows: 30,
      },
    });
    expect(reattachResp.ok).toBe(true);

    const reattachResult = reattachResp.payload as {
      isNew: boolean;
      snapshot: { snapshotAnsi: string } | null;
      pid: number;
    };
    expect(reattachResult.isNew).toBe(false);
    expect(reattachResult.pid).toBe(7000);
    // Snapshot should contain the output produced while disconnected
    expect(reattachResult.snapshot).not.toBeNull();

    // Step 3: Verify new client receives subsequent output
    const eventsPromise = collectStreamEvents(stream2, 1, 1000);
    spawned[0]!.emitData("Still working...\r\n");
    await new Promise((r) => setTimeout(r, 50));

    const events = await eventsPromise;
    expect(events.length).toBeGreaterThan(0);
    const dataEvent = events.find((e) => e.json.event === "data");
    expect(dataEvent).toBeDefined();
    expect(dataEvent!.payload!.toString()).toContain("Still working...");

    ctrl2.destroy();
    stream2.destroy();
  });

  // =========================================================================
  // 5. Agent exit while app is closed → detect on next listSessions
  // =========================================================================

  it("detects agent exit that occurred while app was disconnected", async () => {
    const paths = createServerPaths(tmpDir);
    server = new DaemonServer({
      ...paths,
      spawnSubprocess: spawner,
      idleTimeoutMs: 0,
    });
    await server.start();

    // Phase 1: Client creates an agent session
    const { control: ctrl1, stream: stream1 } = await connectClient(
      paths.socketPath,
      server.authToken,
      "first-client",
    );

    await sendRequest(ctrl1, {
      id: "req-1",
      type: "createOrAttach",
      payload: {
        sessionId: "agent-exit-offline-sess",
        cols: 120,
        rows: 30,
        agentMeta: {
          taskId: "task-exit-offline",
          agentType: "coding-agent",
          worktreePath: "/workspace/project",
        },
      },
    });

    // Phase 2: Simulate app quit (disconnect client)
    ctrl1.destroy();
    stream1.destroy();
    await new Promise((r) => setTimeout(r, 100));

    // Phase 3: Agent exits while app is closed
    spawned[0]!.emitExit(0);
    await new Promise((r) => setTimeout(r, 50));

    // Phase 4: App relaunches — new client connects and queries sessions
    const { control: ctrl2, stream: stream2 } = await connectClient(
      paths.socketPath,
      server.authToken,
      "second-client",
    );

    const listResp = await sendRequest(ctrl2, {
      id: "req-list",
      type: "listSessions",
    });
    expect(listResp.ok).toBe(true);

    const sessions = listResp.payload as Array<{
      sessionId: string;
      state: string;
      isAlive: boolean;
      agentMeta?: { taskId: string; activityState: string };
    }>;

    // The session that exited while disconnected should show as exited
    const exitedSession = sessions.find(
      (s) => s.agentMeta?.taskId === "task-exit-offline",
    );

    if (exitedSession) {
      // If the session is still in the list, it should show as not alive / exited
      expect(exitedSession.isAlive).toBe(false);
      expect(exitedSession.state).toBe("exited");
    } else {
      // Session may have been cleaned up after exit — verify via AgentTracker
      const agentState = server.tracker.getState("agent-exit-offline-sess");
      expect(agentState).toBe("exited");
    }

    // Verify AgentTracker recorded the exit regardless
    const agentInfo = server.tracker.listAgentSessions().find(
      (a) => a.taskId === "task-exit-offline",
    );
    expect(agentInfo).toBeDefined();
    expect(agentInfo!.state).toBe("exited");

    ctrl2.destroy();
    stream2.destroy();
  });
});
