/**
 * Cold-restore integration tests.
 *
 * Verifies that when the daemon starts and finds sessions with a meta.json
 * (no endedAt) plus a checkpoint.json, it broadcasts cold-restore-available
 * events to connected clients. Also drives the resume and discard paths and
 * asserts both clean up correctly.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { DaemonServer } from "../electron/daemon/daemon-server";
import { HistoryManager } from "../electron/daemon/history-manager";
import { socketAddress } from "../electron/daemon/socket-address";
import {
  PROTOCOL_VERSION,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
  type DaemonEvent,
  type ColdRestorePayload,
} from "../electron/daemon/protocol";
import type { SubprocessHandle, StreamClient } from "../electron/daemon/types";
import type { SubprocessSpawner, SubprocessSpawnOptions } from "../electron/daemon/subprocess-handle";
import type { TerminalSnapshot } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Mock subprocess handle (no real PTY needed)
// ---------------------------------------------------------------------------

class MockSubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];
  private _disposed = false;
  private _paused = false;

  constructor(pid = 9000) {
    this.pid = pid;
    this.fd = pid + 100;
  }

  get disposed(): boolean { return this._disposed; }
  get paused(): boolean { return this._paused; }

  write(_data: string): void {}
  resize(_cols: number, _rows: number): void {}
  kill(_signal?: string): void {}
  forceKill(): void {}
  signal(_sig: string): void {}
  pause(): void { this._paused = true; }
  resume(): void { this._paused = false; }

  onData(cb: (data: string) => void): void { this.dataCallbacks.push(cb); }
  onExit(cb: (exitCode: number) => void): void { this.exitCallbacks.push(cb); }

  dispose(): void {
    this._disposed = true;
    this.dataCallbacks = [];
    this.exitCallbacks = [];
  }

  emitExit(code: number): void {
    for (const cb of [...this.exitCallbacks]) cb(code);
  }
}

function createMockSpawner(): { spawner: SubprocessSpawner; spawned: MockSubprocessHandle[] } {
  let nextPid = 9000;
  const spawned: MockSubprocessHandle[] = [];
  const spawner: SubprocessSpawner = (_opts: SubprocessSpawnOptions) => {
    const handle = new MockSubprocessHandle(nextPid++);
    spawned.push(handle);
    return handle;
  };
  return { spawner, spawned };
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "cold-restore-test-"));
}

function createServerPaths(dir: string) {
  return {
    socketPath: path.join(dir, "daemon-v1.sock"),
    tokenPath: path.join(dir, "daemon-v1.token"),
    pidPath: path.join(dir, "daemon-v1.pid"),
  };
}

function makeSnapshot(overrides?: Partial<TerminalSnapshot>): TerminalSnapshot {
  return {
    snapshotAnsi: "$ echo hello\r\nhello\r\n",
    scrollbackAnsi: "",
    rehydrateSequences: "",
    cwd: "/home/user",
    modes: {
      bracketedPaste: false,
      mouseTracking: false,
      mouseTrackingMode: "none",
      sgrMouseMode: false,
      applicationCursor: false,
      alternateScreen: false,
    },
    cols: 80,
    rows: 24,
    scrollbackLines: 0,
    ...overrides,
  };
}

/**
 * Write a meta.json (without endedAt) and a checkpoint.json to simulate
 * a session that was alive when the daemon crashed.
 */
