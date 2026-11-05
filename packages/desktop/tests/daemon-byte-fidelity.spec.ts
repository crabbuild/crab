import { socketAddress } from "../electron/daemon/socket-address";
/**
 * Byte-Fidelity Test Suite
 *
 * Verifies that random binary data (including non-UTF-8 bytes, null bytes,
 * partial multi-byte sequences) written via the daemon arrives byte-perfectly
 * on the stream socket. Also verifies snapshot replay fidelity on reattach.
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

  writtenData: string[] = [];
  resizes: Array<{ cols: number; rows: number }> = [];
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
  }

  forceKill(): void {
    this.forceKillCalled = true;
  }

  signal(sig: string): void {}
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
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-byte-test-"));
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

/**
 * Collect stream data events, concatenating all binary payloads for a session.
 * Returns the concatenated buffer once expectedBytes are received or timeout.
 */
function collectStreamData(
  socket: net.Socket,
  sessionId: string,
  expectedBytes: number,
  timeoutMs = 3000,
): Promise<Buffer> {
  return new Promise((resolve) => {
    const chunks: Buffer[] = [];
    let totalLen = 0;
    const decoder = new FrameDecoder();

    const timer = setTimeout(() => {
      socket.removeListener("data", onData);
      resolve(Buffer.concat(chunks));
    }, timeoutMs);

    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      for (const frame of frames) {
        const event = frame.json as DaemonEvent;
        if (event.type === "event" && event.event === "data" && event.sessionId === sessionId) {
          if (frame.payload) {
            chunks.push(frame.payload);
            totalLen += frame.payload.length;
            if (totalLen >= expectedBytes) {
              clearTimeout(timer);
              socket.removeListener("data", onData);
              resolve(Buffer.concat(chunks));
              return;
            }
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
// Problematic byte patterns for testing
// ---------------------------------------------------------------------------

/** Null bytes */
const NULL_BYTES = Buffer.from([0x00, 0x00, 0x00, 0x00, 0x00]);

/** All 0xFF bytes (invalid in UTF-8) */
const ALL_FF = Buffer.from([0xFF, 0xFF, 0xFF, 0xFF]);

/** Partial UTF-8 multi-byte: start of 3-byte sequence without continuation */
const PARTIAL_UTF8_3BYTE = Buffer.from([0xE0, 0xA0]);

/** Partial UTF-8 multi-byte: start of 4-byte sequence without continuation */
const PARTIAL_UTF8_4BYTE = Buffer.from([0xF0, 0x90, 0x80]);

/** Control characters (C0 range) */
const CONTROL_CHARS = Buffer.from([
  0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
  0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15,
  0x16, 0x17, 0x18, 0x19, 0x1A, 0x1C, 0x1D, 0x1E, 0x1F,
]);

/** Mixed binary: valid ASCII + nulls + high bytes + partial UTF-8 */
const MIXED_BINARY = Buffer.from([
  0x48, 0x65, 0x6C, 0x6C, 0x6F, // "Hello"
  0x00, 0x00,                     // null bytes
  0xFF, 0xFE, 0xFD,              // high bytes (invalid UTF-8)
  0xE2, 0x80,                     // partial 3-byte UTF-8 (€ without last byte)
  0x57, 0x6F, 0x72, 0x6C, 0x64, // "World"
  0xF4, 0x8F,                     // partial 4-byte UTF-8
  0x00,                           // trailing null
]);

/** Generate a buffer of random bytes including problematic patterns */
function generateRandomBinaryData(size: number): Buffer {
  const buf = Buffer.alloc(size);
  for (let i = 0; i < size; i++) {
    buf[i] = Math.floor(Math.random() * 256);
  }
  return buf;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Byte-Fidelity Test Suite", () => {
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
  // 1. Live Streaming Byte Fidelity
  // =========================================================================

  describe("Live streaming byte fidelity", () => {
    it("null bytes arrive byte-perfectly on stream socket", async () => {
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
        "null-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-null", cols: 80, rows: 24 },
      });

      // Emit null bytes from the mock subprocess as a string
      // (node-pty delivers data as strings)
      const inputStr = NULL_BYTES.toString("utf-8");
      const dataPromise = collectStreamData(stream, "sess-null", NULL_BYTES.length);

      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      // The data path converts string → Buffer via Buffer.from(str, "utf-8")
      // so we compare against the same encoding
      const expected = Buffer.from(inputStr, "utf-8");
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });

    it("0xFF bytes (non-UTF-8) arrive byte-perfectly on stream socket", async () => {
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
        "ff-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-ff", cols: 80, rows: 24 },
      });

      const inputStr = ALL_FF.toString("utf-8");
      const expected = Buffer.from(inputStr, "utf-8");
      const dataPromise = collectStreamData(stream, "sess-ff", expected.length);

      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });

    it("partial multi-byte UTF-8 sequences arrive byte-perfectly", async () => {
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
        "partial-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-partial", cols: 80, rows: 24 },
      });

      // Combine partial 3-byte and 4-byte sequences
      const combined = Buffer.concat([PARTIAL_UTF8_3BYTE, PARTIAL_UTF8_4BYTE]);
      const inputStr = combined.toString("utf-8");
      const expected = Buffer.from(inputStr, "utf-8");
      const dataPromise = collectStreamData(stream, "sess-partial", expected.length);

      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });

    it("control characters arrive byte-perfectly", async () => {
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
        "ctrl-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-ctrl", cols: 80, rows: 24 },
      });

      const inputStr = CONTROL_CHARS.toString("utf-8");
      const expected = Buffer.from(inputStr, "utf-8");
      const dataPromise = collectStreamData(stream, "sess-ctrl", expected.length);

      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });

    it("mixed binary data (ASCII + nulls + high bytes + partial UTF-8) arrives byte-perfectly", async () => {
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
        "mixed-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-mixed", cols: 80, rows: 24 },
      });

      const inputStr = MIXED_BINARY.toString("utf-8");
      const expected = Buffer.from(inputStr, "utf-8");
      const dataPromise = collectStreamData(stream, "sess-mixed", expected.length);

      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });

    it("large random binary data (4KB) arrives byte-perfectly", async () => {
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
        "large-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-large", cols: 80, rows: 24 },
      });

      const randomData = generateRandomBinaryData(4096);
      const inputStr = randomData.toString("utf-8");
      const expected = Buffer.from(inputStr, "utf-8");
      const dataPromise = collectStreamData(stream, "sess-large", expected.length);

      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });

    it("multiple sequential binary chunks arrive in order and byte-perfectly", async () => {
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
        "seq-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-seq", cols: 80, rows: 24 },
      });

      // Emit multiple chunks with different problematic patterns
      const chunks = [
        NULL_BYTES.toString("utf-8"),
        ALL_FF.toString("utf-8"),
        CONTROL_CHARS.toString("utf-8"),
        MIXED_BINARY.toString("utf-8"),
      ];

      const allInput = chunks.join("");
      const expected = Buffer.from(allInput, "utf-8");
      const dataPromise = collectStreamData(stream, "sess-seq", expected.length);

      for (const chunk of chunks) {
        spawned[0]!.emitData(chunk);
      }
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      control.destroy();
      stream.destroy();
    });
  });

  // =========================================================================
  // 2. Snapshot Replay Fidelity (Reattach Path)
  // =========================================================================

  describe("Snapshot replay fidelity (reattach path)", () => {
    it("snapshot preserves terminal content after binary data emission", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Client 1 creates session and emits binary data
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "snap-c1",
      );

      await sendRequest(ctrl1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-snap", cols: 80, rows: 24 },
      });

      // Emit mixed binary data
      const inputStr = MIXED_BINARY.toString("utf-8");
      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      // Get snapshot via getSnapshot request
      const snapResp = await sendRequest(ctrl1, {
        id: "req-2",
        type: "getSnapshot",
        payload: { sessionId: "sess-snap" },
      });

      expect(snapResp.ok).toBe(true);
      const snapshot = snapResp.payload as { snapshotAnsi: string } | null;
      expect(snapshot).not.toBeNull();
      // Snapshot should contain some representation of the data
      expect(snapshot!.snapshotAnsi).toBeDefined();

      ctrl1.destroy();
      stream1.destroy();
    });

    it("reattach snapshot is consistent between getSnapshot and createOrAttach", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Client 1 creates session and emits data
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "reattach-c1",
      );

      await sendRequest(ctrl1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-reattach-snap", cols: 80, rows: 24 },
      });

      // Emit various problematic byte patterns
      spawned[0]!.emitData(NULL_BYTES.toString("utf-8"));
      spawned[0]!.emitData(ALL_FF.toString("utf-8"));
      spawned[0]!.emitData(CONTROL_CHARS.toString("utf-8"));
      await new Promise((r) => setTimeout(r, 50));

      // Get snapshot via explicit request
      const snapResp = await sendRequest(ctrl1, {
        id: "req-snap",
        type: "getSnapshot",
        payload: { sessionId: "sess-reattach-snap" },
      });
      expect(snapResp.ok).toBe(true);
      const explicitSnapshot = snapResp.payload as { snapshotAnsi: string };

      // Client 2 reattaches — should get the same snapshot
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "reattach-c2",
      );

      const reattachResp = await sendRequest(ctrl2, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-reattach-snap", cols: 80, rows: 24 },
      });

      expect(reattachResp.ok).toBe(true);
      const result = reattachResp.payload as {
        isNew: boolean;
        snapshot: { snapshotAnsi: string } | null;
      };
      expect(result.isNew).toBe(false);
      expect(result.snapshot).not.toBeNull();
      expect(result.snapshot!.snapshotAnsi).toBe(explicitSnapshot.snapshotAnsi);

      ctrl1.destroy();
      stream1.destroy();
      ctrl2.destroy();
      stream2.destroy();
    });

    it("snapshot after large random data preserves content fidelity", async () => {
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
        "large-snap-client",
      );

      await sendRequest(control, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-large-snap", cols: 80, rows: 24 },
      });

      // Emit a large chunk of random data
      const randomData = generateRandomBinaryData(2048);
      const inputStr = randomData.toString("utf-8");
      spawned[0]!.emitData(inputStr);
      await new Promise((r) => setTimeout(r, 50));

      // Get snapshot
      const snapResp = await sendRequest(control, {
        id: "req-2",
        type: "getSnapshot",
        payload: { sessionId: "sess-large-snap" },
      });

      expect(snapResp.ok).toBe(true);
      const snapshot = snapResp.payload as { snapshotAnsi: string } | null;
      expect(snapshot).not.toBeNull();
      // The snapshot should be non-empty (emulator processed the data)
      expect(snapshot!.snapshotAnsi.length).toBeGreaterThan(0);

      // Reattach and verify same snapshot
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "large-snap-c2",
      );

      const reattachResp = await sendRequest(ctrl2, {
        id: "req-3",
        type: "createOrAttach",
        payload: { sessionId: "sess-large-snap", cols: 80, rows: 24 },
      });

      expect(reattachResp.ok).toBe(true);
      const result = reattachResp.payload as {
        isNew: boolean;
        snapshot: { snapshotAnsi: string } | null;
      };
      expect(result.isNew).toBe(false);
      expect(result.snapshot).not.toBeNull();
      expect(result.snapshot!.snapshotAnsi).toBe(snapshot!.snapshotAnsi);

      control.destroy();
      stream.destroy();
      ctrl2.destroy();
      stream2.destroy();
    });
  });

  // =========================================================================
  // 3. Both Live and Reattach Paths Maintain Data Integrity
  // =========================================================================

  describe("Combined live + reattach data integrity", () => {
    it("live stream data matches what reattach client receives on subsequent output", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Client 1 creates session
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "integrity-c1",
      );

      await sendRequest(ctrl1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-integrity", cols: 80, rows: 24 },
      });

      // Emit initial binary data (only client 1 sees this live)
      const initialData = MIXED_BINARY.toString("utf-8");
      spawned[0]!.emitData(initialData);
      await new Promise((r) => setTimeout(r, 50));

      // Client 2 reattaches (gets snapshot of initial data)
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "integrity-c2",
      );

      const reattachResp = await sendRequest(ctrl2, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-integrity", cols: 80, rows: 24 },
      });

      expect(reattachResp.ok).toBe(true);
      const result = reattachResp.payload as { isNew: boolean };
      expect(result.isNew).toBe(false);

      // Now emit more binary data — both clients should receive it identically
      const newData = Buffer.concat([NULL_BYTES, ALL_FF, CONTROL_CHARS]);
      const newDataStr = newData.toString("utf-8");
      const expected = Buffer.from(newDataStr, "utf-8");

      const data1Promise = collectStreamData(stream1, "sess-integrity", expected.length);
      const data2Promise = collectStreamData(stream2, "sess-integrity", expected.length);

      spawned[0]!.emitData(newDataStr);
      await new Promise((r) => setTimeout(r, 50));

      const received1 = await data1Promise;
      const received2 = await data2Promise;

      // Both clients receive identical byte-perfect data
      expect(received1.equals(expected)).toBe(true);
      expect(received2.equals(expected)).toBe(true);
      expect(received1.equals(received2)).toBe(true);

      ctrl1.destroy();
      ctrl2.destroy();
      stream1.destroy();
      stream2.destroy();
    });

    it("reattach after binary data emission preserves snapshot and continues streaming", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Client 1 creates session and emits binary data
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "persist-c1",
      );

      await sendRequest(ctrl1, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-persist", cols: 80, rows: 24 },
      });

      // Emit binary data while client 1 is connected
      const binaryStr = generateRandomBinaryData(512).toString("utf-8");
      spawned[0]!.emitData(binaryStr);
      await new Promise((r) => setTimeout(r, 50));

      // Disconnect client 1 (session continues running)
      ctrl1.destroy();
      stream1.destroy();
      await new Promise((r) => setTimeout(r, 100));

      // Emit more data while no client is connected (emulator still tracks)
      const moreData = ALL_FF.toString("utf-8");
      spawned[0]!.emitData(moreData);
      await new Promise((r) => setTimeout(r, 50));

      // Client 2 reattaches
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "persist-c2",
      );

      const reattachResp = await sendRequest(ctrl2, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-persist", cols: 80, rows: 24 },
      });

      expect(reattachResp.ok).toBe(true);
      const result = reattachResp.payload as {
        isNew: boolean;
        snapshot: { snapshotAnsi: string } | null;
      };
      expect(result.isNew).toBe(false);
      // Snapshot should exist (emulator tracked data even without clients)
      expect(result.snapshot).not.toBeNull();
      expect(result.snapshot!.snapshotAnsi.length).toBeGreaterThan(0);

      // Verify new data still streams correctly after reattach
      const postReattachData = CONTROL_CHARS.toString("utf-8");
      const expected = Buffer.from(postReattachData, "utf-8");
      const dataPromise = collectStreamData(stream2, "sess-persist", expected.length);

      spawned[0]!.emitData(postReattachData);
      await new Promise((r) => setTimeout(r, 50));

      const received = await dataPromise;
      expect(received.equals(expected)).toBe(true);

      ctrl2.destroy();
      stream2.destroy();
    });
  });
});
