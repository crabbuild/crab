/**
 * Unit tests for the SshSessionManager class.
 *
 * Mocks the ssh2 Client, keychain, fs, and electron modules to test
 * connection lifecycle, reconnection logic, keep-alive timeout
 * detection, and suspend/resume handling without real network calls.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";

// ─── Mocks ───────────────────────────────────────────────────────────────────

// Mock electron powerMonitor
const mockPowerMonitor = new EventEmitter();
vi.mock("electron", () => ({
  powerMonitor: mockPowerMonitor,
}));

// Mock keychain
vi.mock("../electron/keychain", () => ({
  getPassword: vi.fn(async () => "mock-password"),
}));

// Mock fs for key file reads and agent socket checks
vi.mock("node:fs", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:fs")>();
  return {
    ...actual,
    existsSync: vi.fn(() => true),
    readFileSync: vi.fn((p: unknown) => {
      if (typeof p === "string" && p.includes(".ssh")) {
        return Buffer.from("-----BEGIN OPENSSH PRIVATE KEY-----\nmock\n-----END OPENSSH PRIVATE KEY-----");
      }
      return actual.readFileSync(p as string);
    }),
  };
});

// Mock ssh2 Client — track instances for test assertions
class MockClientChannel extends EventEmitter {
  end = vi.fn();
  write = vi.fn();
  setWindow = vi.fn();
  stderr = new EventEmitter();
}

class MockClient extends EventEmitter {
  connect = vi.fn();
  end = vi.fn();
  destroy = vi.fn();
  exec = vi.fn();
  shell = vi.fn();
  forwardOut = vi.fn();
  sftp = vi.fn();
}

let mockClientInstances: MockClient[] = [];

vi.mock("ssh2", () => ({
  Client: vi.fn(() => {
    const client = new MockClient();
    mockClientInstances.push(client);
    return client;
  }),
}));

import { SshSessionManager, SshProfile } from "../electron/ssh-manager";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeProfile(overrides: Partial<SshProfile> = {}): SshProfile {
  return {
    id: "test-profile-1",
    label: "Test Server",
    host: "test.example.com",
    port: 22,
    username: "testuser",
    authMethod: { type: "agent" },
    forwardAgent: false,
    compression: false,
    keepAliveInterval: 15,
    autoReconnect: true,
    autoDeployAgent: false,
    tags: [],
    portForwards: [],
    createdAt: Date.now(),
    ...overrides,
  };
}

/**
 * Connect a profile and simulate the ssh2 client emitting "ready"
 * after `client.connect()` is called. Returns the mock client.
 */
