import { socketAddress } from "../electron/daemon/socket-address";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { DaemonClient, type DaemonClientOptions } from "../electron/daemon/daemon-client";
import {
  PROTOCOL_VERSION,
  NOTIFY_PREFIX,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type DaemonRequest,
  type DaemonResponse,
  type DaemonEvent,
} from "../electron/daemon/protocol";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "daemon-client-test-"));
}

interface MockDaemonOpts {
  token?: string;
  rejectHello?: boolean;
  /** Delay before responding to requests (ms). */
  responseDelay?: number;
  /** If set, don't respond to requests at all (for timeout tests). */
  neverRespond?: boolean;
}

/**
 * Creates a mock daemon server that handles hello handshakes and
 * echoes back responses for requests.
 */
function createMockDaemon(socketPath: string, opts: MockDaemonOpts = {}) {
  const { token = "test-token", rejectHello = false, responseDelay = 0, neverRespond = false } = opts;

  const controlClients: net.Socket[] = [];
  const streamClients: net.Socket[] = [];
  const receivedRequests: DaemonRequest[] = [];

  const server = net.createServer((socket) => {
    const decoder = new FrameDecoder();
    let role: "control" | "stream" | null = null;

    socket.on("data", (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        const msg = frame.json as Record<string, unknown>;

        if (msg.type === "hello") {
          const hello = msg as unknown as HelloMessage;

          if (rejectHello || hello.token !== token || hello.version !== PROTOCOL_VERSION) {
            socket.write(encodeFrame({ type: "hello", ok: false, error: "Auth failed" }));
            return;
          }

          role = hello.role;
          if (role === "control") controlClients.push(socket);
          else streamClients.push(socket);

          socket.write(encodeFrame({ type: "hello", ok: true }));
          return;
        }

        // Handle requests on control socket
        if (role === "control") {
          const req = msg as unknown as DaemonRequest;
          receivedRequests.push(req);

          // Don't respond to notifications
          if (req.id.startsWith(NOTIFY_PREFIX)) return;

          if (neverRespond) return;

          const respond = () => {
            if (socket.destroyed) return;
            const response: DaemonResponse = {
              id: req.id,
              ok: true,
              payload: { echo: req.type },
            };
            socket.write(encodeFrame(response));
          };

          if (responseDelay > 0) {
            setTimeout(respond, responseDelay);
          } else {
            respond();
          }
        }
      }
    });
  });

  server.listen(socketAddress(socketPath));

  return {
    server,
    controlClients,
    streamClients,
    receivedRequests,
    /** Send a stream event to all connected stream clients. */
    sendStreamEvent(event: DaemonEvent, payload?: Buffer) {
      for (const client of streamClients) {
        if (!client.destroyed) {
          client.write(encodeFrame(event, payload));
        }
      }
    },
    close() {
      for (const c of [...controlClients, ...streamClients]) {
        if (!c.destroyed) c.destroy();
      }
      server.close();
    },
  };
}

async function waitForServer(server: net.Server): Promise<void> {
  return new Promise((resolve) => {
    if (server.listening) resolve();
    else server.on("listening", resolve);
  });
}

function createClientOpts(tmpDir: string, overrides: Partial<DaemonClientOptions> = {}): DaemonClientOptions {
  return {
    socketDir: tmpDir,
    daemonEntryPath: "/fake/daemon-main.js",
    onSessionData: vi.fn(),
    onSessionExit: vi.fn(),
    onAgentStateChange: vi.fn(),
    onDisconnected: vi.fn(),
    ...overrides,
  };
}

