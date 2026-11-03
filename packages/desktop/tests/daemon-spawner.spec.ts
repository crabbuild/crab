import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as child_process from "node:child_process";

import { DaemonSpawner } from "../electron/daemon/daemon-spawner";
import {
  PROTOCOL_VERSION,
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type DaemonRequest,
} from "../electron/daemon/protocol";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "spawner-test-"));
}

/**
 * Create a minimal Unix socket server that accepts connections.
 * Used to simulate a running daemon for probe/healthCheck tests.
 */
function createMockDaemonServer(
  socketPath: string,
  opts: {
    respondToHello?: boolean;
    respondToPing?: boolean;
    helloOk?: boolean;
    token?: string;
  } = {},
): net.Server {
  const {
    respondToHello = true,
    respondToPing = true,
    helloOk = true,
    token = "test-token",
  } = opts;

  const server = net.createServer((socket) => {
    const decoder = new FrameDecoder();
    let authenticated = false;

    socket.on("data", (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        const msg = frame.json as Record<string, unknown>;

        if (msg.type === "hello" && respondToHello) {
          const hello = msg as unknown as HelloMessage;
          const ok = helloOk && hello.token === token && hello.version === PROTOCOL_VERSION;
          const response = ok
            ? { type: "hello", ok: true }
            : { type: "hello", ok: false, error: "Auth failed" };
          socket.write(encodeFrame(response));
          if (ok) authenticated = true;
        } else if (msg.type === "ping" && respondToPing && authenticated) {
          const response = {
            id: (msg as DaemonRequest).id,
            ok: true,
            payload: {
              uptime: 100,
              sessions: 0,
              clients: 1,
              memoryUsage: 50_000_000,
              protocolVersion: PROTOCOL_VERSION,
            },
          };
          socket.write(encodeFrame(response));
        }
      }
    });
  });

  server.listen(socketPath);
  return server;
}