async function connectProfile(
  mgr: SshSessionManager,
  profile: SshProfile,
): Promise<MockClient> {
  // Set up the mock to emit "ready" when connect() is called
  const connectPromise = mgr.connect(profile);
  const client = mockClientInstances[mockClientInstances.length - 1];

  // The connect mock triggers "ready" on next tick (after event listeners are attached)
  // We need to wait for buildConnectConfig to finish and connect() to be called
  await vi.waitFor(() => {
    expect(client.connect).toHaveBeenCalled();
  });

  client.emit("ready");
  await connectPromise;
  return client;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("SshSessionManager", () => {
  let manager: SshSessionManager;

  beforeEach(() => {
    mockClientInstances = [];
    process.env.SSH_AUTH_SOCK = "/tmp/mock-agent.sock";
    manager = new SshSessionManager();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  // ── Connection lifecycle ─────────────────────────────────────────

  describe("connection lifecycle", () => {
    it("connect creates a connection entry with status 'connecting'", async () => {
      const profile = makeProfile();
      const statusEvents: Array<{ status: string }> = [];
      manager.on("ssh:status-changed", (payload) => statusEvents.push(payload));

      // Start connect — status should immediately be "connecting"
      const connectPromise = manager.connect(profile);

      expect(statusEvents[0]).toEqual(
        expect.objectContaining({ profileId: "test-profile-1", status: "connecting" }),
      );
      expect(manager.getStatus("test-profile-1")).toBe("connecting");

      // Clean up
      const client = mockClientInstances[0];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      client.emit("ready");
      await connectPromise;
    });

    it("connect transitions to 'connected' when ssh2 client emits 'ready'", async () => {
      const profile = makeProfile();
      await connectProfile(manager, profile);
      expect(manager.getStatus("test-profile-1")).toBe("connected");
    });

    it("connect transitions to 'error' when ssh2 client emits 'error'", async () => {
      const profile = makeProfile();
      const connectPromise = manager.connect(profile);

      const client = mockClientInstances[0];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      client.emit("error", new Error("Connection refused"));

      await expect(connectPromise).rejects.toThrow("Connection refused");
      expect(manager.getStatus("test-profile-1")).toBe("error");
    });

    it("connect emits 'ssh:status-changed' events at each transition", async () => {
      const profile = makeProfile();
      const statusEvents: Array<{ profileId: string; status: string; error?: string }> = [];
      manager.on("ssh:status-changed", (payload) => statusEvents.push(payload));

      await connectProfile(manager, profile);

      expect(statusEvents).toHaveLength(2);
      expect(statusEvents[0]).toMatchObject({ profileId: "test-profile-1", status: "connecting" });
      expect(statusEvents[1]).toMatchObject({ profileId: "test-profile-1", status: "connected" });
    });

    it("disconnect closes all channels and sets status to 'disconnected'", async () => {
      const profile = makeProfile();
      const client = await connectProfile(manager, profile);

      await manager.disconnect("test-profile-1");

      expect(manager.getStatus("test-profile-1")).toBe("disconnected");
      expect(client.end).toHaveBeenCalled();
    });

    it("disconnect is idempotent (calling twice doesn't throw)", async () => {
      const profile = makeProfile();
      await connectProfile(manager, profile);

      await manager.disconnect("test-profile-1");
      await expect(manager.disconnect("test-profile-1")).resolves.toBeUndefined();
    });

    it("disconnect on non-existent profile doesn't throw", async () => {
      await expect(manager.disconnect("non-existent")).resolves.toBeUndefined();
    });

    it("connect disconnects existing connection before reconnecting (idempotent)", async () => {
      const profile = makeProfile();

      // First connection
      const client1 = await connectProfile(manager, profile);
      expect(manager.getStatus("test-profile-1")).toBe("connected");

      // Second connection to same profile — should disconnect first
      const initialCount = mockClientInstances.length;
      const connectPromise2 = manager.connect(profile);

      // Wait for the new client to be created and connect() called
      await vi.waitFor(() => {
        expect(mockClientInstances.length).toBeGreaterThan(initialCount);
      });
      const client2 = mockClientInstances[mockClientInstances.length - 1];
      await vi.waitFor(() => expect(client2.connect).toHaveBeenCalled());
      client2.emit("ready");
      await connectPromise2;

      // First client should have been ended (disconnect was called)
      expect(client1.end).toHaveBeenCalled();
      expect(manager.getStatus("test-profile-1")).toBe("connected");
    });
  });

  // ── Reconnection logic ───────────────────────────────────────────

  describe("reconnection logic", () => {
    beforeEach(() => {
      vi.useFakeTimers();
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    it("when connection is lost and autoReconnect is true, status transitions to 'reconnecting'", async () => {
      const profile = makeProfile({ autoReconnect: true });
      const connectPromise = manager.connect(profile);
      const client = mockClientInstances[0];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      client.emit("ready");
      await connectPromise;

      const statusEvents: Array<{ status: string }> = [];
      manager.on("ssh:status-changed", (payload) => statusEvents.push(payload));

      // Simulate connection loss
      client.emit("end");

      expect(manager.getStatus("test-profile-1")).toBe("reconnecting");
      expect(statusEvents).toContainEqual(
        expect.objectContaining({ profileId: "test-profile-1", status: "reconnecting" }),
      );
    });

    it("exponential backoff delays: 1s, 2s, 4s", async () => {
      const profile = makeProfile({ autoReconnect: true });
      const connectPromise = manager.connect(profile);
      const client = mockClientInstances[0];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      client.emit("ready");
      await connectPromise;

      // Trigger reconnection
      client.emit("end");
      const initialCount = mockClientInstances.length;

      // First attempt after 1s (2^0 * 1000)
      await vi.advanceTimersByTimeAsync(999);
      expect(mockClientInstances.length).toBe(initialCount);
      await vi.advanceTimersByTimeAsync(1);
      // A new client should be created for the reconnection attempt
      expect(mockClientInstances.length).toBe(initialCount + 1);

      // Fail the first reconnection attempt
      const client2 = mockClientInstances[mockClientInstances.length - 1];
      await vi.waitFor(() => expect(client2.connect).toHaveBeenCalled());
      client2.emit("error", new Error("fail"));
      await vi.advanceTimersByTimeAsync(0);

      // Second attempt after 2s (2^1 * 1000)
      const countAfterFirst = mockClientInstances.length;
      await vi.advanceTimersByTimeAsync(2000);
      expect(mockClientInstances.length).toBe(countAfterFirst + 1);

      // Fail the second reconnection attempt
      const client3 = mockClientInstances[mockClientInstances.length - 1];
      await vi.waitFor(() => expect(client3.connect).toHaveBeenCalled());
      client3.emit("error", new Error("fail"));
      await vi.advanceTimersByTimeAsync(0);

      // Third attempt after 4s (2^2 * 1000)
      const countAfterSecond = mockClientInstances.length;
      await vi.advanceTimersByTimeAsync(4000);
      expect(mockClientInstances.length).toBe(countAfterSecond + 1);
    });

    it("successful reconnection transitions back to 'connected'", async () => {
      const profile = makeProfile({ autoReconnect: true });
      const connectPromise = manager.connect(profile);
      const client = mockClientInstances[0];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      client.emit("ready");
      await connectPromise;

      // Trigger reconnection
      client.emit("end");
      expect(manager.getStatus("test-profile-1")).toBe("reconnecting");

      // Advance to first reconnection attempt
      await vi.advanceTimersByTimeAsync(1000);

      // Simulate successful reconnection
      const newClient = mockClientInstances[mockClientInstances.length - 1];
      await vi.waitFor(() => expect(newClient.connect).toHaveBeenCalled());
      newClient.emit("ready");
      await vi.advanceTimersByTimeAsync(0);

      expect(manager.getStatus("test-profile-1")).toBe("connected");
    });

    it("disconnect revokes a reconnect awaiting authentication", async () => {
      const profile = makeProfile();
      const initial = await connectProfile(manager, profile);
      initial.emit("end");
      await vi.advanceTimersByTimeAsync(1000);
      const replacement = mockClientInstances.at(-1)!;
      await vi.waitFor(() => expect(replacement.connect).toHaveBeenCalled());

      await manager.disconnect(profile.id);
      expect(replacement.destroy).toHaveBeenCalled();
      replacement.emit("ready");
      await vi.advanceTimersByTimeAsync(0);
      expect(manager.getStatus(profile.id)).toBe("disconnected");
    });

    it("after 60s total failure, gives up and transitions to 'disconnected'", async () => {
      const profile = makeProfile({ autoReconnect: true });
      const connectPromise = manager.connect(profile);
      const client = mockClientInstances[0];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      client.emit("ready");
      await connectPromise;

      // Trigger reconnection
      client.emit("end");

      // Keep failing reconnection attempts until 60s elapses
      // Backoff: 1s, 2s, 4s, 8s, 16s, 30s (capped), 30s...
      const delays = [1000, 2000, 4000, 8000, 16000, 30000, 30000];
      for (const delay of delays) {
        await vi.advanceTimersByTimeAsync(delay);
        const latestClient = mockClientInstances[mockClientInstances.length - 1];
        await vi.waitFor(() => expect(latestClient.connect).toHaveBeenCalled());
        latestClient.emit("error", new Error("fail"));
        await vi.advanceTimersByTimeAsync(0);

        if (manager.getStatus("test-profile-1") === "disconnected") {
          break;
        }
      }

      expect(manager.getStatus("test-profile-1")).toBe("disconnected");
    });
  });

  // ── Keep-alive timeout detection ─────────────────────────────────

  describe("keep-alive timeout detection", () => {
    it("when ssh2 client emits 'end', triggers reconnection if autoReconnect is true", async () => {
      const profile = makeProfile({ autoReconnect: true });
      const client = await connectProfile(manager, profile);

      client.emit("end");

      expect(manager.getStatus("test-profile-1")).toBe("reconnecting");
    });

    it("when ssh2 client emits 'end' and autoReconnect is false, sets status to 'disconnected'", async () => {
      const profile = makeProfile({ autoReconnect: false });
      const client = await connectProfile(manager, profile);

      client.emit("end");

      expect(manager.getStatus("test-profile-1")).toBe("disconnected");
    });
  });

  // ── Suspend/resume handling ──────────────────────────────────────

  describe("suspend/resume handling", () => {
    it("checkConnectionHealth returns true for responsive connections", async () => {
      const profile = makeProfile();
      const client = await connectProfile(manager, profile);

      // Mock exec to simulate a responsive connection
      client.exec.mockImplementation((_cmd: string, cb: Function) => {
        const stream = new MockClientChannel();
        cb(null, stream);
        setImmediate(() => {
          stream.emit("data", Buffer.from("1\n"));
          stream.emit("close");
        });
      });

      const healthy = await manager.checkConnectionHealth("test-profile-1");
      expect(healthy).toBe(true);
    });

    it("checkConnectionHealth returns false for unresponsive connections (exec error)", async () => {
      const profile = makeProfile();
      const client = await connectProfile(manager, profile);

      // Mock exec to return an error
      client.exec.mockImplementation((_cmd: string, cb: Function) => {
        cb(new Error("channel open failed"));
      });

      const healthy = await manager.checkConnectionHealth("test-profile-1");
      expect(healthy).toBe(false);
    });

    it("checkConnectionHealth returns false for disconnected profiles", async () => {
      const healthy = await manager.checkConnectionHealth("non-existent-profile");
      expect(healthy).toBe(false);
    });
  });

  // ── Test connection ──────────────────────────────────────────────

  describe("testConnection", () => {
    /**
     * Helper: testConnection creates its own Client internally after
     * buildConnectConfig resolves. We need to wait for it to appear.
     */
    async function waitForTestClient(initialCount: number): Promise<MockClient> {
      await vi.waitFor(() => {
        expect(mockClientInstances.length).toBeGreaterThan(initialCount);
      });
      const client = mockClientInstances[mockClientInstances.length - 1];
      await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      return client;
    }

    it("returns { ok: true } on success", async () => {
      const profile = makeProfile();
      const initialCount = mockClientInstances.length;

      const testPromise = manager.testConnection(profile);

      const client = await waitForTestClient(initialCount);

      // Mock exec before emitting ready so it's available when called
      client.exec.mockImplementation((_cmd: string, cb: Function) => {
        const stream = new MockClientChannel();
        cb(null, stream);
        setImmediate(() => {
          stream.emit("data", Buffer.from("ok\n"));
          stream.emit("close");
        });
      });

      client.emit("ready");

      const result = await testPromise;
      expect(result).toEqual({ ok: true });
    });

    it("returns categorized error for auth failure", async () => {
      const profile = makeProfile();
      const initialCount = mockClientInstances.length;

      const testPromise = manager.testConnection(profile);
      const client = await waitForTestClient(initialCount);
      client.emit("error", new Error("All configured authentication methods failed"));

      const result = await testPromise;
      expect(result.ok).toBe(false);
      expect(result.error).toBe("Authentication failed");
    });

    it("returns categorized error for host unreachable", async () => {
      const profile = makeProfile();
      const initialCount = mockClientInstances.length;

      const testPromise = manager.testConnection(profile);
      const client = await waitForTestClient(initialCount);
      const err = new Error("connect ECONNREFUSED") as NodeJS.ErrnoException;
      err.code = "ECONNREFUSED";
      client.emit("error", err);

      const result = await testPromise;
      expect(result.ok).toBe(false);
      expect(result.error).toBe("Host unreachable");
    });

    it("returns categorized error for timeout", async () => {
      const profile = makeProfile();
      const initialCount = mockClientInstances.length;

      const testPromise = manager.testConnection(profile);
      const client = await waitForTestClient(initialCount);
      const err = new Error("Timed out while waiting for handshake") as NodeJS.ErrnoException;
      err.code = "ETIMEDOUT";
      client.emit("error", err);

      const result = await testPromise;
      expect(result.ok).toBe(false);
      expect(result.error).toBe("Connection timed out");
    });

    it("returns categorized error for DNS failure", async () => {
      const profile = makeProfile();
      const initialCount = mockClientInstances.length;

      const testPromise = manager.testConnection(profile);
      const client = await waitForTestClient(initialCount);
      const err = new Error("getaddrinfo ENOTFOUND bad.host") as NodeJS.ErrnoException;
      err.code = "ENOTFOUND";
      client.emit("error", err);

      const result = await testPromise;
      expect(result.ok).toBe(false);
      expect(result.error).toBe("DNS resolution failed");
    });

    it("doesn't modify the connections Map", async () => {
      const profile = makeProfile();
      const initialCount = mockClientInstances.length;

      const testPromise = manager.testConnection(profile);
      const client = await waitForTestClient(initialCount);
      client.emit("error", new Error("fail"));

      await testPromise;

      // testConnection should not store anything in the connections map
      expect(manager.getConnection("test-profile-1")).toBeUndefined();
    });
  });
});
