import { socketAddress } from "../electron/daemon/socket-address";
/**
 * Latency Benchmark for PTY Daemon
 *
 * Measures end-to-end latency from mock subprocess emitData to stream socket
 * data event receipt. Computes p50, p95, p99 percentiles across multiple
 * iterations. Also measures snapshot delivery time on warm reattach with
 * scrollback.
 *
 * Targets (generous for CI):
 * - Data latency p99: <50ms
 * - Snapshot delivery (5000-line scrollback): <500ms
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

  constructor(pid = 1234) {
    this.pid = pid;
    this.fd = pid + 100;
  }

  get disposed(): boolean {
    return this._disposed;
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
// Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-latency-bench-"));
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
  opts: { clientId?: string; role?: "control" | "stream" } = {},
): Promise<HelloResponse> {
  const hello: HelloMessage = {
    type: "hello",
    version: PROTOCOL_VERSION,
    token,
    clientId: opts.clientId ?? "bench-client",
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

/**
 * Compute percentiles from a sorted array of numbers.
 */
function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.ceil((p / 100) * sorted.length) - 1;
  return sorted[Math.max(0, idx)]!;
}

/**
 * Wait for a single stream data event and return the receive timestamp.
 */
function waitForStreamData(
  socket: net.Socket,
  decoder: FrameDecoder,
  timeoutMs = 5000,
): Promise<{ receivedAt: number; data: Buffer }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      socket.removeListener("data", onData);
      reject(new Error("Timed out waiting for stream data"));
    }, timeoutMs);

    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();
      for (const frame of frames) {
        const event = frame.json as DaemonEvent;
        if (event.type === "event" && event.event === "data") {
          const receivedAt = performance.now();
          clearTimeout(timer);
          socket.removeListener("data", onData);
          resolve({ receivedAt, data: frame.payload ?? Buffer.alloc(0) });
          return;
        }
      }
    };

    socket.on("data", onData);
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Daemon Latency Benchmark", () => {
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
  // 1. End-to-end data latency (emitData → stream socket receipt)
  // =========================================================================

  describe("End-to-end data latency", () => {
    it("measures p50, p95, p99 latency from emitData to stream socket receipt", async () => {
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
        "latency-client",
      );

      // Create a session
      const createResp = await sendRequest(control, {
        id: "req-create",
        type: "createOrAttach",
        payload: { sessionId: "latency-session", cols: 80, rows: 24 },
      });
      expect(createResp.ok).toBe(true);

      const handle = spawned[0]!;
      const iterations = 100;
      const latencies: number[] = [];

      // Use a persistent decoder for the stream socket
      const streamDecoder = new FrameDecoder();

      for (let i = 0; i < iterations; i++) {
        const dataPromise = waitForStreamData(stream, streamDecoder, 5000);

        // Record send time and emit data
        const sentAt = performance.now();
        handle.emitData(`ping-${i}\r\n`);

        const { receivedAt } = await dataPromise;
        const latency = receivedAt - sentAt;
        latencies.push(latency);

        // Small delay between iterations to avoid batching interference
        // The batcher uses 8ms intervals, so we wait slightly longer
        await new Promise((r) => setTimeout(r, 12));
      }

      // Sort latencies for percentile computation
      latencies.sort((a, b) => a - b);

      const p50 = percentile(latencies, 50);
      const p95 = percentile(latencies, 95);
      const p99 = percentile(latencies, 99);

      // Log results for CI visibility
      console.log(`\n--- Data Latency Benchmark (${iterations} iterations) ---`);
      console.log(`  p50: ${p50.toFixed(2)}ms`);
      console.log(`  p95: ${p95.toFixed(2)}ms`);
      console.log(`  p99: ${p99.toFixed(2)}ms`);
      console.log(`  min: ${latencies[0]!.toFixed(2)}ms`);
      console.log(`  max: ${latencies[latencies.length - 1]!.toFixed(2)}ms`);
      console.log("---\n");

      // CI-friendly assertions (generous thresholds)
      // Target: <50ms p99 for data delivery
      expect(p99).toBeLessThan(50);
      expect(p95).toBeLessThan(40);
      expect(p50).toBeLessThan(20);

      control.destroy();
      stream.destroy();
    });

    it("measures latency under sustained low-throughput load (<1KB/s)", async () => {
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
        "low-throughput-client",
      );

      await sendRequest(control, {
        id: "req-create",
        type: "createOrAttach",
        payload: { sessionId: "low-throughput-session", cols: 80, rows: 24 },
      });

      const handle = spawned[0]!;
      const iterations = 50;
      const latencies: number[] = [];
      const streamDecoder = new FrameDecoder();

      // Simulate low-throughput: ~100 bytes every 100ms = ~1KB/s
      const smallPayload = "output-line-data-here\r\n"; // ~23 bytes

      for (let i = 0; i < iterations; i++) {
        const dataPromise = waitForStreamData(stream, streamDecoder, 5000);

        const sentAt = performance.now();
        handle.emitData(smallPayload);

        const { receivedAt } = await dataPromise;
        latencies.push(receivedAt - sentAt);

        // ~100ms between sends to simulate low throughput
        await new Promise((r) => setTimeout(r, 20));
      }

      latencies.sort((a, b) => a - b);

      const p50 = percentile(latencies, 50);
      const p95 = percentile(latencies, 95);
      const p99 = percentile(latencies, 99);

      console.log(`\n--- Low-Throughput Latency (${iterations} iterations) ---`);
      console.log(`  p50: ${p50.toFixed(2)}ms`);
      console.log(`  p95: ${p95.toFixed(2)}ms`);
      console.log(`  p99: ${p99.toFixed(2)}ms`);
      console.log("---\n");

      // Under low load, latency should be dominated by the 8ms batch timer
      expect(p99).toBeLessThan(50);
      expect(p50).toBeLessThan(20);

      control.destroy();
      stream.destroy();
    });
  });

  // =========================================================================
  // 2. Snapshot delivery time on warm reattach
  // =========================================================================

  describe("Snapshot delivery on warm reattach", () => {
    it("delivers snapshot within target time for 5000-line scrollback", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      // Phase 1: Create session and build up scrollback
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "snapshot-client-1",
      );

      await sendRequest(ctrl1, {
        id: "req-create",
        type: "createOrAttach",
        payload: { sessionId: "snapshot-session", cols: 80, rows: 24 },
      });

      const handle = spawned[0]!;

      // Emit 5000 lines of scrollback content
      // Each line is ~80 chars to simulate realistic terminal output
      const lineContent = "x".repeat(70) + "\r\n";
      const batchSize = 100;
      const totalLines = 5000;

      for (let batch = 0; batch < totalLines / batchSize; batch++) {
        let batchData = "";
        for (let i = 0; i < batchSize; i++) {
          batchData += lineContent;
        }
        handle.emitData(batchData);
      }

      // Wait for emulator to process all data
      await new Promise((r) => setTimeout(r, 200));

      // Phase 2: Disconnect (simulate app quit)
      ctrl1.destroy();
      stream1.destroy();
      await new Promise((r) => setTimeout(r, 100));

      // Phase 3: Reconnect and measure snapshot delivery time
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "snapshot-client-2",
      );

      const snapshotStart = performance.now();

      const reattachResp = await sendRequest(ctrl2, {
        id: "req-reattach",
        type: "createOrAttach",
        payload: { sessionId: "snapshot-session", cols: 80, rows: 24 },
      });

      const snapshotEnd = performance.now();
      const snapshotDeliveryMs = snapshotEnd - snapshotStart;

      expect(reattachResp.ok).toBe(true);
      const result = reattachResp.payload as {
        isNew: boolean;
        snapshot: { snapshotAnsi: string; scrollbackAnsi: string; scrollbackLines: number } | null;
      };
      expect(result.isNew).toBe(false);
      expect(result.snapshot).not.toBeNull();

      // Verify snapshot has substantial content
      expect(result.snapshot!.snapshotAnsi.length).toBeGreaterThan(0);

      console.log(`\n--- Snapshot Delivery Benchmark ---`);
      console.log(`  Scrollback lines emitted: ${totalLines}`);
      console.log(`  Snapshot ANSI size: ${result.snapshot!.snapshotAnsi.length} bytes`);
      console.log(`  Scrollback ANSI size: ${result.snapshot!.scrollbackAnsi.length} bytes`);
      console.log(`  Delivery time: ${snapshotDeliveryMs.toFixed(2)}ms`);
      console.log("---\n");

      // CI-friendly assertion: <500ms for snapshot delivery
      expect(snapshotDeliveryMs).toBeLessThan(500);

      ctrl2.destroy();
      stream2.destroy();
    });

    it("measures snapshot delivery for multiple sessions on reattach", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({
        ...paths,
        spawnSubprocess: spawner,
        idleTimeoutMs: 0,
      });
      await server.start();

      const sessionCount = 5;

      // Phase 1: Create sessions with scrollback
      const { control: ctrl1, stream: stream1 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "multi-snap-client-1",
      );

      for (let s = 0; s < sessionCount; s++) {
        await sendRequest(ctrl1, {
          id: `create-${s}`,
          type: "createOrAttach",
          payload: { sessionId: `multi-snap-${s}`, cols: 80, rows: 24 },
        });
      }

      // Emit 1000 lines per session
      const lineContent = "y".repeat(60) + "\r\n";
      for (let s = 0; s < sessionCount; s++) {
        let data = "";
        for (let i = 0; i < 1000; i++) {
          data += lineContent;
        }
        spawned[s]!.emitData(data);
      }

      await new Promise((r) => setTimeout(r, 200));

      // Phase 2: Disconnect
      ctrl1.destroy();
      stream1.destroy();
      await new Promise((r) => setTimeout(r, 100));

      // Phase 3: Reconnect and measure total reattach time
      const { control: ctrl2, stream: stream2 } = await connectClient(
        paths.socketPath,
        server.authToken,
        "multi-snap-client-2",
      );

      const reattachStart = performance.now();

      for (let s = 0; s < sessionCount; s++) {
        const resp = await sendRequest(ctrl2, {
          id: `reattach-${s}`,
          type: "createOrAttach",
          payload: { sessionId: `multi-snap-${s}`, cols: 80, rows: 24 },
        });
        expect(resp.ok).toBe(true);
        const result = resp.payload as { isNew: boolean; snapshot: unknown };
        expect(result.isNew).toBe(false);
        expect(result.snapshot).not.toBeNull();
      }

      const reattachEnd = performance.now();
      const totalReattachMs = reattachEnd - reattachStart;

      console.log(`\n--- Multi-Session Reattach Benchmark ---`);
      console.log(`  Sessions: ${sessionCount}`);
      console.log(`  Lines per session: 1000`);
      console.log(`  Total reattach time: ${totalReattachMs.toFixed(2)}ms`);
      console.log(`  Per-session average: ${(totalReattachMs / sessionCount).toFixed(2)}ms`);
      console.log("---\n");

      // All sessions should reattach within 500ms total
      expect(totalReattachMs).toBeLessThan(500);

      ctrl2.destroy();
      stream2.destroy();
    });
  });

  // =========================================================================
  // 3. Daemon overhead comparison (simulated)
  // =========================================================================

  describe("Daemon overhead measurement", () => {
    it("measures daemon data path overhead vs direct callback baseline", async () => {
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
        "overhead-client",
      );

      await sendRequest(control, {
        id: "req-create",
        type: "createOrAttach",
        payload: { sessionId: "overhead-session", cols: 80, rows: 24 },
      });

      const handle = spawned[0]!;
      const streamDecoder = new FrameDecoder();

      // Measure daemon path latency
      const daemonLatencies: number[] = [];
      // A nearest-rank p99 with fewer than 100 samples measures the maximum.
      const iterations = 100;

      for (let i = 0; i < iterations; i++) {
        const dataPromise = waitForStreamData(stream, streamDecoder, 5000);
        const sentAt = performance.now();
        handle.emitData(`overhead-test-${i}\n`);
        const { receivedAt } = await dataPromise;
        daemonLatencies.push(receivedAt - sentAt);
        await new Promise((r) => setTimeout(r, 12));
      }

      // Measure in-process baseline: direct callback invocation
      const directLatencies: number[] = [];
      for (let i = 0; i < iterations; i++) {
        const sentAt = performance.now();
        // Simulate in-process: direct callback (no socket, no framing)
        const _data = Buffer.from(`direct-test-${i}\n`);
        const receivedAt = performance.now();
        directLatencies.push(receivedAt - sentAt);
      }

      daemonLatencies.sort((a, b) => a - b);
      directLatencies.sort((a, b) => a - b);

      const daemonP50 = percentile(daemonLatencies, 50);
      const daemonP99 = percentile(daemonLatencies, 99);
      const directP50 = percentile(directLatencies, 50);
      const directP99 = percentile(directLatencies, 99);

      // The overhead is the daemon latency minus the direct baseline
      const overheadP50 = daemonP50 - directP50;
      const overheadP99 = daemonP99 - directP99;

      console.log(`\n--- Daemon Overhead Benchmark ---`);
      console.log(`  Daemon p50: ${daemonP50.toFixed(2)}ms, p99: ${daemonP99.toFixed(2)}ms`);
      console.log(`  Direct p50: ${directP50.toFixed(4)}ms, p99: ${directP99.toFixed(4)}ms`);
      console.log(`  Overhead p50: ${overheadP50.toFixed(2)}ms`);
      console.log(`  Overhead p99: ${overheadP99.toFixed(2)}ms`);
      console.log("---\n");

      // The daemon overhead should be reasonable for CI
      // Most of the latency comes from the 8ms batch timer + socket I/O
      // Target: daemon adds <50ms overhead at p99 (generous for CI)
      expect(daemonP99).toBeLessThan(50);

      control.destroy();
      stream.destroy();
    });
  });
});
