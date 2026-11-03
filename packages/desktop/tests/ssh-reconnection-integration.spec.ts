/**
 * Integration test: SSH reconnection flow end-to-end.
 *
 * Simulates a network interruption (SSH connection drops) and verifies
 * the full reconnection lifecycle across multiple subsystem modules:
 *
 * 1. Connected state with active terminals and pending RPC calls
 * 2. Network interruption → system enters reconnection state
 * 3. Read-only operations return stale cached data during reconnection
 * 4. Write operations are rejected immediately
 * 5. Non-cacheable read operations are queued
 * 6. Reconnection success → queued calls are replayed
 * 7. Terminal sessions are probed and either resumed or marked as ended
 * 8. Audit log records the reconnection event
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ClientChannel } from "ssh2";

import { RpcRouter } from "../electron/ssh-router";
import { RemoteCache } from "../electron/ssh-cache";
import { RemoteTerminalManager } from "../electron/ssh-terminal";
import { SshAuditLog, bridgeAuditLog } from "../electron/ssh-audit-log";

// ── Mock SSH channel ─────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  ended = false;

  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }

  end(): void {
    this.ended = true;
    this.emit("close");
  }

  setWindow(_rows: number, _cols: number, _height: number, _width: number): void {
    // no-op for tests
  }

  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

// ── Mock SSH Client ──────────────────────────────────────────────

class MockSshClient extends EventEmitter {
  shellCalls: Array<{ opts: any; callback: Function }> = [];
  private shellChannels: MockChannel[] = [];

  /**
   * Pre-register channels that will be returned by shell() calls.
   */
  addShellChannel(channel: MockChannel): void {
    this.shellChannels.push(channel);
  }

  shell(opts: any, callback: (err: Error | null, stream: ClientChannel) => void): void {
    this.shellCalls.push({ opts, callback });
    const ch = this.shellChannels.shift();
    if (ch) {
      callback(null, ch.asChannel());
    } else {
      // Default: create a new channel
      const newCh = new MockChannel();
      callback(null, newCh.asChannel());
    }
  }

  exec(cmd: string, callback: (err: Error | null, stream: ClientChannel) => void): void {
    const ch = new MockChannel();
    callback(null, ch.asChannel());
    // Simulate immediate response for health checks
    ch.emit("data", Buffer.from("1\n"));
    ch.emit("close");
  }

  end(): void {
    this.emit("end");
  }
}

// ── Mock SSH Session Manager ─────────────────────────────────────

class MockSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();
  private connections = new Map<string, any>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
    this.emit("ssh:status-changed", { profileId, status });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  setConnection(profileId: string, conn: any): void {
    this.connections.set(profileId, conn);
  }

  getConnection(profileId: string): any {
    return this.connections.get(profileId);
  }
}

// ── Mock Local Agent ─────────────────────────────────────────────

class MockLocalAgent {
  async request(_method: string, _params: unknown): Promise<unknown> {
    return { ok: true };
  }
}

// ── Test Suite ───────────────────────────────────────────────────

