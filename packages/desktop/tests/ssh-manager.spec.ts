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
import type { ClientChannel } from "ssh2";
import { RpcRouter } from "../electron/ssh-router";
import { AgentDeployer } from "../electron/ssh-deployer";
import { CapabilityNegotiator } from "../electron/ssh-capabilities";
import type { AgentCapabilities } from "../electron/ssh-types";
import { getPassword } from "../electron/keychain";

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
    manager = new SshSessionManager(async () => {});
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

    it("cancelReconnection stops further attempts", async () => {
      const client = await connectProfile(manager, makeProfile());
      client.emit("end");
      manager.cancelReconnection("test-profile-1");
      await vi.advanceTimersByTimeAsync(60_000);
      expect({ status: manager.getStatus("test-profile-1"), clients: mockClientInstances.length }).toEqual({ status: "disconnected", clients: 1 });
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


describe("SSH connection ownership", () => {
  const managers: SshSessionManager[] = [];
  const capabilities: AgentCapabilities = { version: "2.0.0", features: { fileWatching: false, deltaSync: false, compression: false, processManagement: false, environmentManagement: false, debugging: false, search: false, buildRun: false, logTailing: false } };
  class AgentChannel extends EventEmitter {
    end = vi.fn();
    destroy = vi.fn(() => { this.emit("close"); });
    write(frame: string, done?: (error?: Error | null) => void): boolean {
      const request = JSON.parse(frame) as { id: number; method: string };
      this.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: request.id, result: request.method === "capabilities" ? capabilities : { method: request.method } }) + "\n"));
      done?.(); return true;
    }
    asChannel(): ClientChannel { return this as unknown as ClientChannel; }
  }
  function pending<T>() {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>(done => { resolve = done; });
    return { promise, resolve };
  }
  function fixture() {
    let manager: SshSessionManager;
    const proxy = { getStatus: (id: string) => manager.getStatus(id), getConnection: (id: string) => manager.getConnection(id) } as SshSessionManager;
    const router = new RpcRouter(proxy, { request: async () => { throw new Error("Local fallback must not run"); } });
    const deployer = new AgentDeployer(async () => { throw new Error("Fixture must supply its startup channel"); }); const negotiator = new CapabilityNegotiator(router);
    const channels: AgentChannel[] = [];
    const deploy = vi.spyOn(deployer, "ensureAgent").mockImplementation(async () => { const channel = new AgentChannel(); channels.push(channel); return channel.asChannel(); });
    manager = new SshSessionManager(async () => {});
    manager.setAgentDependencies({ createDeployer: () => deployer, capabilityNegotiator: negotiator, rpcRouter: router });
    managers.push(manager);
    return { manager, router, negotiator, deploy, channels };
  }
  beforeEach(() => { vi.useFakeTimers(); mockClientInstances = []; process.env.SSH_AUTH_SOCK = "/tmp/mock-agent.sock"; });
  afterEach(async () => { for (const manager of managers.splice(0)) await manager.disconnect("test-profile-1"); vi.clearAllTimers(); vi.useRealTimers(); vi.restoreAllMocks(); });

  it("does not complete connect before the deployed channel and capabilities are ready", async () => {
    const { manager, deploy, negotiator } = fixture(); const channel = new AgentChannel(); const starting = pending<ClientChannel>();
    deploy.mockReturnValueOnce(starting.promise); let settled = false;
    const connected = manager.connect(makeProfile({ autoDeployAgent: true })).then(() => { settled = true; });
    await vi.waitFor(() => expect(mockClientInstances[0].connect).toHaveBeenCalled()); mockClientInstances[0].emit("ready");
    await vi.advanceTimersByTimeAsync(0);
    const before = { status: manager.getStatus("test-profile-1"), settled };
    starting.resolve(channel.asChannel()); await connected; await vi.advanceTimersByTimeAsync(0);
    expect({ before, after: manager.getStatus("test-profile-1"), capabilities: negotiator.getCached("test-profile-1") }).toEqual({ before: { status: "deploying", settled: false }, after: "connected", capabilities });
  });

  it("publishes deployment completion only after the capability handshake", async () => {
    const { manager, negotiator } = fixture(); const handshake = pending<AgentCapabilities>();
    vi.spyOn(negotiator, "negotiate").mockReturnValueOnce(handshake.promise);
    const completed: unknown[] = [];
    manager.on("ssh:deploy-progress", value => { if (value.stage === "starting" && value.percent === 100) completed.push(value); });
    const connecting = manager.connect(makeProfile({ autoDeployAgent: true }));
    await vi.waitFor(() => expect(mockClientInstances[0].connect).toHaveBeenCalled()); mockClientInstances[0].emit("ready"); await vi.advanceTimersByTimeAsync(0);
    expect({ completed: completed.length, status: manager.getStatus("test-profile-1") }).toEqual({ completed: 0, status: "deploying" });
    handshake.resolve(capabilities); await connecting;
    expect(completed).toEqual([{ profileId: "test-profile-1", stage: "starting", percent: 100 }]);
  });

  it("redeploys and actually executes the router's queued read after reconnect", async () => {
    const { manager, router, deploy, channels } = fixture(); const client = await connectProfile(manager, makeProfile({ autoDeployAgent: true }));
    await vi.advanceTimersByTimeAsync(0); client.emit("end");
    let result: unknown;
    void router.sendToRemoteAgent("test-profile-1", "task_compute_diff", {}).then(value => { result = value; });
    await vi.advanceTimersByTimeAsync(1000); const replacement = mockClientInstances.at(-1)!; replacement.emit("ready");
    await vi.advanceTimersByTimeAsync(0);
    expect({ deployments: deploy.mock.calls.length, channels: channels.length, result, status: manager.getStatus("test-profile-1") }).toEqual({ deployments: 2, channels: 2, result: { method: "task_compute_diff" }, status: "connected" });
  });

  it("ignores a retired client's late end, close and error", async () => {
    const { manager } = fixture(); const previous = await connectProfile(manager, makeProfile({ autoDeployAgent: true })); await vi.advanceTimersByTimeAsync(0);
    await connectProfile(manager, makeProfile({ autoDeployAgent: true })); await vi.advanceTimersByTimeAsync(0);
    previous.emit("end"); previous.emit("close"); previous.emit("error", new Error("Retired transport"));
    expect(manager.getStatus("test-profile-1")).toBe("connected");
  });

  it("does not resurrect a disconnected reconnect attempt after late ready", async () => {
    const { manager } = fixture(); const previous = await connectProfile(manager, makeProfile({ autoDeployAgent: true })); await vi.advanceTimersByTimeAsync(0); previous.emit("end");
    await vi.advanceTimersByTimeAsync(1000); const pendingClient = mockClientInstances.at(-1)!;
    await manager.disconnect("test-profile-1"); pendingClient.emit("ready"); await vi.advanceTimersByTimeAsync(0);
    expect(manager.getStatus("test-profile-1")).toBe("disconnected");
  });

  it("retires a channel that finishes deploying after disconnect", async () => {
    const { manager, deploy } = fixture(); const starting = pending<ClientChannel>(); const channel = new AgentChannel(); deploy.mockReturnValueOnce(starting.promise);
    const connected = manager.connect(makeProfile({ autoDeployAgent: true })).catch(() => {});
    await vi.waitFor(() => expect(mockClientInstances[0].connect).toHaveBeenCalled()); mockClientInstances[0].emit("ready"); await vi.advanceTimersByTimeAsync(0);
    await manager.disconnect("test-profile-1"); starting.resolve(channel.asChannel()); await connected; await vi.advanceTimersByTimeAsync(0);
    expect({ status: manager.getStatus("test-profile-1"), destroyed: channel.destroy.mock.calls.length, channel: manager.getConnection("test-profile-1")?.channels.agent }).toEqual({ status: "disconnected", destroyed: 1, channel: undefined });
  });

  it("settles an opening handshake when the server closes without ready", async () => {
    const { manager } = fixture(); let outcome = "pending";
    void manager.connect(makeProfile({ autoDeployAgent: true })).then(() => { outcome = "connected"; }, () => { outcome = "failed"; });
    await vi.waitFor(() => expect(mockClientInstances[0].connect).toHaveBeenCalled()); mockClientInstances[0].emit("close"); await vi.advanceTimersByTimeAsync(0);
    expect(outcome).toBe("failed");
  });

  it("enforces the total reconnect deadline while a handshake is still opening", async () => {
    const { manager } = fixture(); const previous = await connectProfile(manager, makeProfile({ autoDeployAgent: true })); await vi.advanceTimersByTimeAsync(0); previous.emit("end");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(manager.getStatus("test-profile-1")).toBe("disconnected");
  });
  it("uses the same attached owner for explicit deployment and makes it idempotent", async () => {
    const { manager, deploy, negotiator } = fixture();
    await connectProfile(manager, makeProfile({ autoDeployAgent: false }));
    await manager.deployAgent("test-profile-1"); await manager.deployAgent("test-profile-1");
    expect({ deployments: deploy.mock.calls.length, attached: Boolean(manager.getConnection("test-profile-1")?.channels.agent), capabilities: negotiator.getCached("test-profile-1") }).toEqual({ deployments: 1, attached: true, capabilities });
  });

  it("rejects a queued read on permanent disconnect", async () => {
    const { manager, router } = fixture(); const client = await connectProfile(manager, makeProfile({ autoDeployAgent: true }));
    client.emit("end"); const read = router.sendToRemoteAgent("test-profile-1", "task_compute_diff", {});
    const outcome = read.catch(error => (error as Error).message);
    await manager.disconnect("test-profile-1");
    expect(await outcome).toBe("Connection lost permanently");
  });

  it("cancels credential lookup without starting a late SSH transport", async () => {
    const { manager } = fixture(); const password = pending<string | null>(); vi.mocked(getPassword).mockReturnValueOnce(password.promise);
    let outcome = "pending";
    void manager.connect(makeProfile({ authMethod: { type: "password" }, autoDeployAgent: false })).then(() => { outcome = "connected"; }, () => { outcome = "failed"; });
    const client = mockClientInstances[0]; await manager.disconnect("test-profile-1"); await vi.advanceTimersByTimeAsync(0);
    const cancelled = outcome; password.resolve("owned-fixture-password"); await vi.advanceTimersByTimeAsync(0);
    expect({ cancelled, connected: client.connect.mock.calls.length, status: manager.getStatus("test-profile-1") }).toEqual({ cancelled: "failed", connected: 0, status: "disconnected" });
  });

  it("keeps concurrent jump chains with their own profile", async () => {
    const { manager } = fixture();
    const first = manager.connect(makeProfile({ id: "first", jumpHost: "first-bastion", autoDeployAgent: false }));
    const [firstClient, firstJump] = mockClientInstances;
    const second = manager.connect(makeProfile({ id: "second", jumpHost: "second-bastion", autoDeployAgent: false }));
    const [secondClient, secondJump] = mockClientInstances.slice(2);
    const tunnel = (_source: string, _sourcePort: number, _target: string, _targetPort: number, done: (error: Error | undefined, stream: ClientChannel) => void) => done(undefined, new AgentChannel().asChannel());
    firstJump.forwardOut.mockImplementation(tunnel); secondJump.forwardOut.mockImplementation(tunnel);
    try {
      firstJump.emit("ready"); secondJump.emit("ready"); await vi.advanceTimersByTimeAsync(0);
      firstClient.emit("ready"); secondClient.emit("ready"); await Promise.all([first, second]);
      const owners = [Object.is(manager.getConnection("first")?.jumpClients[0], firstJump), Object.is(manager.getConnection("second")?.jumpClients[0], secondJump)];
      await manager.disconnect("first");
      expect({ owners, firstClosed: firstJump.end.mock.calls.length > 0, secondClosed: secondJump.end.mock.calls.length }).toEqual({ owners: [true, true], firstClosed: true, secondClosed: 0 });
    } finally { await manager.disconnect("first"); await manager.disconnect("second"); }
  });

});

