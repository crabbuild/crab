/**
 * Auto-respawn integration test.
 *
 * Boots a real DaemonServer on a temp socket, connects a real DaemonClient,
 * drops the daemon mid-test, and verifies:
 *   1. Synthesized crab:term-exit events with reason "daemon-disconnected"
 *      are emitted for every tracked session.
 *   2. AutoRespawn.handleDisconnect() runs and a new DaemonServer comes up.
 *   3. The next client.request("ping") succeeds after reconnect.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { DaemonServer } from "../electron/daemon/daemon-server";
import { DaemonClient } from "../electron/daemon/daemon-client";
import { AutoRespawn } from "../electron/daemon/auto-respawn";
import { PROTOCOL_VERSION } from "../electron/daemon/protocol";
import type { SubprocessHandle, StreamClient } from "../electron/daemon/types";
import type { SubprocessSpawner, SubprocessSpawnOptions } from "../electron/daemon/subprocess-handle";
import type { PingResponse } from "../electron/daemon/protocol";

// ---------------------------------------------------------------------------
// Minimal mock subprocess handle — never emits data, never exits on its own
// ---------------------------------------------------------------------------

class StubSubprocessHandle implements SubprocessHandle {
  readonly pid: number;
  readonly fd: number;
  private exitCbs: Array<(code: number) => void> = [];
  private dataCbs: Array<(data: string) => void> = [];
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

  onData(cb: (data: string) => void): void { this.dataCbs.push(cb); }
  onExit(cb: (exitCode: number) => void): void { this.exitCbs.push(cb); }

  dispose(): void {
    this._disposed = true;
    this.exitCbs = [];
    this.dataCbs = [];
  }
}

function createStubSpawner(): SubprocessSpawner {
  let nextPid = 9000;
  return (_opts: SubprocessSpawnOptions) => new StubSubprocessHandle(nextPid++);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "respawn-integ-"));
}

function serverPaths(dir: string) {
  return {
    socketPath: path.join(dir, `daemon-v${PROTOCOL_VERSION}.sock`),
    tokenPath: path.join(dir, `daemon-v${PROTOCOL_VERSION}.token`),
    pidPath: path.join(dir, `daemon-v${PROTOCOL_VERSION}.pid`),
  };
}

async function startServer(dir: string): Promise<DaemonServer> {
  const paths = serverPaths(dir);
  const server = new DaemonServer({
    ...paths,
    spawnSubprocess: createStubSpawner(),
    idleTimeoutMs: 0,
  });
  await server.start();
  return server;
}

function buildClient(
  dir: string,
  callbacks: {
    onSessionData?: (id: string, data: string) => void;
    onSessionExit?: (id: string, code: number) => void;
    onDisconnected?: () => void;
  } = {},
): DaemonClient {
  return new DaemonClient({
    socketDir: dir,
    daemonEntryPath: "/fake/daemon-main.js",
    onSessionData: callbacks.onSessionData ?? (() => {}),
    onSessionExit: callbacks.onSessionExit ?? (() => {}),
    onAgentStateChange: () => {},
    onDisconnected: callbacks.onDisconnected,
  });
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("auto-respawn integration", () => {
  let tmpDir: string;
  let server: DaemonServer | null;

  beforeEach(() => {
    tmpDir = createTmpDir();
    server = null;
  });

  afterEach(async () => {
    if (server) {
      try {
        await server.shutdown({ killSessions: true });
      } catch {
        // already shut down
      }
      server = null;
    }
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
  });

  // -------------------------------------------------------------------------
  // 1. Synthesized crab:term-exit on disconnect
  // -------------------------------------------------------------------------

  it("synthesizes exit events with reason daemon-disconnected for tracked sessions", async () => {
    server = await startServer(tmpDir);

    const exitEvents: Array<{ sessionId: string; code: number; reason: string }> = [];
    const trackedSessions = ["sess-alpha", "sess-beta"];

    // Build the synthesize function that mirrors what daemon-bridge does:
    // it emits a crab:term-exit-like event with reason "daemon-disconnected".
    function synthesizeExitEvent(sessionId: string, code: number): void {
      exitEvents.push({ sessionId, code, reason: "daemon-disconnected" });
    }

    const client = buildClient(tmpDir);
    await client.ensureConnected();
    expect(client.isConnected).toBe(true);

    // Spawn a session so the server has something alive
    await client.request("createOrAttach", {
      sessionId: trackedSessions[0],
      cols: 80,
      rows: 24,
      cwd: os.tmpdir(),
    });
    await client.request("createOrAttach", {
      sessionId: trackedSessions[1],
      cols: 80,
      rows: 24,
      cwd: os.tmpdir(),
    });

    // Build AutoRespawn with a mock spawner that will start a fresh server
    let respawnedServer: DaemonServer | null = null;
    const respawn = new AutoRespawn(
      {
        spawner: {
          resetHandle: () => {},
          ensureRunning: async () => {
            // Spin up a replacement server in the same tmpDir
            respawnedServer = await startServer(tmpDir);
          },
        } as unknown as import("../electron/daemon/daemon-spawner").DaemonSpawner,
        getActiveSessionIds: () => [...trackedSessions],
        synthesizeExitEvent,
        reconnect: () => client.ensureConnected(),
      },
      { maxAttempts: 3, retryDelayMs: 50 },
    );

    // Drop the daemon
    await server.shutdown({ killSessions: true });
    server = null;

    // Wait for the client to detect the disconnect
    await delay(100);

    // Trigger respawn (mirrors what init.ts does in client.onDisconnected)
    const respawnResult = await respawn.handleDisconnect();

    // Verify exit events were synthesized for both tracked sessions
    expect(exitEvents).toHaveLength(2);
    for (const evt of exitEvents) {
      expect(evt.reason).toBe("daemon-disconnected");
      expect(evt.code).toBe(-1);
      expect(trackedSessions).toContain(evt.sessionId);
    }

    // Verify respawn succeeded
    expect(respawnResult).toBe(true);
    expect(respawn.state).toBe("idle");
    expect(respawn.totalRespawns).toBe(1);

    // Clean up the replacement server
    if (respawnedServer) {
      await (respawnedServer as DaemonServer).shutdown({ killSessions: true });
    }
    client.disconnect();
  });

  // -------------------------------------------------------------------------
  // 2. Next ping succeeds after respawn
  // -------------------------------------------------------------------------

  it("next client.request(ping) succeeds after daemon drop and respawn", async () => {
    server = await startServer(tmpDir);

    const client = buildClient(tmpDir);
    await client.ensureConnected();

    // Confirm initial ping works
    const firstPing = await client.request<PingResponse>("ping");
    expect(firstPing.protocolVersion).toBe(PROTOCOL_VERSION);

    let respawnedServer: DaemonServer | null = null;

    const respawn = new AutoRespawn(
      {
        spawner: {
          resetHandle: () => {},
          ensureRunning: async () => {
            respawnedServer = await startServer(tmpDir);
          },
        } as unknown as import("../electron/daemon/daemon-spawner").DaemonSpawner,
        getActiveSessionIds: () => [],
        synthesizeExitEvent: () => {},
        reconnect: () => client.ensureConnected(),
      },
      { maxAttempts: 3, retryDelayMs: 50 },
    );

    // Drop the daemon
    await server.shutdown({ killSessions: true });
    server = null;

    // Wait for disconnect to propagate
    await delay(100);

    // Respawn
    const ok = await respawn.handleDisconnect();
    expect(ok).toBe(true);

    // The client should now be reconnected to the new server
    expect(client.isConnected).toBe(true);

    // Ping the new server
    const secondPing = await client.request<PingResponse>("ping");
    expect(secondPing.protocolVersion).toBe(PROTOCOL_VERSION);
    expect(secondPing.sessions).toBe(0);

    if (respawnedServer) {
      await (respawnedServer as DaemonServer).shutdown({ killSessions: true });
    }
    client.disconnect();
  });

  // -------------------------------------------------------------------------
  // 3. AutoRespawn state transitions during the full lifecycle
  // -------------------------------------------------------------------------

  it("transitions idle → respawning → idle across a disconnect/reconnect cycle", async () => {
    server = await startServer(tmpDir);

    const client = buildClient(tmpDir);
    await client.ensureConnected();

    let respawnedServer: DaemonServer | null = null;
    let stateAtEnsureRunning: string | null = null;

    const respawn = new AutoRespawn(
      {
        spawner: {
          resetHandle: () => {},
          ensureRunning: async () => {
            stateAtEnsureRunning = respawn.state;
            respawnedServer = await startServer(tmpDir);
          },
        } as unknown as import("../electron/daemon/daemon-spawner").DaemonSpawner,
        getActiveSessionIds: () => [],
        synthesizeExitEvent: () => {},
        reconnect: () => client.ensureConnected(),
      },
      { maxAttempts: 3, retryDelayMs: 50 },
    );

    expect(respawn.state).toBe("idle");

    await server.shutdown({ killSessions: true });
    server = null;
    await delay(100);

    await respawn.handleDisconnect();

    // During ensureRunning the state must have been "respawning"
    expect(stateAtEnsureRunning).toBe("respawning");
    // After success it returns to "idle"
    expect(respawn.state).toBe("idle");

    if (respawnedServer) {
      await (respawnedServer as DaemonServer).shutdown({ killSessions: true });
    }
    client.disconnect();
  });

  // -------------------------------------------------------------------------
  // 4. Respawn fails gracefully when the new server cannot start
  // -------------------------------------------------------------------------

  it("transitions to failed state when respawn cannot start a new server", async () => {
    server = await startServer(tmpDir);

    const client = buildClient(tmpDir);
    await client.ensureConnected();

    const respawn = new AutoRespawn(
      {
        spawner: {
          resetHandle: () => {},
          ensureRunning: async () => {
            throw new Error("cannot start daemon");
          },
        } as unknown as import("../electron/daemon/daemon-spawner").DaemonSpawner,
        getActiveSessionIds: () => ["orphan-session"],
        synthesizeExitEvent: () => {},
        reconnect: () => client.ensureConnected(),
      },
      { maxAttempts: 2, retryDelayMs: 20 },
    );

    await server.shutdown({ killSessions: true });
    server = null;
    await delay(100);

    const ok = await respawn.handleDisconnect();

    expect(ok).toBe(false);
    expect(respawn.state).toBe("failed");
    expect(respawn.totalRespawns).toBe(0);

    client.disconnect();
  });

  // -------------------------------------------------------------------------
  // 5. Concurrent disconnect events are coalesced into a single respawn
  // -------------------------------------------------------------------------

  it("coalesces concurrent disconnect events into a single respawn cycle", async () => {
    server = await startServer(tmpDir);

    const client = buildClient(tmpDir);
    await client.ensureConnected();

    let ensureRunningCallCount = 0;
    let respawnedServer: DaemonServer | null = null;

    const respawn = new AutoRespawn(
      {
        spawner: {
          resetHandle: () => {},
          ensureRunning: async () => {
            ensureRunningCallCount++;
            respawnedServer = await startServer(tmpDir);
          },
        } as unknown as import("../electron/daemon/daemon-spawner").DaemonSpawner,
        getActiveSessionIds: () => [],
        synthesizeExitEvent: () => {},
        reconnect: () => client.ensureConnected(),
      },
      { maxAttempts: 3, retryDelayMs: 50 },
    );

    await server.shutdown({ killSessions: true });
    server = null;
    await delay(100);

    // Fire three concurrent disconnect handlers
    const [r1, r2, r3] = await Promise.all([
      respawn.handleDisconnect(),
      respawn.handleDisconnect(),
      respawn.handleDisconnect(),
    ]);

    expect(r1).toBe(true);
    expect(r2).toBe(true);
    expect(r3).toBe(true);

    // Only one actual spawn should have occurred
    expect(ensureRunningCallCount).toBe(1);
    expect(respawn.totalRespawns).toBe(1);

    if (respawnedServer) {
      await (respawnedServer as DaemonServer).shutdown({ killSessions: true });
    }
    client.disconnect();
  });
});
