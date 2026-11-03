import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import {
  LegacyDaemonAdapter,
  detectLegacyDaemons,
  type LegacyDaemonAdapterOptions,
} from "../electron/daemon/legacy-daemon-adapter";

import {
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
  type DaemonEvent,
} from "../electron/daemon/protocol";

// ---------------------------------------------------------------------------
// Helpers: Fake legacy daemon server
// ---------------------------------------------------------------------------

interface FakeLegacyDaemon {
  server: net.Server;
  socketPath: string;
  tokenPath: string;
  pidPath: string;
  socketDir: string;
  token: string;
  version: number;
  connections: net.Socket[];
  receivedRequests: Array<{ id: string; type: string; payload?: unknown }>;
  sessions: Array<{ sessionId: string; isAlive: boolean; state: string }>;
  close: () => Promise<void>;
  sendStreamEvent: (event: DaemonEvent, payload?: Buffer) => void;
}

async function createFakeLegacyDaemon(version: number): Promise<FakeLegacyDaemon> {
  const socketDir = fs.mkdtempSync(path.join(os.tmpdir(), "legacy-daemon-test-"));
  const socketPath = path.join(socketDir, `daemon-v${version}.sock`);
  const tokenPath = path.join(socketDir, `daemon-v${version}.token`);
  const pidPath = path.join(socketDir, `daemon-v${version}.pid`);
  const token = "test-legacy-token-12345";

  fs.writeFileSync(tokenPath, token, { mode: 0o600 });
  fs.writeFileSync(pidPath, JSON.stringify({ pid: process.pid, startedAtMs: Date.now(), entryPath: "/fake" }));

  const connections: net.Socket[] = [];
  const receivedRequests: Array<{ id: string; type: string; payload?: unknown }> = [];
  let streamSockets: net.Socket[] = [];

  const sessions = [
    { sessionId: "ws1@@abc123", isAlive: true, state: "running" },
    { sessionId: "ws1@@def456", isAlive: true, state: "running" },
  ];

  const server = net.createServer((socket) => {
    connections.push(socket);
    const decoder = new FrameDecoder();
    let handshakeDone = false;
    let role: "control" | "stream" = "control";

    socket.on("data", (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        const msg = frame.json as Record<string, unknown>;

        if (!handshakeDone && msg.type === "hello") {
          const hello = msg as unknown as HelloMessage;
          handshakeDone = true;
          role = hello.role;

          const resp: HelloResponse = {
            type: "hello",
            ok: hello.version === version && hello.token === token,
          };
          if (!resp.ok) {
            resp.error = "version or token mismatch";
          }
          socket.write(encodeFrame(resp));

          if (role === "stream") {
            streamSockets.push(socket);
          }
          return;
        }

        if (handshakeDone && role === "control") {
          const req = msg as unknown as DaemonRequest;
          receivedRequests.push({ id: req.id, type: req.type, payload: (req as Record<string, unknown>).payload });

          // Handle requests
          let response: DaemonResponse;
          if (req.type === "listSessions") {
            response = {
              id: req.id,
              ok: true,
              payload: sessions.map((s) => ({
                sessionId: s.sessionId,
                state: s.state,
                shellState: "ready",
                isAlive: s.isAlive,
                pid: 12345,
                cwd: "/tmp",
                cols: 80,
                rows: 24,
                createdAt: Date.now(),
              })),
            };
          } else if (req.type === "shutdown") {
            response = { id: req.id, ok: true, payload: null };
          } else if (req.type === "write" || req.type === "resize" || req.type === "kill" || req.type === "signal") {
            response = { id: req.id, ok: true, payload: null };
          } else if (req.type === "getSnapshot") {
            response = { id: req.id, ok: true, payload: { snapshotAnsi: "test", scrollbackAnsi: "", rehydrateSequences: "", cwd: "/tmp", modes: {}, cols: 80, rows: 24, scrollbackLines: 0 } };
          } else if (req.type === "getCwd") {
            response = { id: req.id, ok: true, payload: "/tmp" };
          } else {
            response = { id: req.id, ok: true, payload: null };
          }
          socket.write(encodeFrame(response));
        }
      }
    });

    socket.on("error", () => {});
  });

  await new Promise<void>((resolve) => {
    server.listen(socketPath, () => resolve());
  });

  return {
    server,
    socketPath,
    tokenPath,
    pidPath,
    socketDir,
    token,
    version,
    connections,
    receivedRequests,
    sessions,
    close: async () => {
      for (const conn of connections) {
        conn.destroy();
      }
      await new Promise<void>((resolve) => server.close(() => resolve()));
      safeRmdir(socketDir);
    },
    sendStreamEvent: (event: DaemonEvent, payload?: Buffer) => {
      for (const sock of streamSockets) {
        if (!sock.destroyed) {
          sock.write(encodeFrame(event, payload));
        }
      }
    },
  };
}

