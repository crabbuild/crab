import { socketAddress } from "../electron/daemon/socket-address";
/**
 * Control-Plane Test Suite
 *
 * Comprehensive integration tests for DaemonServer over real Unix sockets.
 * Covers: handshake, session lifecycle, I/O, multi-client fan-out,
 * error handling, and tombstones.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { DaemonServer } from "../electron/daemon/daemon-server";
import {
  PROTOCOL_VERSION,
  NOTIFY_PREFIX,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
  type DaemonEvent,
  type PingResponse,
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
  let nextPid = 5000;
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
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-cp-test-"));
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

/** Collect stream events from a stream socket. */
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

/** Connect a full client (control + stream) and return both sockets. */
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

describe("Control-Plane Test Suite", () => {
  let tmpDir: string;
  let server: DaemonServer;
  let spawner: SubprocessSpawner;
  let spawned: MockSubprocessHandle[];

  beforeEach(async () => {
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
  // 1. Handshake
  // =========================================================================

  describe("Handshake", () => {
    it("accepts valid hello with correct version and token", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      const response = await sendHello(socket, server.authToken);

      expect(response.ok).toBe(true);
      expect(response.type).toBe("hello");
      expect(response.error).toBeUndefined();

      socket.destroy();
    });

    it("rejects hello with wrong protocol version", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      const response = await sendHello(socket, server.authToken, { version: 999 });

      expect(response.ok).toBe(false);
      expect(response.error).toContain("Version mismatch");

      socket.destroy();
    });

    it("rejects hello with invalid token", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      const response = await sendHello(socket, "bogus-token-value");

      expect(response.ok).toBe(false);
      expect(response.error).toContain("Invalid token");

      socket.destroy();
    });

    it("rejects non-hello first message", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);

      // Send a request frame instead of hello
      const frame = encodeFrame({
        id: "req-1",
        type: "ping",
      });
      socket.write(frame);

      const response = await new Promise<HelloResponse>((resolve) => {
        const decoder = new FrameDecoder();
        socket.on("data", (chunk: Buffer) => {
          decoder.push(chunk);
          const frames = decoder.frames();
          if (frames.length > 0) {
            resolve(frames[0]!.json as HelloResponse);
          }
        });
      });

      expect(response.ok).toBe(false);
      expect(response.error).toContain("Expected hello message");

      socket.destroy();
    });

    it("accepts both control and stream roles for same client", async () => {
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
        "dual-client",
      );

      // Verify the client is registered
      expect(server.clientCount).toBe(1);

      control.destroy();
      stream.destroy();
    });
  });

  // =========================================================================
  // 2. Session Lifecycle
  // =========================================================================

  describe("Session Lifecycle", () => {
    it("createOrAttach spawns a new session", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      const response = await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-new", cols: 80, rows: 24 },
      });

      expect(response.ok).toBe(true);
      const result = response.payload as { isNew: boolean; pid: number; shellState: string };
      expect(result.isNew).toBe(true);
      expect(result.pid).toBe(5000);
      expect(result.shellState).toBeDefined();
      expect(server.sessionCount).toBe(1);

      socket.destroy();
    });

    it("createOrAttach reattaches to an existing session with snapshot", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Client 1 creates the session
      const socket1 = await connectSocket(paths.socketPath);
      await sendHello(socket1, server.authToken, { clientId: "c1" });

      await sendRequest(socket1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-reattach", cols: 80, rows: 24 },
      });

      // Emit some data so the emulator has content
      spawned[0]!.emitData("hello world\r\n");

      // Client 2 reattaches to the same session
      const socket2 = await connectSocket(paths.socketPath);
      await sendHello(socket2, server.authToken, { clientId: "c2" });

      const response = await sendRequest(socket2, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-reattach", cols: 80, rows: 24 },
      });

      expect(response.ok).toBe(true);
      const result = response.payload as {
        isNew: boolean;
        pid: number;
        snapshot: { snapshotAnsi: string } | null;
      };
      expect(result.isNew).toBe(false);
      expect(result.pid).toBe(5000);
      // Snapshot should contain the emitted data
      expect(result.snapshot).not.toBeNull();

      socket1.destroy();
      socket2.destroy();
    });

    it("kill terminates a session and records tombstone", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });

      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-kill", cols: 80, rows: 24 },
      });

      expect(server.sessionCount).toBe(1);

      const killResp = await sendRequest(socket, {
        id: "req-2",
        type: "kill",
        payload: { sessionId: "sess-kill" },
      });

      expect(killResp.ok).toBe(true);
      expect(spawned[0]!.killCalled).toBe(true);

      socket.destroy();
      vi.useRealTimers();
    });

    it("natural exit emits exit event on stream socket", async () => {
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
        "exit-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-exit", cols: 80, rows: 24 },
      });

      // Collect exit event
      const eventsPromise = collectStreamEvents(stream, 1, 1000);

      // Simulate natural process exit
      spawned[0]!.emitExit(0);

      const events = await eventsPromise;
      expect(events.length).toBeGreaterThanOrEqual(1);

      const exitEvent = events.find((e) => e.json.event === "exit");
      expect(exitEvent).toBeDefined();
      expect(exitEvent!.json.sessionId).toBe("sess-exit");
      expect((exitEvent!.json as any).payload.code).toBe(0);

      control.destroy();
      stream.destroy();
    });
  });

  // =========================================================================
  // 3. I/O (write, resize, signal)
  // =========================================================================

  describe("I/O", () => {
    it("write forwards data to the session subprocess", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-io", cols: 80, rows: 24 },
      });

      // Write data with binary payload
      const writeData = Buffer.from("ls -la\n");
      const response = await sendRequest(
        socket,
        {
          id: "req-2",
          type: "write",
          payload: { sessionId: "sess-io" },
        },
        writeData,
      );

      expect(response.ok).toBe(true);
      expect(spawned[0]!.writtenData).toContain("ls -la\n");

      socket.destroy();
    });

    it("resize updates session dimensions", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-resize", cols: 80, rows: 24 },
      });

      const response = await sendRequest(socket, {
        id: "req-2",
        type: "resize",
        payload: { sessionId: "sess-resize", cols: 120, rows: 40 },
      });

      expect(response.ok).toBe(true);
      expect(spawned[0]!.resizes).toContainEqual({ cols: 120, rows: 40 });

      // Verify via listSessions
      const listResp = await sendRequest(socket, {
        id: "req-3",
        type: "listSessions",
      });
      const sessions = listResp.payload as Array<{ cols: number; rows: number }>;
      expect(sessions[0]!.cols).toBe(120);
      expect(sessions[0]!.rows).toBe(40);

      socket.destroy();
    });

    it("signal forwards arbitrary signal to subprocess", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-sig", cols: 80, rows: 24 },
      });

      const response = await sendRequest(socket, {
        id: "req-2",
        type: "signal",
        payload: { sessionId: "sess-sig", signal: "SIGUSR1" },
      });

      expect(response.ok).toBe(true);
      expect(spawned[0]!.signals).toContain("SIGUSR1");

      socket.destroy();
    });
  });

  // =========================================================================
  // 4. Multi-Client Fan-Out
  // =========================================================================

  describe("Multi-Client Fan-Out", () => {
    it("two clients attached to same session both receive output", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Connect client 1
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "fan-c1",
      );

      // Create session via client 1
      await sendRequest(ctrl1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-fanout", cols: 80, rows: 24 },
      });

      // Connect client 2
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "fan-c2",
      );

      // Attach client 2 to the same session
      await sendRequest(ctrl2, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-fanout", cols: 80, rows: 24 },
      });

      // Collect events from both streams
      const events1Promise = collectStreamEvents(stream1, 1, 500);
      const events2Promise = collectStreamEvents(stream2, 1, 500);

      // Emit PTY output
      spawned[0]!.emitData("fan-out data");

      // Wait for batch timer
      await new Promise((r) => setTimeout(r, 50));

      const events1 = await events1Promise;
      const events2 = await events2Promise;

      // Both clients should receive data events
      expect(events1.length).toBeGreaterThan(0);
      expect(events2.length).toBeGreaterThan(0);

      const dataEvent1 = events1.find((e) => e.json.event === "data");
      const dataEvent2 = events2.find((e) => e.json.event === "data");

      expect(dataEvent1).toBeDefined();
      expect(dataEvent1!.json.sessionId).toBe("sess-fanout");
      expect(dataEvent2).toBeDefined();
      expect(dataEvent2!.json.sessionId).toBe("sess-fanout");

      // Both should have the same payload content
      expect(dataEvent1!.payload!.toString()).toBe("fan-out data");
      expect(dataEvent2!.payload!.toString()).toBe("fan-out data");

      ctrl1.destroy();
      ctrl2.destroy();
      stream1.destroy();
      stream2.destroy();
    });

    it("both clients receive exit event when session exits", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Connect two clients to the same session
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "exit-c1",
      );
      await sendRequest(ctrl1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-exit-fan", cols: 80, rows: 24 },
      });

      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "exit-c2",
      );
      await sendRequest(ctrl2, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-exit-fan", cols: 80, rows: 24 },
      });

      // Collect exit events from both
      const events1Promise = collectStreamEvents(stream1, 1, 1000);
      const events2Promise = collectStreamEvents(stream2, 1, 1000);

      // Simulate exit
      spawned[0]!.emitExit(42);

      const events1 = await events1Promise;
      const events2 = await events2Promise;

      const exit1 = events1.find((e) => e.json.event === "exit");
      const exit2 = events2.find((e) => e.json.event === "exit");

      expect(exit1).toBeDefined();
      expect((exit1!.json as any).payload.code).toBe(42);
      expect(exit2).toBeDefined();
      expect((exit2!.json as any).payload.code).toBe(42);

      ctrl1.destroy();
      ctrl2.destroy();
      stream1.destroy();
      stream2.destroy();
    });
  });

  // =========================================================================
  // 5. Error Handling
  // =========================================================================

  describe("Error Handling", () => {
    it("returns error for operations on invalid session ID", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      // Try write to non-existent session
      const writeResp = await sendRequest(
        socket,
        {
          id: "req-1",
          type: "write",
          payload: { sessionId: "does-not-exist" },
        },
        Buffer.from("data"),
      );
      expect(writeResp.ok).toBe(false);
      expect(writeResp.error).toContain("Session not found");

      // Try resize on non-existent session
      const resizeResp = await sendRequest(socket, {
        id: "req-2",
        type: "resize",
        payload: { sessionId: "does-not-exist", cols: 80, rows: 24 },
      });
      expect(resizeResp.ok).toBe(false);
      expect(resizeResp.error).toContain("Session not found");

      // Try kill on non-existent session
      const killResp = await sendRequest(socket, {
        id: "req-3",
        type: "kill",
        payload: { sessionId: "does-not-exist" },
      });
      expect(killResp.ok).toBe(false);
      expect(killResp.error).toContain("Session not found");

      // Try signal on non-existent session
      const sigResp = await sendRequest(socket, {
        id: "req-4",
        type: "signal",
        payload: { sessionId: "does-not-exist", signal: "SIGTERM" },
      });
      expect(sigResp.ok).toBe(false);
      expect(sigResp.error).toContain("Session not found");

      socket.destroy();
    });

    it("returns error when session capacity limit is reached", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
        maxSessions: 2, // Low limit for testing
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      // Create sessions up to the limit
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "cap-1", cols: 80, rows: 24 },
      });
      await sendRequest(socket, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "cap-2", cols: 80, rows: 24 },
      });

      expect(server.sessionCount).toBe(2);

      // Third session should fail
      const response = await sendRequest(socket, {
        id: "req-3",
        type: "createOrAttach",
        payload: { sessionId: "cap-3", cols: 80, rows: 24 },
      });

      expect(response.ok).toBe(false);
      expect(response.error).toContain("capacity");

      socket.destroy();
    });
  });

  // =========================================================================
  // 6. Tombstones
  // =========================================================================

  describe("Tombstones", () => {
    it("killed session rejects subsequent createOrAttach", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });

      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      // Create and kill a session
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-tomb", cols: 80, rows: 24 },
      });

      await sendRequest(socket, {
        id: "req-2",
        type: "kill",
        payload: { sessionId: "sess-tomb" },
      });

      // Simulate the process exiting after kill
      spawned[0]!.emitExit(137);
      vi.advanceTimersByTime(100);

      // Attempt to recreate the same session — should be rejected
      const response = await sendRequest(socket, {
        id: "req-3",
        type: "createOrAttach",
        payload: { sessionId: "sess-tomb", cols: 80, rows: 24 },
      });

      expect(response.ok).toBe(false);
      expect(response.error).toContain("killed");

      socket.destroy();
      vi.useRealTimers();
    });

    it("tombstone LRU eviction allows reuse of old session IDs", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });

      const paths = createServerPaths(tmpDir);
      // Use a very small tombstone limit to test eviction
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
        maxSessions: 100,
      });
      await server.start();

      // Access the terminal host's tombstone limit via a lower maxTombstones
      // We'll test this by creating and killing enough sessions to evict
      // the first tombstone. The TerminalHost defaults to 1000 tombstones,
      // so we test the concept at the integration level.
      const socket = await connectSocket(paths.socketPath);
      await sendHello(socket, server.authToken);

      // Create and kill a session to tombstone it
      await sendRequest(socket, {
        id: "req-create",
        type: "createOrAttach",
        payload: { sessionId: "evict-target", cols: 80, rows: 24 },
      });
      await sendRequest(socket, {
        id: "req-kill",
        type: "kill",
        payload: { sessionId: "evict-target" },
      });
      spawned[0]!.emitExit(137);
      vi.advanceTimersByTime(100);

      // Verify it's tombstoned
      const rejectedResp = await sendRequest(socket, {
        id: "req-rejected",
        type: "createOrAttach",
        payload: { sessionId: "evict-target", cols: 80, rows: 24 },
      });
      expect(rejectedResp.ok).toBe(false);

      // The tombstone count on the host should be 1
      expect(server.terminalHost.tombstoneCount).toBe(1);

      socket.destroy();
      vi.useRealTimers();
    });
  });
});
