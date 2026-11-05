import { socketAddress } from "../electron/daemon/socket-address";
/**
 * Integration test for daemon-main-style wiring.
 *
 * Boots a real DaemonServer with a mock spawner and wires the durability
 * subsystems (HistoryManager, CheckpointTimer, SessionActivityBitmap,
 * ResourceQuotaManager, AuditLogger) exactly as daemon-main.ts does.
 * Drives a full session lifecycle and asserts that:
 *   - HistoryManager.checkpoint is called within the active interval
 *   - AuditLogger.sessionCreated and sessionExited fire
 *   - ResourceQuotaManager.checkCanCreateSession is called and refuses past the cap
 *   - meta.json and checkpoint.json exist on disk after a session lifecycle
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import * as net from "node:net";

import { DaemonServer } from "../electron/daemon/daemon-server";
import { HistoryManager } from "../electron/daemon/history-manager";
import { CheckpointTimer } from "../electron/daemon/checkpoint-timer";
import { SessionActivityBitmap } from "../electron/daemon/session-activity-bitmap";
import { ResourceQuotaManager, QuotaExceededError } from "../electron/daemon/resource-quotas";
import { AuditLogger } from "../electron/daemon/audit-logger";
import {
  PROTOCOL_VERSION,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
} from "../electron/daemon/protocol";
import type { SubprocessHandle, StreamClient } from "../electron/daemon/types";
import type { SubprocessSpawner, SubprocessSpawnOptions } from "../electron/daemon/subprocess-handle";

// ---------------------------------------------------------------------------
// Mock subprocess handle
// ---------------------------------------------------------------------------

class MockSubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private dataCallbacks: Array<(data: string) => void> = [];
  private exitCallbacks: Array<(exitCode: number) => void> = [];
  private _disposed = false;

  constructor(pid = 9000) {
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
    for (const cb of [...this.dataCallbacks]) cb(data);
  }

  emitExit(code: number): void {
    for (const cb of [...this.exitCallbacks]) cb(code);
  }
}

// ---------------------------------------------------------------------------
// Mock spawner factory
// ---------------------------------------------------------------------------

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
// Socket helpers (mirrors daemon-server.spec.ts pattern)
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-wiring-test-"));
}

function serverPaths(dir: string) {
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
    clientId: opts.clientId ?? "wiring-test-client",
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

// ---------------------------------------------------------------------------
// Wiring helper — mirrors daemon-main.ts construction
// ---------------------------------------------------------------------------

interface WiredDaemon {
  server: DaemonServer;
  historyManager: HistoryManager;
  checkpointTimer: CheckpointTimer;
  activityTracker: SessionActivityBitmap;
  quotaManager: ResourceQuotaManager;
  auditLogger: AuditLogger;
  spawned: MockSubprocessHandle[];
  historyDir: string;
  auditLogPath: string;
}

function buildWiredDaemon(
  tmpDir: string,
  opts: {
    maxConcurrentSessions?: number;
    activeIntervalMs?: number;
    idleIntervalMs?: number;
  } = {},
): WiredDaemon {
  const historyDir = path.join(tmpDir, "terminal-history");
  const auditLogPath = path.join(tmpDir, "audit.log");
  const quotasPath = path.join(tmpDir, "quotas.json");

  // Write a quotas.json with a low cap so we can test refusal
  if (opts.maxConcurrentSessions !== undefined) {
    fs.mkdirSync(path.dirname(quotasPath), { recursive: true });
    fs.writeFileSync(
      quotasPath,
      JSON.stringify({ maxConcurrentSessions: opts.maxConcurrentSessions }),
    );
  }

  const historyManager = new HistoryManager({ basePath: historyDir });
  const activityTracker = new SessionActivityBitmap();
  const auditLogger = new AuditLogger({ filePath: auditLogPath });

  const { spawner, spawned } = createMockSpawner();

  // Quota manager — reads from quotasPath (or uses defaults if file absent)
  const quotaManager = new ResourceQuotaManager(
    () => server.terminalHost.getQuotaSessions(),
    {
      configPath: quotasPath,
      readFile: (p) => fs.readFileSync(p, "utf-8"),
      fileExists: (p) => fs.existsSync(p),
    },
  );

  const paths = serverPaths(tmpDir);

  const server = new DaemonServer({
    ...paths,
    spawnSubprocess: spawner,
    hooks: {
      beforeSpawn: () => {
        quotaManager.checkCanCreateSession(server.terminalHost.sessionCount);
      },
      onSessionCreated: (id, sessionOpts) => {
        historyManager.openSession(id, {
          cwd: sessionOpts.cwd ?? os.homedir(),
          cols: sessionOpts.cols,
          rows: sessionOpts.rows,
          env: sessionOpts.env,
        });
        auditLogger.sessionCreated(id, { agent: !!sessionOpts.agentMeta });
        activityTracker.register(id);
      },
      onSessionAttached: (id) => {
        auditLogger.sessionAttached(id);
      },
      onSessionData: (id, _byteLength) => {
        activityTracker.markDirty(id);
        historyManager.markDirty(id);
      },
      onSessionExit: (id, code) => {
        historyManager.closeSession(id, code);
        auditLogger.sessionExited(id, { code });
        activityTracker.remove(id);
        checkpointTimer.removeSession(id);
      },
      onSessionKilled: (id) => {
        auditLogger.sessionKilled(id);
      },
    },
  });

  const checkpointTimer = new CheckpointTimer({
    historyManager,
    snapshotProvider: {
      getSnapshot: (id) => server.terminalHost.getSnapshot(id),
    },
    activityTracker,
    activeIntervalMs: opts.activeIntervalMs ?? 5_000,
    idleIntervalMs: opts.idleIntervalMs ?? 60_000,
  });

  quotaManager.start();
  checkpointTimer.start();

  return {
    server,
    historyManager,
    checkpointTimer,
    activityTracker,
    quotaManager,
    auditLogger,
    spawned,
    historyDir,
    auditLogPath,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("daemon-init wiring integration", () => {
  let tmpDir: string;
  let wired: WiredDaemon;

  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    tmpDir = createTmpDir();
  });

  afterEach(async () => {
    vi.useRealTimers();
    wired?.checkpointTimer.stop();
    wired?.quotaManager.dispose();
    wired?.auditLogger.dispose();
    if (wired?.server) {
      await wired.server.shutdown({ killSessions: true });
    }
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // best-effort
    }
  });

  // -------------------------------------------------------------------------
  // AuditLogger fires on session lifecycle
  // -------------------------------------------------------------------------

  describe("AuditLogger wiring", () => {
    it("writes sessionCreated entry when a new session is spawned", async () => {
      wired = buildWiredDaemon(tmpDir);
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      const hello = await sendHello(socket, wired.server.authToken);
      expect(hello.ok).toBe(true);

      const resp = await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-audit-1", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      expect(resp.ok).toBe(true);

      socket.destroy();

      // Audit log must exist and contain a session.created entry
      expect(fs.existsSync(wired.auditLogPath)).toBe(true);
      const lines = fs.readFileSync(wired.auditLogPath, "utf-8").trim().split("\n");
      const events = lines.map((l) => JSON.parse(l) as { event: string; sessionId: string });
      const created = events.find((e) => e.event === "session.created" && e.sessionId === "sess-audit-1");
      expect(created).toBeDefined();
    });

    it("writes sessionExited entry when the subprocess exits", async () => {
      wired = buildWiredDaemon(tmpDir);
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-audit-2", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      // Simulate subprocess exit
      const handle = wired.spawned[0]!;
      handle.emitExit(0);

      // Give the event loop a tick to propagate
      await new Promise((r) => setImmediate(r));

      const lines = fs.readFileSync(wired.auditLogPath, "utf-8").trim().split("\n");
      const events = lines.map((l) => JSON.parse(l) as { event: string; sessionId: string });
      const exited = events.find((e) => e.event === "session.exited" && e.sessionId === "sess-audit-2");
      expect(exited).toBeDefined();
    });
  });

  // -------------------------------------------------------------------------
  // HistoryManager — meta.json and checkpoint.json on disk
  // -------------------------------------------------------------------------

  describe("HistoryManager wiring", () => {
    it("creates meta.json in the history dir when a session is opened", async () => {
      wired = buildWiredDaemon(tmpDir);
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-hist-1", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      const metaPath = path.join(wired.historyDir, "sess-hist-1", "meta.json");
      expect(fs.existsSync(metaPath)).toBe(true);

      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.sessionId).toBe("sess-hist-1");
      expect(meta.cols).toBe(80);
      expect(meta.rows).toBe(24);
      expect(meta.createdAt).toBeDefined();
      expect(meta.endedAt).toBeUndefined();
    });

    it("writes checkpoint.json after data arrives and the active interval elapses", async () => {
      wired = buildWiredDaemon(tmpDir, { activeIntervalMs: 50, idleIntervalMs: 5_000 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-hist-2", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      // Simulate data arriving from the subprocess — marks the session dirty
      const handle = wired.spawned[0]!;
      handle.emitData("$ hello\r\n");

      // Advance time past the active interval so the checkpoint timer fires
      vi.advanceTimersByTime(200);
      await new Promise((r) => setImmediate(r));

      const checkpointPath = path.join(wired.historyDir, "sess-hist-2", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);

      const checkpoint = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(checkpoint.snapshot).toBeDefined();
      expect(checkpoint.checkpointedAt).toBeDefined();
    });

    it("updates meta.json with endedAt after session exits", async () => {
      wired = buildWiredDaemon(tmpDir);
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-hist-3", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      const handle = wired.spawned[0]!;
      handle.emitExit(0);
      await new Promise((r) => setImmediate(r));

      const metaPath = path.join(wired.historyDir, "sess-hist-3", "meta.json");
      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.endedAt).toBeDefined();
      expect(meta.exitCode).toBe(0);
    });

    it("both meta.json and checkpoint.json exist after a full lifecycle with data", async () => {
      wired = buildWiredDaemon(tmpDir, { activeIntervalMs: 50, idleIntervalMs: 5_000 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-full-lifecycle", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      const handle = wired.spawned[0]!;
      handle.emitData("$ echo done\r\n");

      vi.advanceTimersByTime(200);
      await new Promise((r) => setImmediate(r));

      handle.emitExit(0);
      await new Promise((r) => setImmediate(r));

      const sessionDir = path.join(wired.historyDir, "sess-full-lifecycle");
      expect(fs.existsSync(path.join(sessionDir, "meta.json"))).toBe(true);
      expect(fs.existsSync(path.join(sessionDir, "checkpoint.json"))).toBe(true);
    });
  });

  // -------------------------------------------------------------------------
  // ResourceQuotaManager — checkCanCreateSession called and refuses past cap
  // -------------------------------------------------------------------------

  describe("ResourceQuotaManager wiring", () => {
    it("allows session creation when under the cap", async () => {
      wired = buildWiredDaemon(tmpDir, { maxConcurrentSessions: 2 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);

      const resp = await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-quota-1", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      expect(resp.ok).toBe(true);

      socket.destroy();
    });

    it("refuses session creation when the concurrent session cap is reached", async () => {
      // Cap at 1 session
      wired = buildWiredDaemon(tmpDir, { maxConcurrentSessions: 1 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);

      // First session — should succeed
      const resp1 = await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-quota-cap-1", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      expect(resp1.ok).toBe(true);

      // Second session — should be refused by quota
      const resp2 = await sendRequest(socket, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-quota-cap-2", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      expect(resp2.ok).toBe(false);
      expect(resp2.error).toMatch(/quota-exceeded/);

      socket.destroy();
    });

    it("quota error message starts with quota-exceeded: prefix", async () => {
      wired = buildWiredDaemon(tmpDir, { maxConcurrentSessions: 1 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);

      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-prefix-1", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);

      const resp = await sendRequest(socket, {
        id: "req-2",
        type: "createOrAttach",
        payload: { sessionId: "sess-prefix-2", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);

      expect(resp.ok).toBe(false);
      expect(resp.error).toMatch(/^quota-exceeded:/);

      socket.destroy();
    });
  });

  // -------------------------------------------------------------------------
  // CheckpointTimer — checkpoint called within active interval
  // -------------------------------------------------------------------------

  describe("CheckpointTimer wiring", () => {
    it("does not write checkpoint.json when no data has arrived", async () => {
      wired = buildWiredDaemon(tmpDir, { activeIntervalMs: 50, idleIntervalMs: 5_000 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-no-data", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      // Advance past active interval — no data, so no checkpoint
      vi.advanceTimersByTime(200);
      await new Promise((r) => setImmediate(r));

      const checkpointPath = path.join(wired.historyDir, "sess-no-data", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);
    });

    it("writes checkpoint.json within the active interval after data arrives", async () => {
      wired = buildWiredDaemon(tmpDir, { activeIntervalMs: 50, idleIntervalMs: 5_000 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-with-data", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      const handle = wired.spawned[0]!;
      handle.emitData("output line\r\n");

      // Advance past the active interval
      vi.advanceTimersByTime(200);
      await new Promise((r) => setImmediate(r));

      const checkpointPath = path.join(wired.historyDir, "sess-with-data", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);
    });

    it("finalCheckpoint writes checkpoint.json for all active sessions on shutdown", async () => {
      wired = buildWiredDaemon(tmpDir, { activeIntervalMs: 60_000, idleIntervalMs: 300_000 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);
      await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-final-cp", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      socket.destroy();

      const handle = wired.spawned[0]!;
      handle.emitData("some output\r\n");
      await new Promise((r) => setImmediate(r));

      // Simulate graceful shutdown: finalCheckpoint before server.shutdown
      wired.checkpointTimer.finalCheckpoint();

      const checkpointPath = path.join(wired.historyDir, "sess-final-cp", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);
    });
  });

  // -------------------------------------------------------------------------
  // Full lifecycle: all subsystems together
  // -------------------------------------------------------------------------

  describe("full session lifecycle", () => {
    it("all subsystems fire correctly across create → data → exit", async () => {
      wired = buildWiredDaemon(tmpDir, { activeIntervalMs: 50, idleIntervalMs: 5_000 });
      await wired.server.start();

      const socket = await connectSocket(serverPaths(tmpDir).socketPath);
      await sendHello(socket, wired.server.authToken);

      // Create session
      const createResp = await sendRequest(socket, {
        id: "req-1",
        type: "createOrAttach",
        payload: { sessionId: "sess-lifecycle", cols: 80, rows: 24, cwd: tmpDir },
      } as DaemonRequest);
      expect(createResp.ok).toBe(true);
      socket.destroy();

      // meta.json must exist immediately after creation
      const metaPath = path.join(wired.historyDir, "sess-lifecycle", "meta.json");
      expect(fs.existsSync(metaPath)).toBe(true);

      // Audit log must have session.created
      const auditLines1 = fs.readFileSync(wired.auditLogPath, "utf-8").trim().split("\n");
      const auditEvents1 = auditLines1.map((l) => JSON.parse(l) as { event: string });
      expect(auditEvents1.some((e) => e.event === "session.created")).toBe(true);

      // Simulate data → marks dirty → checkpoint timer fires
      const handle = wired.spawned[0]!;
      handle.emitData("$ ls\r\n");
      vi.advanceTimersByTime(200);
      await new Promise((r) => setImmediate(r));

      const checkpointPath = path.join(wired.historyDir, "sess-lifecycle", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);

      // Simulate exit
      handle.emitExit(0);
      await new Promise((r) => setImmediate(r));

      // meta.json must have endedAt
      const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
      expect(meta.endedAt).toBeDefined();

      // Audit log must have session.exited
      const auditLines2 = fs.readFileSync(wired.auditLogPath, "utf-8").trim().split("\n");
      const auditEvents2 = auditLines2.map((l) => JSON.parse(l) as { event: string });
      expect(auditEvents2.some((e) => e.event === "session.exited")).toBe(true);
    });
  });
});