describe("SSH host verification ownership", () => {
  let manager: SshSessionManager;
  let verify: ReturnType<typeof vi.fn<[string, number, Buffer, AbortSignal], Promise<void>>>;
  beforeEach(() => {
    vi.useFakeTimers(); mockClientInstances = []; process.env.SSH_AUTH_SOCK = "/tmp/mock-agent.sock";
    verify = vi.fn<[string, number, Buffer, AbortSignal], Promise<void>>(async () => {}); manager = new SshSessionManager(verify);
  });
  afterEach(async () => { await manager.disconnect("test-profile-1"); vi.clearAllTimers(); vi.useRealTimers(); vi.restoreAllMocks(); });
  async function start() {
    const connected = manager.connect(makeProfile());
    const outcome = connected.catch((error: unknown) => error);
    await vi.waitFor(() => expect(mockClientInstances[0].connect).toHaveBeenCalled());
    const client = mockClientInstances[0];
    const config = client.connect.mock.calls[0][0] as import("ssh2").ConnectConfig;
    const hostVerifier = config.hostVerifier as import("ssh2").HostVerifier;
    return { client, config, hostVerifier, outcome };
  }
  it("pauses the network deadline while the owner reviews the fingerprint", async () => {
    let approve!: () => void; verify.mockImplementation(() => new Promise<void>(resolve => { approve = resolve; }));
    const { client, config, hostVerifier, outcome } = await start(); const callback = vi.fn(); const key = Buffer.from("fixture");
    await vi.advanceTimersByTimeAsync(9000); hostVerifier(key, callback);
    await vi.advanceTimersByTimeAsync(60_000);
    expect({ destroyed: client.destroy.mock.calls.length, decisions: callback.mock.calls.length, readyTimeout: config.readyTimeout, identity: verify.mock.calls[0].slice(0, 3) }).toEqual({ destroyed: 0, decisions: 0, readyTimeout: 0, identity: ["test.example.com", 22, key] });
    approve(); await vi.advanceTimersByTimeAsync(0); client.emit("ready"); await outcome;
    expect(callback).toHaveBeenCalledWith(true);
  });
  it("retains the remaining network budget after approval", async () => {
    const { client, hostVerifier, outcome } = await start();
    await vi.advanceTimersByTimeAsync(9000); hostVerifier(Buffer.from("fixture"), vi.fn());
    await vi.advanceTimersByTimeAsync(1001);
    expect({ result: (await outcome as Error).message, destroyed: client.destroy.mock.calls.length > 0 }).toEqual({ result: "SSH connection handshake timed out", destroyed: true });
  });
  it("does not resume a retired handshake after late approval", async () => {
    let approve!: () => void; verify.mockImplementation(() => new Promise<void>(resolve => { approve = resolve; }));
    const { hostVerifier, outcome } = await start(); const callback = vi.fn();
    hostVerifier(Buffer.from("fixture"), callback); await manager.disconnect("test-profile-1"); approve(); await outcome; await vi.advanceTimersByTimeAsync(0);
    expect({ decisions: callback.mock.calls.length, status: manager.getStatus("test-profile-1"), aborted: verify.mock.calls[0][3].aborted }).toEqual({ decisions: 0, status: "disconnected", aborted: true });
  });
  it("stops reconnecting after a trust rejection", async () => {
    const first = await connectProfile(manager, makeProfile()); first.emit("end");
    await vi.advanceTimersByTimeAsync(1000); const replacement = mockClientInstances.at(-1)!;
    const hostVerifier = replacement.connect.mock.calls[0][0].hostVerifier as import("ssh2").HostVerifier;
    const { HostKeyError } = await import("../electron/ssh-known-hosts");
    verify.mockRejectedValue(new HostKeyError("SSH_HOST_KEY_REJECTED", "Key rejected"));
    hostVerifier(Buffer.from("fixture"), vi.fn()); await vi.advanceTimersByTimeAsync(60_000);
    expect({ status: manager.getStatus("test-profile-1"), attempts: mockClientInstances.length }).toEqual({ status: "error", attempts: 2 });
  });
  it("keeps verification active for rekey after readiness", async () => {
    const { client, hostVerifier, outcome } = await start(); client.emit("ready"); await outcome;
    const { HostKeyError } = await import("../electron/ssh-known-hosts");
    verify.mockRejectedValue(new HostKeyError("SSH_HOST_KEY_REVOKED", "Key revoked"));
    hostVerifier(Buffer.from("rekey"), vi.fn()); await vi.advanceTimersByTimeAsync(60_000);
    expect({ status: manager.getStatus("test-profile-1"), attempts: mockClientInstances.length, destroyed: client.destroy.mock.calls.length }).toEqual({ status: "error", attempts: 1, destroyed: 1 });
  });
});