function writeTokenFile(tmpDir: string, token: string = "test-token"): void {
  fs.writeFileSync(path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.token`), token);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("DaemonClient", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = createTmpDir();
  });

  afterEach(() => {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  // -------------------------------------------------------------------------
  // ensureConnected + hello handshake
  // -------------------------------------------------------------------------

  describe("ensureConnected", () => {
    it("connects to daemon with dual sockets and hello handshake", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      expect(client.isConnected).toBe(true);
      expect(daemon.controlClients.length).toBe(1);
      expect(daemon.streamClients.length).toBe(1);

      client.disconnect();
      daemon.close();
    });

    it("reuses existing connection on repeated calls", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();
      await client.ensureConnected();

      // Should still only have one control + one stream connection
      expect(daemon.controlClients.length).toBe(1);
      expect(daemon.streamClients.length).toBe(1);

      client.disconnect();
      daemon.close();
    });

    it("coalesces concurrent connect attempts (connect-lock)", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));

      // Fire multiple concurrent ensureConnected calls
      await Promise.all([
        client.ensureConnected(),
        client.ensureConnected(),
        client.ensureConnected(),
      ]);

      // All should resolve, but only one connection should be made
      expect(daemon.controlClients.length).toBe(1);
      expect(daemon.streamClients.length).toBe(1);

      client.disconnect();
      daemon.close();
    });

    it("throws when no token file exists", async () => {
      const client = new DaemonClient(createClientOpts(tmpDir));

      await expect(client.ensureConnected()).rejects.toThrow(/No daemon token file/);
    });

    it("throws when hello is rejected", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir, "wrong-token");

      const daemon = createMockDaemon(socketPath, { token: "correct-token" });
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));

      await expect(client.ensureConnected()).rejects.toThrow(/Hello rejected/);

      daemon.close();
    });
  });

  // -------------------------------------------------------------------------
  // request<T>() — ID correlation and timeout
  // -------------------------------------------------------------------------

  describe("request", () => {
    it("correlates response to request by ID", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      const result = await client.request<{ echo: string }>("ping");
      expect(result).toEqual({ echo: "ping" });

      client.disconnect();
      daemon.close();
    });

    it("handles multiple concurrent requests with correct correlation", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath, { responseDelay: 10 });
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      // Fire multiple requests concurrently
      const [r1, r2, r3] = await Promise.all([
        client.request<{ echo: string }>("ping"),
        client.request<{ echo: string }>("listSessions"),
        client.request<{ echo: string }>("ping"),
      ]);

      expect(r1).toEqual({ echo: "ping" });
      expect(r2).toEqual({ echo: "listSessions" });
      expect(r3).toEqual({ echo: "ping" });

      client.disconnect();
      daemon.close();
    });

    it("rejects with error when daemon returns error response", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      // Custom server that returns error responses
      const server = net.createServer((socket) => {
        const decoder = new FrameDecoder();
        let authed = false;

        socket.on("data", (chunk: Buffer) => {
          decoder.push(chunk);
          const frames = decoder.frames();

          for (const frame of frames) {
            const msg = frame.json as Record<string, unknown>;

            if (msg.type === "hello") {
              socket.write(encodeFrame({ type: "hello", ok: true }));
              authed = true;
              return;
            }

            if (authed && msg.id) {
              const response: DaemonResponse = {
                id: msg.id as string,
                ok: false,
                error: "Session not found",
              };
              socket.write(encodeFrame(response));
            }
          }
        });
      });
      server.listen(socketAddress(socketPath));
      await waitForServer(server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      await expect(client.request("getSnapshot", { sessionId: "nope" }))
        .rejects.toThrow("Session not found");

      client.disconnect();
      server.close();
    });

    it("times out when no response arrives", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath, { neverRespond: true });
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      // Use a short timeout override to avoid waiting 30s in tests
      await expect(client.request("ping", undefined, 100))
        .rejects.toThrow(/timed out/);

      client.disconnect();
      daemon.close();
    });
  });

  // -------------------------------------------------------------------------
  // notify() — fire-and-forget
  // -------------------------------------------------------------------------

  describe("notify", () => {
    it("sends request with NOTIFY_PREFIX and does not wait for response", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      await client.notify("write", { sessionId: "s1", data: "hello" });

      // Give the server a tick to receive the message
      await new Promise((r) => setTimeout(r, 50));

      expect(daemon.receivedRequests.length).toBe(1);
      expect(daemon.receivedRequests[0].type).toBe("write");
      expect(daemon.receivedRequests[0].id.startsWith(NOTIFY_PREFIX)).toBe(true);

      client.disconnect();
      daemon.close();
    });
  });

  // -------------------------------------------------------------------------
  // Connection generation guard
  // -------------------------------------------------------------------------

  describe("connection generation guard", () => {
    it("ignores stale socket close events from old connections", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const onDisconnected = vi.fn();
      const client = new DaemonClient(createClientOpts(tmpDir, { onDisconnected }));
      await client.ensureConnected();

      // Grab references to old sockets on the daemon side
      const oldControlClient = daemon.controlClients[0];
      const oldStreamClient = daemon.streamClients[0];

      // Disconnect (increments generation) and reconnect
      client.disconnect();

      await client.ensureConnected();
      expect(client.isConnected).toBe(true);

      // Now destroy the old daemon-side sockets (simulates stale close event)
      // The generation guard should prevent this from tearing down the new connection
      if (oldControlClient && !oldControlClient.destroyed) {
        oldControlClient.destroy();
      }
      if (oldStreamClient && !oldStreamClient.destroyed) {
        oldStreamClient.destroy();
      }

      // Give a tick for the close event to propagate
      await new Promise((r) => setTimeout(r, 50));

      // The new connection should still be alive — generation guard protected it
      expect(client.isConnected).toBe(true);
      // onDisconnected should NOT have been called for the stale close
      expect(onDisconnected).not.toHaveBeenCalled();

      client.disconnect();
      daemon.close();
    });
  });

  // -------------------------------------------------------------------------
  // disconnect() rejects pending requests
  // -------------------------------------------------------------------------

  describe("disconnect", () => {
    it("rejects all pending requests with disconnect error", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath, { neverRespond: true });
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      // Fire requests that will never get responses
      const p1 = client.request("ping", undefined, 60_000);
      const p2 = client.request("listSessions", undefined, 60_000);

      // Give a tick for the requests to be sent
      await new Promise((r) => setTimeout(r, 10));

      // Disconnect while requests are pending
      client.disconnect();

      await expect(p1).rejects.toThrow(/disconnected/i);
      await expect(p2).rejects.toThrow(/disconnected/i);

      daemon.close();
    });

    it("sets isConnected to false", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();
      expect(client.isConnected).toBe(true);

      client.disconnect();
      expect(client.isConnected).toBe(false);

      daemon.close();
    });
  });

  // -------------------------------------------------------------------------
  // Stream event parsing and dispatch
  // -------------------------------------------------------------------------

  describe("stream events", () => {
    it("dispatches data events to onSessionData callback", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const onSessionData = vi.fn();
      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir, { onSessionData }));
      await client.ensureConnected();

      // Wait for stream socket to be fully wired
      await new Promise((r) => setTimeout(r, 20));

      // Send a data event from the daemon
      const dataPayload = Buffer.from("hello world", "utf-8");
      const event: DaemonEvent = { type: "event", event: "data", sessionId: "session-1" };
      daemon.sendStreamEvent(event, dataPayload);

      // Wait for event to propagate
      await new Promise((r) => setTimeout(r, 50));

      expect(onSessionData).toHaveBeenCalledWith("session-1", "hello world");

      client.disconnect();
      daemon.close();
    });

    it("dispatches exit events to onSessionExit callback", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const onSessionExit = vi.fn();
      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir, { onSessionExit }));
      await client.ensureConnected();

      await new Promise((r) => setTimeout(r, 20));

      const event: DaemonEvent = {
        type: "event",
        event: "exit",
        sessionId: "session-2",
        payload: { code: 42 },
      };
      daemon.sendStreamEvent(event);

      await new Promise((r) => setTimeout(r, 50));

      expect(onSessionExit).toHaveBeenCalledWith("session-2", 42);

      client.disconnect();
      daemon.close();
    });

    it("dispatches state-changed events to onAgentStateChange callback", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const onAgentStateChange = vi.fn();
      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir, { onAgentStateChange }));
      await client.ensureConnected();

      await new Promise((r) => setTimeout(r, 20));

      const event: DaemonEvent = {
        type: "event",
        event: "state-changed",
        sessionId: "agent-1",
        payload: { state: "working", previousState: "idle", timestamp: 1234567890 },
      };
      daemon.sendStreamEvent(event);

      await new Promise((r) => setTimeout(r, 50));

      expect(onAgentStateChange).toHaveBeenCalledWith("agent-1", "working", "idle");

      client.disconnect();
      daemon.close();
    });
  });

  // -------------------------------------------------------------------------
  // Session ID mapping
  // -------------------------------------------------------------------------

  describe("session ID mapping", () => {
    it("maps renderer IDs to daemon session IDs", () => {
      const client = new DaemonClient(createClientOpts(tmpDir));

      client.mapSessionId("renderer-1", "daemon-abc");
      client.mapSessionId("renderer-2", "daemon-def");

      expect(client.getDaemonSessionId("renderer-1")).toBe("daemon-abc");
      expect(client.getDaemonSessionId("renderer-2")).toBe("daemon-def");
      expect(client.getDaemonSessionId("renderer-3")).toBeUndefined();
    });

    it("removes mappings with unmapSessionId", () => {
      const client = new DaemonClient(createClientOpts(tmpDir));

      client.mapSessionId("renderer-1", "daemon-abc");
      client.unmapSessionId("renderer-1");

      expect(client.getDaemonSessionId("renderer-1")).toBeUndefined();
    });
  });

  // -------------------------------------------------------------------------
  // Daemon-initiated disconnect
  // -------------------------------------------------------------------------

  describe("daemon-initiated disconnect", () => {
    it("calls onDisconnected when daemon closes the connection", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const onDisconnected = vi.fn();
      const daemon = createMockDaemon(socketPath);
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir, { onDisconnected }));
      await client.ensureConnected();

      // Wait for sockets to be fully wired
      await new Promise((r) => setTimeout(r, 20));

      // Daemon closes all client sockets (simulates daemon crash)
      for (const c of daemon.controlClients) {
        if (!c.destroyed) c.destroy();
      }
      for (const c of daemon.streamClients) {
        if (!c.destroyed) c.destroy();
      }

      await new Promise((r) => setTimeout(r, 100));

      expect(onDisconnected).toHaveBeenCalled();
      expect(client.isConnected).toBe(false);

      daemon.close();
    });

    it("rejects pending requests when daemon disconnects", async () => {
      const socketPath = path.join(tmpDir, `daemon-v${PROTOCOL_VERSION}.sock`);
      writeTokenFile(tmpDir);

      const daemon = createMockDaemon(socketPath, { neverRespond: true });
      await waitForServer(daemon.server);

      const client = new DaemonClient(createClientOpts(tmpDir));
      await client.ensureConnected();

      const p1 = client.request("ping", undefined, 60_000);

      // Give a tick for the request to be sent
      await new Promise((r) => setTimeout(r, 20));

      // Daemon crashes
      for (const c of daemon.controlClients) {
        if (!c.destroyed) c.destroy();
      }
      for (const c of daemon.streamClients) {
        if (!c.destroyed) c.destroy();
      }

      await expect(p1).rejects.toThrow(/connection lost/i);

      daemon.close();
    });
  });
});
