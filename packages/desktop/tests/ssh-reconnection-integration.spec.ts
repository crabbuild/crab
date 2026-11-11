import { mockStorageConnections } from "./mock-storage-connections";
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
 * 5. Only explicitly cacheable metadata can be replayed
 * 6. Reconnection success → queued calls are replayed
 * 7. Closed terminal attachments resume their existing tmux sessions
 * 8. Audit log records the reconnection event
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ClientChannel, ExecOptions } from "ssh2";

import { RpcRouter } from "../electron/ssh-router";
import { RemoteCache } from "../electron/ssh-cache";
import { RemoteTerminalManager } from "../electron/ssh-terminal";
import { SshAuditLog, bridgeAuditLog } from "../electron/ssh-audit-log";

// ── Mock SSH channel ─────────────────────────────────────────────

class MockChannel extends EventEmitter {
  stderr = new EventEmitter();
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

  close(): void { this.end(); }
  resume(): void {}

  setWindow(_rows: number, _cols: number, _height: number, _width: number): void {
    // no-op for tests
  }

  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

// ── Mock SSH Client ──────────────────────────────────────────────

type ExecCallback = (error: Error | null, stream: ClientChannel) => void;

class MockSshClient extends EventEmitter {
  commands: string[] = [];
  controlError: string | null = null;
  private terminalChannels: MockChannel[] = [];

  addTerminalChannel(channel: MockChannel): void { this.terminalChannels.push(channel); }

  exec(command: string, optionsOrCallback: ExecOptions | ExecCallback, callback?: ExecCallback): void {
    this.commands.push(command);
    if (typeof optionsOrCallback === "function") {
      const channel = new MockChannel();
      const error = this.controlError;
      this.controlError = null;
      optionsOrCallback(null, channel.asChannel());
      queueMicrotask(() => {
        if (error) channel.stderr.emit("data", Buffer.from(error));
        channel.emit("close", error ? 1 : 0);
      });
    } else {
      const channel = this.terminalChannels.shift() ?? new MockChannel();
      callback?.(null, channel.asChannel());
      setImmediate(() => channel.emit("data", Buffer.from("tmux screen")));
    }
  }

  end(): void { this.emit("end"); }
}

// ── Mock SSH Session Manager ─────────────────────────────────────

class MockSessionManager extends EventEmitter {
  repositoryWork(): Promise<unknown> | undefined { return undefined; }
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
    router = new RpcRouter(sessionManager as any, localAgent, mockStorageConnections);
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