function writeCrashedSession(
  historyBasePath: string,
  sessionId: string,
  snapshot: TerminalSnapshot,
  opts: {
    cwd?: string;
    cols?: number;
    rows?: number;
    sanitizedKeys?: string[];
    savedAt?: number;
  } = {},
): void {
  const dirPath = path.join(historyBasePath, sessionId);
  fs.mkdirSync(dirPath, { recursive: true });

  const meta = {
    sessionId,
    cwd: opts.cwd ?? "/home/user",
    cols: opts.cols ?? 80,
    rows: opts.rows ?? 24,
    sanitizedKeys: opts.sanitizedKeys ?? [],
    createdAt: new Date(Date.now() - 60_000).toISOString(),
    // No endedAt — simulates unclean shutdown
  };
  fs.writeFileSync(path.join(dirPath, "meta.json"), JSON.stringify(meta, null, 2), "utf-8");

  const checkpoint = {
    snapshot,
    checkpointedAt: new Date(opts.savedAt ?? Date.now() - 30_000).toISOString(),
  };
  fs.writeFileSync(
    path.join(dirPath, "checkpoint.json"),
    JSON.stringify(checkpoint),
    "utf-8",
  );
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

/**
 * Collect all stream events received within a timeout window.
 */
async function collectStreamEvents(
  socket: net.Socket,
  timeoutMs: number,
): Promise<DaemonEvent[]> {
  const events: DaemonEvent[] = [];
  const decoder = new FrameDecoder();

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      socket.removeListener("data", onData);
      resolve(events);
    }, timeoutMs);

    const onData = (chunk: Buffer) => {
      decoder.push(chunk);
      for (const frame of decoder.frames()) {
        events.push(frame.json as DaemonEvent);
      }
    };

    socket.on("data", onData);
    socket.once("close", () => {
      clearTimeout(timer);
      socket.removeListener("data", onData);
      resolve(events);
    });
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("cold-restore", () => {
  let tmpDir: string;
  let historyDir: string;
  let server: DaemonServer | null;
  let spawner: SubprocessSpawner;

  beforeEach(() => {
    tmpDir = createTmpDir();
    // History lives in a sub-directory of the temp dir, mirroring the real
    // ~/.crab/desktop/terminal-history layout.
    historyDir = path.join(tmpDir, "terminal-history");
    fs.mkdirSync(historyDir, { recursive: true });
    server = null;
    const mock = createMockSpawner();
    spawner = mock.spawner;
  });

  afterEach(async () => {
    if (server) {
      await server.shutdown({ killSessions: true });
      server = null;
    }
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  // -------------------------------------------------------------------------
  // cold-restore-available broadcast
  // -------------------------------------------------------------------------

  describe("cold-restore-available event", () => {
    it("broadcasts cold-restore-available to a connected stream client for each crashed session", async () => {
      const sessionId = "crashed-sess-1";
      const snapshot = makeSnapshot({
        snapshotAnsi: "$ git status\r\nOn branch main\r\n",
        cwd: "/workspace/project",
        cols: 120,
        rows: 40,
      });
      const savedAt = Date.now() - 5_000;

      writeCrashedSession(historyDir, sessionId, snapshot, {
        cwd: "/workspace/project",
        cols: 120,
        rows: 40,
        savedAt,
      });

      // Build a HistoryManager pointing at the temp history dir so the server
      // can detect the crashed session.
      const historyManager = new HistoryManager({ basePath: historyDir });
      const coldRestoreSessions = historyManager.listColdRestoreSessions();
      expect(coldRestoreSessions).toHaveLength(1);

      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({ ...paths, spawnSubprocess: spawner, idleTimeoutMs: 0 });
      await server.start();

      // Connect a client (control + stream) before broadcasting.
      const controlSocket = await connectSocket(paths.socketPath);
      const helloResp = await sendHello(controlSocket, server.authToken, {
        clientId: "client-1",
        role: "control",
      });
      expect(helloResp.ok).toBe(true);

      const streamSocket = await connectSocket(paths.socketPath);
      const streamHello = await sendHello(streamSocket, server.authToken, {
        clientId: "client-1",
        role: "stream",
      });
      expect(streamHello.ok).toBe(true);

      // Start collecting stream events before broadcasting.
      const eventsPromise = collectStreamEvents(streamSocket, 300);

      // Broadcast cold-restore-available for each detected session (mirrors daemon-main.ts).
      for (const session of coldRestoreSessions) {
        server.broadcastToAllClients({
          type: "event",
          event: "cold-restore-available",
          sessionId: session.sessionId,
          payload: {
            snapshot: session.snapshot,
            savedAt: session.savedAt,
            sanitizedKeys: session.sanitizedKeys,
            cwd: session.cwd,
            cols: session.cols,
            rows: session.rows,
          },
        });
      }

      const events = await eventsPromise;

      const coldRestoreEvents = events.filter((e) => e.event === "cold-restore-available");
      expect(coldRestoreEvents).toHaveLength(1);

      const evt = coldRestoreEvents[0] as Extract<DaemonEvent, { event: "cold-restore-available" }>;
      expect(evt.sessionId).toBe(sessionId);
      expect(evt.payload.cwd).toBe("/workspace/project");
      expect(evt.payload.cols).toBe(120);
      expect(evt.payload.rows).toBe(40);
      expect(evt.payload.sanitizedKeys).toEqual([]);
      expect(evt.payload.snapshot.snapshotAnsi).toBe("$ git status\r\nOn branch main\r\n");
      expect(typeof evt.payload.savedAt).toBe("number");

      controlSocket.destroy();
      streamSocket.destroy();
    });

    it("broadcasts cold-restore-available for multiple crashed sessions", async () => {
      const snapshot1 = makeSnapshot({ snapshotAnsi: "session A output" });
      const snapshot2 = makeSnapshot({ snapshotAnsi: "session B output" });

      writeCrashedSession(historyDir, "sess-a", snapshot1, { cwd: "/a" });
      writeCrashedSession(historyDir, "sess-b", snapshot2, { cwd: "/b" });

      const historyManager = new HistoryManager({ basePath: historyDir });
      const coldRestoreSessions = historyManager.listColdRestoreSessions();
      expect(coldRestoreSessions).toHaveLength(2);

      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({ ...paths, spawnSubprocess: spawner, idleTimeoutMs: 0 });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { clientId: "c1", role: "control" });

      const streamSocket = await connectSocket(paths.socketPath);
      await sendHello(streamSocket, server.authToken, { clientId: "c1", role: "stream" });

      const eventsPromise = collectStreamEvents(streamSocket, 300);

      for (const session of coldRestoreSessions) {
        server.broadcastToAllClients({
          type: "event",
          event: "cold-restore-available",
          sessionId: session.sessionId,
          payload: {
            snapshot: session.snapshot,
            savedAt: session.savedAt,
            sanitizedKeys: session.sanitizedKeys,
            cwd: session.cwd,
            cols: session.cols,
            rows: session.rows,
          },
        });
      }

      const events = await eventsPromise;
      const coldRestoreEvents = events.filter((e) => e.event === "cold-restore-available");
      expect(coldRestoreEvents).toHaveLength(2);

      const sessionIds = coldRestoreEvents.map((e) => e.sessionId).sort();
      expect(sessionIds).toEqual(["sess-a", "sess-b"]);

      controlSocket.destroy();
      streamSocket.destroy();
    });

    it("includes sanitizedKeys in the payload when env secrets were stripped", async () => {
      const sessionId = "sess-with-secrets";
      const snapshot = makeSnapshot();

      writeCrashedSession(historyDir, sessionId, snapshot, {
        sanitizedKeys: ["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"],
      });

      const historyManager = new HistoryManager({ basePath: historyDir });
      const coldRestoreSessions = historyManager.listColdRestoreSessions();
      expect(coldRestoreSessions).toHaveLength(1);

      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({ ...paths, spawnSubprocess: spawner, idleTimeoutMs: 0 });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { clientId: "c1", role: "control" });

      const streamSocket = await connectSocket(paths.socketPath);
      await sendHello(streamSocket, server.authToken, { clientId: "c1", role: "stream" });

      const eventsPromise = collectStreamEvents(streamSocket, 300);

      for (const session of coldRestoreSessions) {
        server.broadcastToAllClients({
          type: "event",
          event: "cold-restore-available",
          sessionId: session.sessionId,
          payload: {
            snapshot: session.snapshot,
            savedAt: session.savedAt,
            sanitizedKeys: session.sanitizedKeys,
            cwd: session.cwd,
            cols: session.cols,
            rows: session.rows,
          },
        });
      }

      const events = await eventsPromise;
      const coldRestoreEvents = events.filter((e) => e.event === "cold-restore-available");
      expect(coldRestoreEvents).toHaveLength(1);

      const evt = coldRestoreEvents[0] as Extract<DaemonEvent, { event: "cold-restore-available" }>;
      expect(evt.payload.sanitizedKeys).toEqual(["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"]);
    });

    it("does not broadcast for sessions that ended cleanly (endedAt present)", async () => {
      // Write a cleanly-ended session (has endedAt in meta.json)
      const sessionId = "clean-sess";
      const dirPath = path.join(historyDir, sessionId);
      fs.mkdirSync(dirPath, { recursive: true });

      const meta = {
        sessionId,
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        createdAt: new Date(Date.now() - 120_000).toISOString(),
        endedAt: new Date(Date.now() - 60_000).toISOString(), // cleanly ended
        exitCode: 0,
      };
      fs.writeFileSync(path.join(dirPath, "meta.json"), JSON.stringify(meta), "utf-8");
      fs.writeFileSync(
        path.join(dirPath, "checkpoint.json"),
        JSON.stringify({ snapshot: makeSnapshot(), checkpointedAt: new Date().toISOString() }),
        "utf-8",
      );

      const historyManager = new HistoryManager({ basePath: historyDir });
      const coldRestoreSessions = historyManager.listColdRestoreSessions();
      expect(coldRestoreSessions).toHaveLength(0);
    });

    it("does not broadcast for sessions without a checkpoint file", async () => {
      // Write only meta.json (no checkpoint.json)
      const sessionId = "no-checkpoint-sess";
      const dirPath = path.join(historyDir, sessionId);
      fs.mkdirSync(dirPath, { recursive: true });

      const meta = {
        sessionId,
        cwd: "/tmp",
        cols: 80,
        rows: 24,
        createdAt: new Date().toISOString(),
        // No endedAt
      };
      fs.writeFileSync(path.join(dirPath, "meta.json"), JSON.stringify(meta), "utf-8");

      const historyManager = new HistoryManager({ basePath: historyDir });
      const coldRestoreSessions = historyManager.listColdRestoreSessions();
      expect(coldRestoreSessions).toHaveLength(0);
    });
  });

  // -------------------------------------------------------------------------
  // Resume path
  // -------------------------------------------------------------------------

  describe("resume path", () => {
    it("openSession on resume overwrites meta.json with fresh createdAt and no endedAt", () => {
      const sessionId = "resume-sess";
      const snapshot = makeSnapshot({ snapshotAnsi: "$ ls\r\nfoo bar\r\n" });

      writeCrashedSession(historyDir, sessionId, snapshot, { cwd: "/workspace" });

      const historyManager = new HistoryManager({ basePath: historyDir });

      // Verify the session is detected as a cold-restore candidate
      const coldRestoreSessions = historyManager.listColdRestoreSessions();
      expect(coldRestoreSessions).toHaveLength(1);
      expect(coldRestoreSessions[0]!.sessionId).toBe(sessionId);

      // Simulate resume: call openSession again (as daemon-main.ts would on cold-restore-resume)
      historyManager.openSession(sessionId, {
        cwd: "/workspace",
        cols: 80,
        rows: 24,
      });

      // After openSession, the old checkpoint.json is removed (openSession cleans stale checkpoints)
      const checkpointPath = path.join(historyDir, sessionId, "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);

      // meta.json should exist with a fresh createdAt and no endedAt
      const metaPath = path.join(historyDir, sessionId, "meta.json");
      expect(fs.existsSync(metaPath)).toBe(true);
      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.endedAt).toBeUndefined();
      expect(meta.cwd).toBe("/workspace");
    });

    it("session is no longer a cold-restore candidate after openSession (resume)", () => {
      const sessionId = "resume-sess-2";
      const snapshot = makeSnapshot();

      writeCrashedSession(historyDir, sessionId, snapshot);

      const historyManager = new HistoryManager({ basePath: historyDir });

      // Confirm it's a cold-restore candidate before resume
      expect(historyManager.listColdRestoreSessions()).toHaveLength(1);

      // Resume: openSession clears the old checkpoint
      historyManager.openSession(sessionId, { cwd: "/tmp", cols: 80, rows: 24 });

      // Now it should no longer appear as a cold-restore candidate
      // (checkpoint.json was removed by openSession)
      const freshHm = new HistoryManager({ basePath: historyDir });
      expect(freshHm.listColdRestoreSessions()).toHaveLength(0);
    });

    it("closeSession after resume writes endedAt and cleans up in-memory state", () => {
      const sessionId = "resume-close-sess";
      const snapshot = makeSnapshot();

      writeCrashedSession(historyDir, sessionId, snapshot);

      const historyManager = new HistoryManager({ basePath: historyDir });
      historyManager.openSession(sessionId, { cwd: "/tmp", cols: 80, rows: 24 });

      // Simulate session running and then exiting
      historyManager.closeSession(sessionId, 0);

      const metaPath = path.join(historyDir, sessionId, "meta.json");
      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.endedAt).toBeDefined();
      expect(meta.exitCode).toBe(0);

      // After close, the session should not be a cold-restore candidate
      const freshHm = new HistoryManager({ basePath: historyDir });
      expect(freshHm.listColdRestoreSessions()).toHaveLength(0);
    });
  });

  // -------------------------------------------------------------------------
  // Discard path
  // -------------------------------------------------------------------------

  describe("discard path", () => {
    it("removeSession deletes the entire session directory", () => {
      const sessionId = "discard-sess";
      const snapshot = makeSnapshot();

      writeCrashedSession(historyDir, sessionId, snapshot);

      const sessionDir = path.join(historyDir, sessionId);
      expect(fs.existsSync(sessionDir)).toBe(true);

      const historyManager = new HistoryManager({ basePath: historyDir });
      historyManager.removeSession(sessionId);

      expect(fs.existsSync(sessionDir)).toBe(false);
    });

    it("session is no longer a cold-restore candidate after removeSession (discard)", () => {
      const sessionId = "discard-sess-2";
      const snapshot = makeSnapshot();

      writeCrashedSession(historyDir, sessionId, snapshot);

      const historyManager = new HistoryManager({ basePath: historyDir });

      // Confirm it's a cold-restore candidate before discard
      expect(historyManager.listColdRestoreSessions()).toHaveLength(1);

      historyManager.removeSession(sessionId);

      // Should no longer appear as a cold-restore candidate
      const freshHm = new HistoryManager({ basePath: historyDir });
      expect(freshHm.listColdRestoreSessions()).toHaveLength(0);
    });

    it("removeSession is idempotent — does not throw if called twice", () => {
      const sessionId = "discard-idempotent";
      const snapshot = makeSnapshot();

      writeCrashedSession(historyDir, sessionId, snapshot);

      const historyManager = new HistoryManager({ basePath: historyDir });
      historyManager.removeSession(sessionId);

      // Second call should not throw
      expect(() => historyManager.removeSession(sessionId)).not.toThrow();
    });

    it("removeSession on a non-existent session does not throw", () => {
      const historyManager = new HistoryManager({ basePath: historyDir });
      expect(() => historyManager.removeSession("nonexistent-session")).not.toThrow();
    });
  });

  // -------------------------------------------------------------------------
  // Mixed resume + discard
  // -------------------------------------------------------------------------

  describe("mixed resume and discard", () => {
    it("can resume one session and discard another independently", () => {
      const snapshotA = makeSnapshot({ snapshotAnsi: "session A" });
      const snapshotB = makeSnapshot({ snapshotAnsi: "session B" });

      writeCrashedSession(historyDir, "sess-resume", snapshotA, { cwd: "/a" });
      writeCrashedSession(historyDir, "sess-discard", snapshotB, { cwd: "/b" });

      const historyManager = new HistoryManager({ basePath: historyDir });
      expect(historyManager.listColdRestoreSessions()).toHaveLength(2);

      // Resume sess-resume
      historyManager.openSession("sess-resume", { cwd: "/a", cols: 80, rows: 24 });

      // Discard sess-discard
      historyManager.removeSession("sess-discard");

      // sess-resume: directory still exists, no checkpoint (was cleared by openSession)
      expect(fs.existsSync(path.join(historyDir, "sess-resume"))).toBe(true);
      expect(fs.existsSync(path.join(historyDir, "sess-resume", "checkpoint.json"))).toBe(false);

      // sess-discard: directory is gone
      expect(fs.existsSync(path.join(historyDir, "sess-discard"))).toBe(false);

      // Neither should appear as cold-restore candidates now
      const freshHm = new HistoryManager({ basePath: historyDir });
      expect(freshHm.listColdRestoreSessions()).toHaveLength(0);
    });
  });

  // -------------------------------------------------------------------------
  // DaemonServer.broadcastToAllClients — only reaches stream sockets
  // -------------------------------------------------------------------------

  describe("broadcastToAllClients", () => {
    it("sends cold-restore-available only to stream sockets, not control sockets", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({ ...paths, spawnSubprocess: spawner, idleTimeoutMs: 0 });
      await server.start();

      const controlSocket = await connectSocket(paths.socketPath);
      await sendHello(controlSocket, server.authToken, { clientId: "c1", role: "control" });

      const streamSocket = await connectSocket(paths.socketPath);
      await sendHello(streamSocket, server.authToken, { clientId: "c1", role: "stream" });

      // Collect events from both sockets
      const controlEvents: unknown[] = [];
      const streamEvents: DaemonEvent[] = [];

      const controlDecoder = new FrameDecoder();
      controlSocket.on("data", (chunk: Buffer) => {
        controlDecoder.push(chunk);
        for (const frame of controlDecoder.frames()) {
          controlEvents.push(frame.json);
        }
      });

      const streamDecoder = new FrameDecoder();
      streamSocket.on("data", (chunk: Buffer) => {
        streamDecoder.push(chunk);
        for (const frame of streamDecoder.frames()) {
          streamEvents.push(frame.json as DaemonEvent);
        }
      });

      server.broadcastToAllClients({
        type: "event",
        event: "cold-restore-available",
        sessionId: "test-sess",
        payload: {
          snapshot: makeSnapshot(),
          savedAt: Date.now(),
          sanitizedKeys: [],
          cwd: "/tmp",
          cols: 80,
          rows: 24,
        },
      });

      await new Promise((r) => setTimeout(r, 100));

      // Stream socket should have received the event
      const coldRestoreStreamEvents = streamEvents.filter(
        (e) => e.event === "cold-restore-available",
      );
      expect(coldRestoreStreamEvents).toHaveLength(1);

      // Control socket should NOT have received any extra frames
      // (it only gets hello response and request responses, not stream events)
      const coldRestoreControlEvents = controlEvents.filter(
        (e) => (e as Record<string, unknown>).event === "cold-restore-available",
      );
      expect(coldRestoreControlEvents).toHaveLength(0);

      controlSocket.destroy();
      streamSocket.destroy();
    });

    it("broadcasts to all connected stream clients simultaneously", async () => {
      const paths = createServerPaths(tmpDir);
      server = new DaemonServer({ ...paths, spawnSubprocess: spawner, idleTimeoutMs: 0 });
      await server.start();

      // Connect two independent clients
      const control1 = await connectSocket(paths.socketPath);
      await sendHello(control1, server.authToken, { clientId: "c1", role: "control" });
      const stream1 = await connectSocket(paths.socketPath);
      await sendHello(stream1, server.authToken, { clientId: "c1", role: "stream" });

      const control2 = await connectSocket(paths.socketPath);
      await sendHello(control2, server.authToken, { clientId: "c2", role: "control" });
      const stream2 = await connectSocket(paths.socketPath);
      await sendHello(stream2, server.authToken, { clientId: "c2", role: "stream" });

      const events1Promise = collectStreamEvents(stream1, 200);
      const events2Promise = collectStreamEvents(stream2, 200);

      server.broadcastToAllClients({
        type: "event",
        event: "cold-restore-available",
        sessionId: "shared-sess",
        payload: {
          snapshot: makeSnapshot(),
          savedAt: Date.now(),
          sanitizedKeys: [],
          cwd: "/shared",
          cols: 80,
          rows: 24,
        },
      });

      const [events1, events2] = await Promise.all([events1Promise, events2Promise]);

      const crEvents1 = events1.filter((e) => e.event === "cold-restore-available");
      const crEvents2 = events2.filter((e) => e.event === "cold-restore-available");

      expect(crEvents1).toHaveLength(1);
      expect(crEvents2).toHaveLength(1);
      expect(crEvents1[0]!.sessionId).toBe("shared-sess");
      expect(crEvents2[0]!.sessionId).toBe("shared-sess");

      control1.destroy();
      stream1.destroy();
      control2.destroy();
      stream2.destroy();
    });
  });
});