describe("SSH connection test command ownership", () => {
  beforeEach(() => { vi.useFakeTimers(); mockClientInstances = []; process.env.SSH_AUTH_SOCK = "/tmp/mock-agent.sock"; });
  afterEach(() => { vi.clearAllTimers(); vi.useRealTimers(); vi.restoreAllMocks(); });
  async function testing() {
    const manager = new SshSessionManager(async () => {});
    let result: { ok: boolean; error?: string } | undefined;
    const done = manager.testConnection(makeProfile()).then(value => { result = value; });
    await vi.waitFor(() => expect(mockClientInstances[0]?.connect).toHaveBeenCalled());
    const client = mockClientInstances[0]; const channel = Object.assign(new MockClientChannel(), { destroy: vi.fn() });
    client.exec.mockImplementation((_command: string, callback: (error: null, stream: typeof channel) => void) => callback(null, channel));
    client.emit("ready"); await vi.advanceTimersByTimeAsync(0);
    return { client, channel, done, result: () => result };
  }
  it("bounds a command that never closes and retires its transport", async () => {
    const test = await testing(); await vi.advanceTimersByTimeAsync(5001);
    try { expect({ result: test.result(), closed: test.client.destroy.mock.calls.length > 0 }).toEqual({ result: { ok: false, error: "Connection timed out" }, closed: true }); }
    finally { test.channel.emit("close", 0); await test.done; }
  });
  it("fails immediately if the transport closes before the command", async () => {
    const test = await testing(); test.client.emit("close"); await vi.advanceTimersByTimeAsync(0);
    try { expect(test.result()).toEqual({ ok: false, error: "SSH connection closed during connection test" }); }
    finally { test.channel.emit("close", 0); await test.done; }
  });
  it.each([[1, undefined], [null, "TERM"]])("does not accept expected output with failed exit %s/%s", async (code, signal) => {
    const test = await testing(); test.channel.emit("data", Buffer.from("ok\n")); test.channel.emit("close", code, signal); await test.done;
    expect(test.result()?.ok).toBe(false);
  });
  it("does not treat an unrelated word containing ok as success", async () => {
    const test = await testing(); test.channel.emit("data", Buffer.from("broken\n")); test.channel.emit("close", 0); await test.done;
    expect(test.result()).toEqual({ ok: false, error: "Remote command did not return expected output" });
  });
  it("handles command stream errors without an unhandled event", async () => {
    const test = await testing(); test.channel.emit("error", new Error("Remote channel failed")); await test.done;
    expect(test.result()).toEqual({ ok: false, error: "Remote channel failed" }); test.channel.emit("close");
  });
  it("bounds output from a command that floods stdout", async () => {
    const test = await testing(); test.channel.emit("data", Buffer.alloc(70 * 1024)); await test.done;
    expect(test.result()).toEqual({ ok: false, error: "Remote connection test output exceeded 64 KiB" }); test.channel.emit("close");
  });
  it("retires an exec channel delivered after the command deadline", async () => {
    const manager = new SshSessionManager(async () => {}); const done = manager.testConnection(makeProfile());
    await vi.waitFor(() => expect(mockClientInstances[0]?.connect).toHaveBeenCalled()); const client = mockClientInstances[0];
    let callback!: (error: null, stream: MockClientChannel & { destroy: ReturnType<typeof vi.fn> }) => void;
    client.exec.mockImplementation((_command: string, ready: typeof callback) => { callback = ready; });
    client.emit("ready"); await vi.advanceTimersByTimeAsync(5001); await done;
    const late = Object.assign(new MockClientChannel(), { destroy: vi.fn() }); callback(null, late);
    expect({ destroyed: late.destroy.mock.calls.length, readers: late.listenerCount("data") }).toEqual({ destroyed: 1, readers: 0 }); late.emit("close");
  });

});