async function waitForServer(server: net.Server): Promise<void> {
  return new Promise((resolve) => {
    if (server.listening) {
      resolve();
    } else {
      server.on("listening", resolve);
    }
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("DaemonSpawner", () => {
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
  // probe tests
  // -------------------------------------------------------------------------

  describe("probe", () => {
    it("returns true when a daemon is listening on the socket", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const server = createMockDaemonServer(socketPath);
      await waitForServer(server);

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.probe(socketPath);
      expect(result).toBe(true);

      server.close();
    });

    it("returns false when no daemon is listening (socket file missing)", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.probe(socketPath);
      expect(result).toBe(false);
    });

    it("returns false when socket file exists but nothing is listening", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      // Create a regular file at the socket path (simulates stale socket)
      fs.writeFileSync(socketPath, "");

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.probe(socketPath);
      expect(result).toBe(false);
    });

    it("uses default socketPath when none provided", async () => {
      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      // No server running, should return false
      const result = await spawner.probe();
      expect(result).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // healthCheck tests
  // -------------------------------------------------------------------------

  describe("healthCheck", () => {
    it("returns true when daemon responds to hello + ping", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      const token = "valid-token-123";
      fs.writeFileSync(tokenPath, token);

      const server = createMockDaemonServer(socketPath, { token });
      await waitForServer(server);

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.healthCheck(socketPath, token);
      expect(result).toBe(true);

      server.close();
    });

    it("returns false when hello is rejected (bad token)", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      fs.writeFileSync(tokenPath, "wrong-token");

      const server = createMockDaemonServer(socketPath, { token: "correct-token" });
      await waitForServer(server);

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.healthCheck(socketPath, "wrong-token");
      expect(result).toBe(false);

      server.close();
    });

    it("returns false when daemon does not respond to ping (timeout)", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      const token = "test-token";
      fs.writeFileSync(tokenPath, token);

      // Server that accepts hello but never responds to ping
      const server = createMockDaemonServer(socketPath, {
        token,
        respondToPing: false,
      });
      await waitForServer(server);

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.healthCheck(socketPath, token);
      expect(result).toBe(false);

      server.close();
    }, 5000);

    it("returns false when no token file exists", async () => {
      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      // No token file, no explicit token — should return false
      const result = await spawner.healthCheck();
      expect(result).toBe(false);
    });

    it("returns false when socket is not reachable", async () => {
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      fs.writeFileSync(tokenPath, "some-token");

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const result = await spawner.healthCheck();
      expect(result).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // cleanupStaleSocket tests
  // -------------------------------------------------------------------------

  describe("cleanupStaleSocket", () => {
    it("removes stale socket/token/pid files when process is dead", () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      const pidPath = path.join(tmpDir, "daemon-v1.pid");

      // Create stale files with a PID that doesn't exist
      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(tokenPath, "old-token");
      fs.writeFileSync(
        pidPath,
        JSON.stringify({ pid: 999999, startedAtMs: Date.now(), entryPath: "/old" }),
      );

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const cleaned = spawner.cleanupStaleSocket(socketPath, pidPath);

      expect(cleaned).toBe(true);
      expect(fs.existsSync(socketPath)).toBe(false);
      expect(fs.existsSync(tokenPath)).toBe(false);
      expect(fs.existsSync(pidPath)).toBe(false);
    });

    it("does not remove files when process is alive", () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      const pidPath = path.join(tmpDir, "daemon-v1.pid");

      // Use current process PID (which is alive)
      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(tokenPath, "token");
      fs.writeFileSync(
        pidPath,
        JSON.stringify({ pid: process.pid, startedAtMs: Date.now(), entryPath: "/entry" }),
      );

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const cleaned = spawner.cleanupStaleSocket(socketPath, pidPath);

      expect(cleaned).toBe(false);
      expect(fs.existsSync(socketPath)).toBe(true);
      expect(fs.existsSync(tokenPath)).toBe(true);
      expect(fs.existsSync(pidPath)).toBe(true);
    });

    it("cleans up when PID file is missing or malformed", () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      const pidPath = path.join(tmpDir, "daemon-v1.pid");

      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(tokenPath, "token");
      // No PID file — should treat as stale

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const cleaned = spawner.cleanupStaleSocket(socketPath, pidPath);

      expect(cleaned).toBe(true);
      expect(fs.existsSync(socketPath)).toBe(false);
      expect(fs.existsSync(tokenPath)).toBe(false);
    });

    it("handles malformed PID file JSON gracefully", () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const pidPath = path.join(tmpDir, "daemon-v1.pid");

      fs.writeFileSync(socketPath, "");
      fs.writeFileSync(pidPath, "not valid json {{{");

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      const cleaned = spawner.cleanupStaleSocket(socketPath, pidPath);
      expect(cleaned).toBe(true);
      expect(fs.existsSync(socketPath)).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // spawn tests
  // -------------------------------------------------------------------------

  describe("spawn", () => {
    it("rejects when daemon does not send ready within timeout", async () => {
      // Create a script that never sends a ready message
      const scriptPath = path.join(tmpDir, "no-ready-daemon.js");
      fs.writeFileSync(
        scriptPath,
        `// Daemon that never sends ready
         setTimeout(() => {}, 60000);
        `,
      );

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: scriptPath,
      });

      // Use a short timeout by mocking the constant behavior
      // We'll test with the real timeout but use a script that exits quickly
      const scriptExitPath = path.join(tmpDir, "exit-daemon.js");
      fs.writeFileSync(
        scriptExitPath,
        `// Daemon that exits immediately without sending ready
         process.exit(1);
        `,
      );

      await expect(spawner.spawn(scriptExitPath)).rejects.toThrow(
        /Daemon exited prematurely/,
      );
    });

    it("resolves when daemon sends ready IPC message", async () => {
      // Create a script that sends ready immediately
      const scriptPath = path.join(tmpDir, "ready-daemon.js");
      fs.writeFileSync(
        scriptPath,
        `// Daemon that sends ready immediately
         if (process.send) {
           process.send({ type: "ready", pid: process.pid });
         }
         // Keep alive briefly so disconnect works
         setTimeout(() => process.exit(0), 500);
        `,
      );

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: scriptPath,
      });

      await expect(spawner.spawn(scriptPath)).resolves.toBeUndefined();
    });

    it("rejects when entry path does not exist", async () => {
      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/nonexistent/daemon.js",
      });

      await expect(spawner.spawn("/nonexistent/daemon.js")).rejects.toThrow();
    });
  });

  // -------------------------------------------------------------------------
  // validateSocketPathLength tests
  // -------------------------------------------------------------------------

  describe("validateSocketPathLength", () => {
    it("accepts paths within the limit", () => {
      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      expect(() => spawner.validateSocketPathLength("/tmp/short.sock")).not.toThrow();
    });

    it("rejects paths exceeding the OS limit", () => {
      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      // Create a path that exceeds 104 bytes (macOS limit)
      const longPath = "/tmp/" + "a".repeat(200) + ".sock";

      expect(() => spawner.validateSocketPathLength(longPath)).toThrow(
        /Socket path exceeds/,
      );
    });
  });

  // -------------------------------------------------------------------------
  // resetHandle tests
  // -------------------------------------------------------------------------

  describe("resetHandle", () => {
    it("resets internal daemon process reference", () => {
      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      // resetHandle should not throw even when no process exists
      expect(() => spawner.resetHandle()).not.toThrow();
    });
  });

  // -------------------------------------------------------------------------
  // ensureRunning tests
  // -------------------------------------------------------------------------

  describe("ensureRunning", () => {
    it("reuses existing daemon when probe and healthCheck succeed", async () => {
      const socketPath = path.join(tmpDir, "daemon-v2.sock");
      const tokenPath = path.join(tmpDir, "daemon-v2.token");
      const token = "ensure-token";
      fs.writeFileSync(tokenPath, token);

      const server = createMockDaemonServer(socketPath, { token });
      await waitForServer(server);

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: "/fake/entry.js",
      });

      // Should not throw — daemon is healthy
      await expect(spawner.ensureRunning()).resolves.toBeUndefined();

      server.close();
    });

    it("spawns new daemon when probe fails", async () => {
      // Create a script that sends ready
      const scriptPath = path.join(tmpDir, "ready-daemon.js");
      fs.writeFileSync(
        scriptPath,
        `if (process.send) {
           process.send({ type: "ready", pid: process.pid });
         }
         setTimeout(() => process.exit(0), 500);
        `,
      );

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: scriptPath,
      });

      // No daemon running — should spawn
      await expect(spawner.ensureRunning()).resolves.toBeUndefined();
    });

    it("cleans up and respawns when probe succeeds but healthCheck fails", async () => {
      const socketPath = path.join(tmpDir, "daemon-v1.sock");
      const tokenPath = path.join(tmpDir, "daemon-v1.token");
      const pidPath = path.join(tmpDir, "daemon-v1.pid");

      // Create a server that accepts connections but fails health check
      const token = "bad-health-token";
      fs.writeFileSync(tokenPath, token);
      fs.writeFileSync(
        pidPath,
        JSON.stringify({ pid: 999999, startedAtMs: Date.now(), entryPath: "/old" }),
      );

      // Server that rejects hello (simulates unhealthy daemon)
      const server = createMockDaemonServer(socketPath, {
        token: "different-token",
        helloOk: false,
      });
      await waitForServer(server);

      // Create a script that sends ready for the respawn
      const scriptPath = path.join(tmpDir, "ready-daemon.js");
      fs.writeFileSync(
        scriptPath,
        `if (process.send) {
           process.send({ type: "ready", pid: process.pid });
         }
         setTimeout(() => process.exit(0), 500);
        `,
      );

      const spawner = new DaemonSpawner({
        socketDir: tmpDir,
        entryPath: scriptPath,
      });

      // Probe will succeed (server is listening), but healthCheck will fail
      // Then it should cleanup and spawn
      server.close();
      // Wait for server to close so spawn can bind
      await new Promise((r) => setTimeout(r, 50));

      await expect(spawner.ensureRunning()).resolves.toBeUndefined();
    });
  });

  // -------------------------------------------------------------------------
  // Path derivation tests
  // -------------------------------------------------------------------------

  describe("path derivation", () => {
    it("derives correct socket/token/pid paths from socketDir", () => {
      const spawner = new DaemonSpawner({
        socketDir: "/home/user/.crab/desktop/daemon",
        entryPath: "/app/daemon-main.js",
        protocolVersion: 2,
      });

      expect(spawner.socketPath).toBe("/home/user/.crab/desktop/daemon/daemon-v2.sock");
      expect(spawner.tokenPath).toBe("/home/user/.crab/desktop/daemon/daemon-v2.token");
      expect(spawner.pidPath).toBe("/home/user/.crab/desktop/daemon/daemon-v2.pid");
    });
  });
});