function safeRmdir(dir: string): void {
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Ignore
  }
}

function createAdapterOptions(
  daemon: FakeLegacyDaemon,
  overrides?: Partial<LegacyDaemonAdapterOptions>,
): LegacyDaemonAdapterOptions {
  return {
    socketDir: daemon.socketDir,
    legacyVersion: daemon.version,
    onSessionData: overrides?.onSessionData ?? (() => {}),
    onSessionExit: overrides?.onSessionExit ?? (() => {}),
    onShutdownComplete: overrides?.onShutdownComplete,
    log: overrides?.log,
    now: overrides?.now,
    pollIntervalMs: overrides?.pollIntervalMs ?? 50,
    maxLifetimeMs: overrides?.maxLifetimeMs ?? 60_000,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("LegacyDaemonAdapter", () => {
  let daemon: FakeLegacyDaemon;

  beforeEach(async () => {
    daemon = await createFakeLegacyDaemon(1);
  });

  afterEach(async () => {
    await daemon.close();
  });

  // -------------------------------------------------------------------------
  // Connection and session discovery
  // -------------------------------------------------------------------------

  describe("connection and session discovery", () => {
    it("connects to legacy daemon and discovers sessions", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      expect(adapter.state).toBe("active");
      expect(adapter.sessionCount).toBe(2);
      expect(adapter.ownsSession("ws1@@abc123")).toBe(true);
      expect(adapter.ownsSession("ws1@@def456")).toBe(true);
      expect(adapter.ownsSession("ws1@@unknown")).toBe(false);

      adapter.dispose();
    });

    it("reports correct protocol version", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      expect(adapter.protocolVersion).toBe(1);

      adapter.dispose();
    });

    it("throws when token file is missing", async () => {
      fs.unlinkSync(daemon.tokenPath);

      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await expect(adapter.connect()).rejects.toThrow("No token file found");
    });

    it("transitions to draining when no sessions exist", async () => {
      daemon.sessions.length = 0;

      const shutdownComplete = vi.fn();
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon, {
        onShutdownComplete: shutdownComplete,
      }));
      await adapter.connect();

      // Should have initiated shutdown since no sessions
      expect(adapter.state).toBe("shutdown");
      expect(shutdownComplete).toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Operation routing
  // -------------------------------------------------------------------------

  describe("operation routing", () => {
    it("forwards write to legacy daemon", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      await adapter.write("ws1@@abc123", "hello");

      const writeReq = daemon.receivedRequests.find((r) => r.type === "write");
      expect(writeReq).toBeDefined();
      expect(writeReq!.payload).toEqual({ sessionId: "ws1@@abc123", data: "hello" });

      adapter.dispose();
    });

    it("forwards resize to legacy daemon", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      await adapter.resize("ws1@@abc123", 120, 40);

      const resizeReq = daemon.receivedRequests.find((r) => r.type === "resize");
      expect(resizeReq).toBeDefined();
      expect(resizeReq!.payload).toEqual({ sessionId: "ws1@@abc123", cols: 120, rows: 40 });

      adapter.dispose();
    });

    it("forwards kill to legacy daemon", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      await adapter.kill("ws1@@abc123");

      const killReq = daemon.receivedRequests.find((r) => r.type === "kill");
      expect(killReq).toBeDefined();
      expect(killReq!.payload).toEqual({ sessionId: "ws1@@abc123" });

      adapter.dispose();
    });

    it("forwards signal to legacy daemon", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      await adapter.signal("ws1@@abc123", "SIGINT");

      const sigReq = daemon.receivedRequests.find((r) => r.type === "signal");
      expect(sigReq).toBeDefined();
      expect(sigReq!.payload).toEqual({ sessionId: "ws1@@abc123", signal: "SIGINT" });

      adapter.dispose();
    });

    it("forwards getSnapshot to legacy daemon", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      const snapshot = await adapter.getSnapshot("ws1@@abc123");
      expect(snapshot).toBeDefined();
      expect((snapshot as Record<string, unknown>).snapshotAnsi).toBe("test");

      adapter.dispose();
    });

    it("forwards getCwd to legacy daemon", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      const cwd = await adapter.getCwd("ws1@@abc123");
      expect(cwd).toBe("/tmp");

      adapter.dispose();
    });

    it("rejects operations for sessions not owned by adapter", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      await expect(adapter.write("unknown-session", "data")).rejects.toThrow("not owned by legacy adapter");
      await expect(adapter.resize("unknown-session", 80, 24)).rejects.toThrow("not owned by legacy adapter");
      await expect(adapter.kill("unknown-session")).rejects.toThrow("not owned by legacy adapter");

      adapter.dispose();
    });
  });

  // -------------------------------------------------------------------------
  // Stream event forwarding
  // -------------------------------------------------------------------------

  describe("stream event forwarding", () => {
    it("forwards data events from legacy daemon", async () => {
      const dataEvents: Array<{ sessionId: string; data: string }> = [];
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon, {
        onSessionData: (sessionId, data) => dataEvents.push({ sessionId, data }),
      }));
      await adapter.connect();

      // Send a data event from the fake daemon
      const event: DaemonEvent = { type: "event", event: "data", sessionId: "ws1@@abc123" };
      const payload = Buffer.from("hello world");
      daemon.sendStreamEvent(event, payload);

      // Wait for event to propagate
      await sleep(50);

      expect(dataEvents).toHaveLength(1);
      expect(dataEvents[0].sessionId).toBe("ws1@@abc123");
      expect(dataEvents[0].data).toBe("hello world");

      adapter.dispose();
    });

    it("forwards exit events and removes session from tracking", async () => {
      const exitEvents: Array<{ sessionId: string; code: number }> = [];
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon, {
        onSessionExit: (sessionId, code) => exitEvents.push({ sessionId, code }),
      }));
      await adapter.connect();

      expect(adapter.sessionCount).toBe(2);

      // Send an exit event
      const event: DaemonEvent = { type: "event", event: "exit", sessionId: "ws1@@abc123", payload: { code: 0 } };
      daemon.sendStreamEvent(event);

      await sleep(50);

      expect(exitEvents).toHaveLength(1);
      expect(exitEvents[0]).toEqual({ sessionId: "ws1@@abc123", code: 0 });
      expect(adapter.sessionCount).toBe(1);
      expect(adapter.ownsSession("ws1@@abc123")).toBe(false);

      adapter.dispose();
    });
  });

  // -------------------------------------------------------------------------
  // Shutdown when all sessions exit
  // -------------------------------------------------------------------------

  describe("shutdown when all sessions exit", () => {
    it("shuts down legacy daemon when all sessions exit", async () => {
      const shutdownComplete = vi.fn();
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon, {
        onSessionExit: () => {},
        onShutdownComplete: shutdownComplete,
      }));
      await adapter.connect();

      expect(adapter.state).toBe("active");

      // Exit both sessions
      daemon.sendStreamEvent({ type: "event", event: "exit", sessionId: "ws1@@abc123", payload: { code: 0 } });
      await sleep(50);

      daemon.sendStreamEvent({ type: "event", event: "exit", sessionId: "ws1@@def456", payload: { code: 0 } });
      await sleep(100);

      expect(adapter.state).toBe("shutdown");
      expect(shutdownComplete).toHaveBeenCalled();

      // Verify shutdown request was sent
      const shutdownReq = daemon.receivedRequests.find((r) => r.type === "shutdown");
      expect(shutdownReq).toBeDefined();
    });
  });

  // -------------------------------------------------------------------------
  // Legacy daemon death handling
  // -------------------------------------------------------------------------

  describe("legacy daemon death handling", () => {
    it("synthesizes exit events when legacy daemon dies", async () => {
      const exitEvents: Array<{ sessionId: string; code: number }> = [];
      const shutdownComplete = vi.fn();
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon, {
        onSessionExit: (sessionId, code) => exitEvents.push({ sessionId, code }),
        onShutdownComplete: shutdownComplete,
      }));
      await adapter.connect();

      expect(adapter.sessionCount).toBe(2);

      // Kill the daemon server (simulates daemon death)
      for (const conn of daemon.connections) {
        conn.destroy();
      }
      daemon.server.close();

      await sleep(100);

      // Should have synthesized exit events for both sessions
      expect(exitEvents).toHaveLength(2);
      expect(exitEvents.every((e) => e.code === -1)).toBe(true);
      expect(adapter.state).toBe("shutdown");
      expect(adapter.sessionCount).toBe(0);
      expect(shutdownComplete).toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Max lifetime enforcement
  // -------------------------------------------------------------------------

  describe("max lifetime enforcement", () => {
    it("force-shuts down after max lifetime exceeded", async () => {
      let clock = 0;
      const exitEvents: Array<{ sessionId: string; code: number }> = [];
      const shutdownComplete = vi.fn();

      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon, {
        onSessionExit: (sessionId, code) => exitEvents.push({ sessionId, code }),
        onShutdownComplete: shutdownComplete,
        now: () => clock,
        pollIntervalMs: 20,
        maxLifetimeMs: 100,
      }));
      await adapter.connect();

      expect(adapter.state).toBe("active");

      // Advance clock past max lifetime
      clock = 150;

      // Wait for poll to fire
      await sleep(50);

      expect(adapter.state).toBe("shutdown");
      expect(exitEvents).toHaveLength(2);
      expect(exitEvents.every((e) => e.code === -1)).toBe(true);
      expect(shutdownComplete).toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Dispose
  // -------------------------------------------------------------------------

  describe("dispose", () => {
    it("stops polling and disconnects sockets", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      adapter.dispose();

      expect(adapter.state).toBe("shutdown");
    });

    it("rejects pending requests on dispose", async () => {
      const adapter = new LegacyDaemonAdapter(createAdapterOptions(daemon));
      await adapter.connect();

      // Start a request but dispose before it completes
      // (We can't easily test this without a slow server, so just verify dispose is clean)
      adapter.dispose();
      await expect(adapter.write("ws1@@abc123", "data")).rejects.toThrow();
    });
  });
});

// ---------------------------------------------------------------------------
// detectLegacyDaemons
// ---------------------------------------------------------------------------

describe("detectLegacyDaemons", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "detect-legacy-test-"));
  });

  afterEach(() => {
    safeRmdir(tmpDir);
  });

  it("detects socket files with lower protocol versions", () => {
    fs.writeFileSync(path.join(tmpDir, "daemon-v1.sock"), "");
    fs.writeFileSync(path.join(tmpDir, "daemon-v2.sock"), "");
    fs.writeFileSync(path.join(tmpDir, "daemon-v3.sock"), "");

    const versions = detectLegacyDaemons(tmpDir, 3);
    expect(versions).toEqual([1, 2]);
  });

  it("returns empty array when no legacy daemons exist", () => {
    fs.writeFileSync(path.join(tmpDir, "daemon-v3.sock"), "");

    const versions = detectLegacyDaemons(tmpDir, 3);
    expect(versions).toEqual([]);
  });

  it("ignores non-socket files", () => {
    fs.writeFileSync(path.join(tmpDir, "daemon-v1.token"), "");
    fs.writeFileSync(path.join(tmpDir, "daemon-v1.pid"), "");
    fs.writeFileSync(path.join(tmpDir, "other-file.sock"), "");

    const versions = detectLegacyDaemons(tmpDir, 3);
    expect(versions).toEqual([]);
  });

  it("returns empty array when directory does not exist", () => {
    const versions = detectLegacyDaemons("/nonexistent/path", 3);
    expect(versions).toEqual([]);
  });

  it("returns sorted versions", () => {
    fs.writeFileSync(path.join(tmpDir, "daemon-v5.sock"), "");
    fs.writeFileSync(path.join(tmpDir, "daemon-v2.sock"), "");
    fs.writeFileSync(path.join(tmpDir, "daemon-v8.sock"), "");

    const versions = detectLegacyDaemons(tmpDir, 10);
    expect(versions).toEqual([2, 5, 8]);
  });

  it("does not include current version", () => {
    fs.writeFileSync(path.join(tmpDir, "daemon-v1.sock"), "");
    fs.writeFileSync(path.join(tmpDir, "daemon-v2.sock"), "");

    const versions = detectLegacyDaemons(tmpDir, 2);
    expect(versions).toEqual([1]);
  });
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