      // Live operations stay bound to the lost agent; only explicitly
      // cacheable metadata without a stale entry can wait for another session.
      for (const method of ["git_stash_list", "git_blame", "file_open", "rpc/cancel"]) {
        await expect(router.route(REPO_ID, method, { repo_id: REPO_ID, op_id: "lost" })).rejects.toMatchObject({ code: "SSH_RECONNECTING" });
      }
      const logPromise = router.route(REPO_ID, "git_log", { repo_id: REPO_ID });
      const hydrationPromise = router.route(REPO_ID, "hydration_status", { repo_id: REPO_ID });

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
          if (method === "git_log") {
            result = { history: true };
          } else if (method === "hydration_status") {
            result = { files: 3 };
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
      expect(await logPromise).toEqual({ history: true });
      expect(await hydrationPromise).toEqual({ files: 3 });

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
    function connect(client: MockSshClient) {
      sessionManager.setConnection(PROFILE_ID, { status: "connected", client });
      sessionManager.setStatus(PROFILE_ID, "connected");
    }

    it("retains the terminal identity when its transport channel closes", async () => {
      const client = new MockSshClient();
      const channel = new MockChannel();
      client.addTerminalChannel(channel);
      connect(client);
      const id = await terminalManager.openTerminal(PROFILE_ID, 80, 24, "/home/user");
      const ended = vi.fn();
      terminalManager.on("ssh:terminal-session-ended", ended);
      channel.emit("close");
      expect(terminalManager.getSession(id)).toMatchObject({ terminalId: id, alive: false, awaitingReconnect: true, channel: null });
      expect(ended).toHaveBeenCalledWith({ terminalId: id, profileId: PROFILE_ID });
    });

    it("automatically reattaches the same tmux session on a connected event", async () => {
      const first = new MockSshClient();
      const oldChannel = new MockChannel();
      first.addTerminalChannel(oldChannel);
      connect(first);
      const id = await terminalManager.openTerminal(PROFILE_ID, 80, 24);
      oldChannel.emit("close");
      sessionManager.setStatus(PROFILE_ID, "reconnecting");
      const replacement = new MockSshClient();
      const resumed = vi.fn();
      terminalManager.on("ssh:terminal-resumed", resumed);
      connect(replacement);
      await vi.waitFor(() => expect(resumed).toHaveBeenCalledWith({ terminalId: id, profileId: PROFILE_ID }));
      expect(terminalManager.getSession(id)).toMatchObject({ alive: true, awaitingReconnect: false });
      expect(replacement.commands).toEqual([
        expect.stringContaining(`has-session -t '=crab-${id}'`),
        expect.stringContaining(`attach-session -d -t '=crab-${id}'`),
      ]);
      expect(oldChannel.written).toEqual([]);
    });

    it("keeps a failed remote restore retryable without creating a new shell", async () => {
      const first = new MockSshClient();
      const channel = new MockChannel();
      first.addTerminalChannel(channel);
      connect(first);
      const id = await terminalManager.openTerminal(PROFILE_ID, 120, 40);
      channel.emit("close");
      const replacement = new MockSshClient();
      replacement.controlError = "session is not available";
      const output = vi.fn();
      terminalManager.on("ssh:terminal-data", output);
      connect(replacement);
      await vi.waitFor(() => expect(output).toHaveBeenCalledWith(expect.objectContaining({ terminalId: id, data: expect.stringContaining("session is not available") })));
      expect(terminalManager.getSession(id)).toMatchObject({ alive: false, awaitingReconnect: true });
      expect(replacement.commands).toEqual([expect.stringContaining(`has-session -t '=crab-${id}'`)]);
      const resumed = vi.fn();
      terminalManager.on("ssh:terminal-resumed", resumed);
      terminalManager.writeToTerminal(id, "\r");
      await vi.waitFor(() => expect(resumed).toHaveBeenCalledWith({ terminalId: id, profileId: PROFILE_ID }));
      expect(replacement.commands.some(command => command.includes("new-session"))).toBe(false);
    });

    it("reattaches closed terminals while leaving healthy attachments untouched", async () => {
      const client = new MockSshClient();
      const healthy = new MockChannel();
      const closed = new MockChannel();
      client.addTerminalChannel(healthy);
      client.addTerminalChannel(closed);
      connect(client);
      const healthyId = await terminalManager.openTerminal(PROFILE_ID, 80, 24);
      const closedId = await terminalManager.openTerminal(PROFILE_ID, 80, 24);
      closed.emit("close");
      const replacement = new MockSshClient();
      const resumed = vi.fn();
      terminalManager.on("ssh:terminal-resumed", resumed);
      connect(replacement);
      await vi.waitFor(() => expect(resumed).toHaveBeenCalledOnce());
      expect(resumed).toHaveBeenCalledWith({ terminalId: closedId, profileId: PROFILE_ID });
      expect(terminalManager.getSession(healthyId)?.channel).toBe(healthy);
      expect(replacement.commands.every(command => !command.includes(healthyId))).toBe(true);
    });
  });

  describe("queue overflow during extended reconnection", () => {
    it("drops oldest calls when queue exceeds max capacity", async () => {
      sessionManager.setStatus(PROFILE_ID, "reconnecting");

      // Queue 50 calls (the max)
      const promises: Promise<unknown>[] = [];
      for (let i = 0; i < 50; i++) {
        promises.push(
          router.route(REPO_ID, "git_log", { repo_id: REPO_ID, idx: i }),
        );
      }

      // Queue one more — should drop the oldest
      const overflowPromise = router.route(
        REPO_ID, "git_log", { repo_id: REPO_ID, idx: 50 },
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