describe("SSH Reconnection Integration", () => {
  const PROFILE_ID = "gpu-server";
  const REPO_ID = `ssh://${PROFILE_ID}/home/user/ml-project`;

  let sessionManager: MockSessionManager;
  let localAgent: MockLocalAgent;
  let router: RpcRouter;
  let cache: RemoteCache;
  let terminalManager: RemoteTerminalManager;
  let auditLog: SshAuditLog;
  let auditLogPath: string;
  let tempDir: string;

  beforeEach(() => {
    // Set up temp directory for audit log
    tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "ssh-reconnect-test-"));
    auditLogPath = path.join(tempDir, "ssh-audit.log");

    // Initialize all subsystem modules
    sessionManager = new MockSessionManager();
    localAgent = new MockLocalAgent();
    router = new RpcRouter(sessionManager as any, localAgent);
    cache = new RemoteCache();
    router.setCache(cache);
    auditLog = new SshAuditLog(auditLogPath);
    terminalManager = new RemoteTerminalManager(sessionManager as any);

    // Wire audit log to session manager events
    bridgeAuditLog(auditLog, sessionManager);
  });

  afterEach(() => {
    try {
      fs.rmSync(tempDir, { recursive: true, force: true });
    } catch {
      // Best-effort cleanup
    }
  });

  describe("full reconnection lifecycle", () => {
    it("end-to-end: connected → interruption → reconnection → recovery", async () => {
      // ─── Phase 1: Establish connected state ────────────────────

      const agentChannel = new MockChannel();
      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: new MockSshClient(),
        channels: { agent: agentChannel.asChannel() },
        profile: { operationTimeoutMs: 5000 },
      });
      router.attachChannel(PROFILE_ID, agentChannel.asChannel());

      // Populate cache with data (simulating prior successful fetches)
      await cache.getOrFetch(`${REPO_ID}:repo_summary`, async () => ({
        name: "ml-project",
        branch: "main",
        dirty: 2,
      }), 30_000);

      await cache.getOrFetch(`${REPO_ID}:git_status_porcelain`, async () => (
        "M train.py\nM config.yaml"
      ), 30_000);

      await cache.getOrFetch(`${REPO_ID}:list_refs`, async () => ({
        branches: ["main", "feature/training", "fix/data-loader"],
      }), 30_000);

      // ─── Phase 2: Simulate network interruption ────────────────

      sessionManager.setStatus(PROFILE_ID, "reconnecting");

      // Verify audit log records the reconnection event
      const entriesAfterReconnecting = auditLog.readEntries();
      const reconnectionEntry = entriesAfterReconnecting.find(
        (e) => e.event === "reconnection" && e.profileId === PROFILE_ID,
      );
      expect(reconnectionEntry).toBeDefined();

      // ─── Phase 3: Verify stale cached data for read-only ops ───

      const summaryResult = await router.route(
        REPO_ID, "repo_summary", { repo_id: REPO_ID },
      ) as any;
      expect(summaryResult.__stale).toBe(true);
      expect(summaryResult.name).toBe("ml-project");
      expect(summaryResult.branch).toBe("main");

      const statusResult = await router.route(
        REPO_ID, "git_status_porcelain", { repo_id: REPO_ID },
      ) as any;
      expect(statusResult.__stale).toBe(true);
      expect(statusResult.data).toBe("M train.py\nM config.yaml");

      const refsResult = await router.route(
        REPO_ID, "list_refs", { repo_id: REPO_ID },
      ) as any;
      expect(refsResult.__stale).toBe(true);
      expect(refsResult.branches).toEqual(["main", "feature/training", "fix/data-loader"]);

      // ─── Phase 4: Verify write operations are rejected ─────────

      await expect(
        router.route(REPO_ID, "git_commit", { repo_id: REPO_ID, message: "wip" }),
      ).rejects.toThrow("Connection lost");

      await expect(
        router.route(REPO_ID, "git_push", { repo_id: REPO_ID }),
      ).rejects.toThrow("Connection lost");

      await expect(
        router.route(REPO_ID, "hydrate", { repo_id: REPO_ID, paths: ["model.bin"] }),
      ).rejects.toThrow("Connection lost");

      // Verify the error code is SSH_RECONNECTING
      try {
        await router.route(REPO_ID, "git_stage", { repo_id: REPO_ID, paths: ["file.py"] });
      } catch (err: any) {
        expect(err.code).toBe("SSH_RECONNECTING");
      }

      // ─── Phase 5: Verify non-cacheable reads are queued ────────

      // git_stash_list is not cacheable, so it gets queued
      const stashPromise = router.route(
        REPO_ID, "git_stash_list", { repo_id: REPO_ID },
      );

      // A second non-cacheable call also gets queued
      const blamePromise = router.route(
        REPO_ID, "git_blame", { repo_id: REPO_ID, path: "train.py" },
      );

      // ─── Phase 6: Simulate reconnection success ────────────────

      const newAgentChannel = new MockChannel();

      // Set up auto-responder: when the router writes a request to the
      // channel, parse the ID and respond with mock data.
      const originalWrite = newAgentChannel.write.bind(newAgentChannel);
      newAgentChannel.write = (data: string, cb?: (err?: Error | null) => void): boolean => {
        originalWrite(data, cb);
        // Parse the request to extract the ID and method
        try {
          const request = JSON.parse(data.trim());
          const id = request.id;
          const method = request.method;

          let result: unknown;
          if (method === "git_stash_list") {
            result = [{ index: 0, message: "WIP on main" }];
          } else if (method === "git_blame") {
            result = { lines: [{ author: "user", line: 1 }] };
          } else {
            result = { ok: true };
          }

          // Respond asynchronously (simulates network round-trip)
          setImmediate(() => {
            const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
            newAgentChannel.emit("data", Buffer.from(response));
          });
        } catch {
          // Not valid JSON — ignore
        }
        return true;
      };

      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: new MockSshClient(),
        channels: { agent: newAgentChannel.asChannel() },
        profile: { operationTimeoutMs: 5000 },
      });
      router.attachChannel(PROFILE_ID, newAgentChannel.asChannel());

      // Replay the queue — sends queued calls to the new channel
      await router.replayQueue(PROFILE_ID);

      // Verify queued calls resolved with correct data
      const stashResult = await stashPromise;
      expect(stashResult).toEqual([{ index: 0, message: "WIP on main" }]);

      const blameResult = await blamePromise;
      expect(blameResult).toEqual({ lines: [{ author: "user", line: 1 }] });

      // ─── Phase 7: Verify audit log records connect event ───────

      const finalEntries = auditLog.readEntries();
      const connectEntry = finalEntries.find(
        (e) => e.event === "connect" && e.profileId === PROFILE_ID,
      );
      expect(connectEntry).toBeDefined();

      // Verify the full sequence: reconnection → connect
      const profileEvents = finalEntries
        .filter((e) => e.profileId === PROFILE_ID)
        .map((e) => e.event);
      expect(profileEvents).toContain("reconnection");
      expect(profileEvents).toContain("connect");
    });
  });

  describe("terminal recovery after reconnection", () => {
    it("probes terminal sessions and marks dead ones as ended", async () => {
      // Set up a connected state with a terminal
      const terminalChannel = new MockChannel();
      const mockClient = new MockSshClient();
      mockClient.addShellChannel(terminalChannel);

      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: mockClient,
        channels: { agent: new MockChannel().asChannel() },
      });

      // Open a terminal session
      const terminalId = await terminalManager.openTerminal(PROFILE_ID, 80, 24, "/home/user");

      // Verify terminal is alive
      const session = terminalManager.getSession(terminalId);
      expect(session).toBeDefined();
      expect(session!.alive).toBe(true);

      // Simulate network interruption — channel closes
      terminalChannel.emit("close");

      // The channel close handler should mark the session as ended
      const updatedSession = terminalManager.getSession(terminalId);
      expect(updatedSession!.alive).toBe(false);
      expect(updatedSession!.awaitingReconnect).toBe(true);
    });

    it("emits terminal-session-ended when probe fails after reconnection", async () => {
      // Set up a terminal with a channel that will fail the probe
      const terminalChannel = new MockChannel();
      const mockClient = new MockSshClient();
      mockClient.addShellChannel(terminalChannel);

      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: mockClient,
        channels: { agent: new MockChannel().asChannel() },
      });

      const terminalId = await terminalManager.openTerminal(PROFILE_ID, 120, 40);

      // Collect events
      const endedEvents: any[] = [];
      terminalManager.on("ssh:terminal-session-ended", (e) => endedEvents.push(e));

      // Simulate the channel dying (no response to probe)
      // First, null out the channel to simulate it being dead
      const session = terminalManager.getSession(terminalId);
      session!.channel = null;

      // Trigger reconnection handling
      await terminalManager.handleReconnection(PROFILE_ID);

      // Verify the session is marked as ended
      expect(session!.alive).toBe(false);
      expect(session!.awaitingReconnect).toBe(true);
      expect(endedEvents).toHaveLength(1);
      expect(endedEvents[0].terminalId).toBe(terminalId);
      expect(endedEvents[0].profileId).toBe(PROFILE_ID);
    });

    it("resumes terminal when probe succeeds after reconnection", async () => {
      // Set up a terminal with a channel that will respond to probe
      const terminalChannel = new MockChannel();
      const mockClient = new MockSshClient();
      mockClient.addShellChannel(terminalChannel);

      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: mockClient,
        channels: { agent: new MockChannel().asChannel() },
      });

      const terminalId = await terminalManager.openTerminal(PROFILE_ID, 80, 24);

      // Collect events
      const resumedEvents: any[] = [];
      terminalManager.on("ssh:terminal-resumed", (e) => resumedEvents.push(e));

      // Simulate the probe succeeding: when the probe writes \x00,
      // the channel responds with data
      const session = terminalManager.getSession(terminalId);
      const channel = session!.channel!;

      // Override write to simulate the shell responding
      const originalWrite = channel.write.bind(channel);
      (channel as any).write = (data: any, ...args: any[]) => {
        originalWrite(data, ...args);
        // Simulate shell responding to the probe
        if (data === "\x00") {
          setTimeout(() => channel.emit("data", Buffer.from("")), 10);
        }
      };

      // Trigger reconnection handling
      await terminalManager.handleReconnection(PROFILE_ID);

      // Verify the session is resumed
      expect(resumedEvents).toHaveLength(1);
      expect(resumedEvents[0].terminalId).toBe(terminalId);
      expect(resumedEvents[0].profileId).toBe(PROFILE_ID);
    });

    it("handles multiple terminals with mixed probe results", async () => {
      const aliveChannel = new MockChannel();
      const deadChannel = new MockChannel();
      const mockClient = new MockSshClient();
      mockClient.addShellChannel(aliveChannel);
      mockClient.addShellChannel(deadChannel);

      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: mockClient,
        channels: { agent: new MockChannel().asChannel() },
      });

      const aliveTerminalId = await terminalManager.openTerminal(PROFILE_ID, 80, 24);
      const deadTerminalId = await terminalManager.openTerminal(PROFILE_ID, 80, 24);

      const resumedEvents: any[] = [];
      const endedEvents: any[] = [];
      terminalManager.on("ssh:terminal-resumed", (e) => resumedEvents.push(e));
      terminalManager.on("ssh:terminal-session-ended", (e) => endedEvents.push(e));

      // Make the alive channel respond to probe
      const aliveSession = terminalManager.getSession(aliveTerminalId);
      const aliveCh = aliveSession!.channel!;
      const origWrite = aliveCh.write.bind(aliveCh);
      (aliveCh as any).write = (data: any, ...args: any[]) => {
        origWrite(data, ...args);
        if (data === "\x00") {
          setTimeout(() => aliveCh.emit("data", Buffer.from("ok")), 10);
        }
      };

      // Make the dead channel not respond (null it out)
      const deadSession = terminalManager.getSession(deadTerminalId);
      deadSession!.channel = null;

      await terminalManager.handleReconnection(PROFILE_ID);

      // Alive terminal should be resumed
      expect(resumedEvents).toHaveLength(1);
      expect(resumedEvents[0].terminalId).toBe(aliveTerminalId);

      // Dead terminal should be marked as ended
      expect(endedEvents).toHaveLength(1);
      expect(endedEvents[0].terminalId).toBe(deadTerminalId);
    });
  });

  describe("queue overflow during extended reconnection", () => {
    it("drops oldest calls when queue exceeds max capacity", async () => {
      sessionManager.setStatus(PROFILE_ID, "reconnecting");

      // Queue 50 calls (the max)
      const promises: Promise<unknown>[] = [];
      for (let i = 0; i < 50; i++) {
        promises.push(
          router.route(REPO_ID, "git_stash_list", { repo_id: REPO_ID, idx: i }),
        );
      }

      // Queue one more — should drop the oldest
      const overflowPromise = router.route(
        REPO_ID, "git_stash_list", { repo_id: REPO_ID, idx: 50 },
      );

      // The first promise should be rejected due to overflow
      await expect(promises[0]).rejects.toThrow("reconnection queue overflow");

      // Clean up: simulate reconnection to resolve remaining promises
      const newChannel = new MockChannel();
      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: new MockSshClient(),
        channels: { agent: newChannel.asChannel() },
        profile: { operationTimeoutMs: 5000 },
      });
      router.attachChannel(PROFILE_ID, newChannel.asChannel());

      // Clear the queue by rejecting all (simpler than replaying 50 calls)
      router.clearQueue(PROFILE_ID);

      // Remaining promises should reject with "permanently lost"
      for (let i = 1; i < 50; i++) {
        await expect(promises[i]).rejects.toThrow("Connection lost permanently");
      }
      await expect(overflowPromise).rejects.toThrow("Connection lost permanently");
    });
  });

  describe("audit log records full reconnection sequence", () => {
    it("logs reconnection and connect events in order", () => {
      // Simulate the full lifecycle through status changes
      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setStatus(PROFILE_ID, "reconnecting");
      sessionManager.setStatus(PROFILE_ID, "connected");

      const entries = auditLog.readEntries();
      const profileEntries = entries.filter((e) => e.profileId === PROFILE_ID);

      expect(profileEntries.length).toBeGreaterThanOrEqual(3);
      expect(profileEntries[0].event).toBe("connect");
      expect(profileEntries[1].event).toBe("reconnection");
      expect(profileEntries[2].event).toBe("connect");
    });

    it("logs disconnect when reconnection ultimately fails", () => {
      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setStatus(PROFILE_ID, "reconnecting");
      sessionManager.setStatus(PROFILE_ID, "disconnected");

      const entries = auditLog.readEntries();
      const profileEntries = entries.filter((e) => e.profileId === PROFILE_ID);

      expect(profileEntries).toHaveLength(3);
      expect(profileEntries[0].event).toBe("connect");
      expect(profileEntries[1].event).toBe("reconnection");
      expect(profileEntries[2].event).toBe("disconnect");
    });

    it("does not log credentials in audit entries", () => {
      // The bridgeAuditLog function only passes safe fields
      sessionManager.emit("ssh:status-changed", {
        profileId: PROFILE_ID,
        status: "error",
        error: "Authentication failed",
      });

      const entries = auditLog.readEntries();
      const authEntry = entries.find((e) => e.event === "auth_failure");
      expect(authEntry).toBeDefined();
      expect(authEntry!.details).toEqual({ error: "Authentication failed" });
      // No password, key, or token fields
      expect(JSON.stringify(authEntry)).not.toContain("password");
      expect(JSON.stringify(authEntry)).not.toContain("privateKey");
    });
  });

  describe("cache interaction during reconnection", () => {
    it("returns stale data even when cache entries have expired", async () => {
      // Populate cache with very short TTL
      await cache.getOrFetch(`${REPO_ID}:hydration_status`, async () => ({
        hydrated: 5,
        dehydrated: 12,
        total: 17,
      }), 1); // 1ms TTL — will expire immediately

      // Wait for expiry
      await new Promise((r) => setTimeout(r, 10));

      // Enter reconnecting state
      sessionManager.setStatus(PROFILE_ID, "reconnecting");

      // Should still return stale data
      const result = await router.route(
        REPO_ID, "hydration_status", { repo_id: REPO_ID },
      ) as any;

      expect(result.__stale).toBe(true);
      expect(result.hydrated).toBe(5);
      expect(result.dehydrated).toBe(12);
    });

    it("queues cacheable reads when no cached data exists", async () => {
      // No cache populated for this method
      sessionManager.setStatus(PROFILE_ID, "reconnecting");

      // git_log is cacheable but has no cached data — should be queued
      const logPromise = router.route(
        REPO_ID, "git_log", { repo_id: REPO_ID },
      );

      // Simulate reconnection with auto-responder
      const newChannel = new MockChannel();
      const origWrite = newChannel.write.bind(newChannel);
      newChannel.write = (data: string, cb?: (err?: Error | null) => void): boolean => {
        origWrite(data, cb);
        try {
          const request = JSON.parse(data.trim());
          setImmediate(() => {
            const resp = JSON.stringify({
              jsonrpc: "2.0",
              id: request.id,
              result: { commits: [{ sha: "abc123", message: "initial" }] },
            }) + "\n";
            newChannel.emit("data", Buffer.from(resp));
          });
        } catch {
          // ignore
        }
        return true;
      };

      sessionManager.setStatus(PROFILE_ID, "connected");
      sessionManager.setConnection(PROFILE_ID, {
        status: "connected",
        client: new MockSshClient(),
        channels: { agent: newChannel.asChannel() },
        profile: { operationTimeoutMs: 5000 },
      });
      router.attachChannel(PROFILE_ID, newChannel.asChannel());

      await router.replayQueue(PROFILE_ID);

      const logResult = await logPromise;
      expect(logResult).toEqual({ commits: [{ sha: "abc123", message: "initial" }] });
    });
  });
});
