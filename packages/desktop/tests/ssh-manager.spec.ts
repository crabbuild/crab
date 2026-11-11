/**
 * Unit tests for the SshSessionManager class.
 *
 * Mocks the ssh2 Client, keychain, fs, and electron modules to test
 * connection lifecycle, reconnection logic, keep-alive timeout
 * detection, and suspend/resume handling without real network calls.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { Client, ClientChannel, SFTPWrapper } from "ssh2";
import type { AgentDeployer } from "../electron/ssh-deployer";
import type { CapabilityNegotiator } from "../electron/ssh-capabilities";
import type { RpcRouter } from "../electron/ssh-router";

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
  stderr = Object.assign(new EventEmitter(), { resume: vi.fn() });
}

class MockClient extends EventEmitter {
  connect = vi.fn();
  setNoDelay = vi.fn();
  end = vi.fn();
  destroy = vi.fn(() => { this.emit("close"); return this; });
  exec = vi.fn();
  shell = vi.fn();
  forwardOut = vi.fn();
  sftp = vi.fn();
}

let mockClientInstances: MockClient[] = [];

vi.mock("ssh2", () => ({
  Client: vi.fn(function () {
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

function nativeRuntime(manager: SshSessionManager) {
  const channels: MockClientChannel[] = [];
  const capabilities = { protocol: 1 as const, version: "test", methods: ["capabilities", "open_local"] };
  let cached = false;
  const send = vi.fn(async (_profile: string, method: string, params: Record<string, unknown>): Promise<unknown> => method === "open_local" ? { repo_id: params.repo_id, url: params.repo_id } : {});
  const replay = vi.fn(async () => {});
  manager.setAgentDependencies({
    createDeployer: () => Object.assign(new EventEmitter(), { ensureAgent: vi.fn(async () => {
      const channel = new MockClientChannel();
      channel.end.mockImplementation(() => { channel.emit("close", 0); });
      channels.push(channel); return channel;
    }) }) as unknown as AgentDeployer,
    capabilityNegotiator: {
      getCached: () => cached ? capabilities : null,
      negotiate: async () => { cached = true; return capabilities; },
      clearCache: () => { cached = false; },
    } as unknown as CapabilityNegotiator,
    rpcRouter: { attachChannel: vi.fn(), sendToRemoteAgent: send, retireRepository: (profile: string, repo_id: string) => send(profile, "close", { repo_id }), replayQueue: replay, clearQueue: vi.fn() } as unknown as RpcRouter,
  });
  return { channels, send, replay };
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

    it("connected profiles reuse the authenticated connection", async () => {
      const profile = makeProfile();
      const client1 = await connectProfile(manager, profile);
      const reopened = manager.connect(profile);
      await new Promise<void>(resolve => setImmediate(resolve));
      for (const client of mockClientInstances) client.emit("ready");
      await reopened;
      expect(mockClientInstances).toHaveLength(1);
      expect(client1.end).not.toHaveBeenCalled();
    });

    it("concurrent repository restores share the same connection attempt", async () => {
      const profile = makeProfile();
      const joined = Promise.allSettled([manager.connect(profile), manager.connect(profile)]);
      await new Promise<void>(resolve => setImmediate(resolve));
      for (const client of mockClientInstances) client.emit("ready");
      expect((await joined).map(result => result.status)).toEqual(["fulfilled", "fulfilled"]);
      expect(mockClientInstances).toHaveLength(1);
    });

    it("reuses a restored repository instead of replacing its newly captured readers", async () => {
      const profile = makeProfile();
      const channels: MockClientChannel[] = [];
      const capabilities = { protocol: 1 as const, version: "test", methods: ["capabilities", "open_local"] };
      const send = vi.fn(async (_profile: string, method: string, params: { repo_id: string }) => method === "open_local" ? { repo_id: params.repo_id, url: params.repo_id } : {});
      manager.setAgentDependencies({
        createDeployer: () => Object.assign(new EventEmitter(), { ensureAgent: vi.fn(async () => {
          const channel = new MockClientChannel();
          channel.end.mockImplementation(() => { channel.emit("close", 0); });
          channels.push(channel); return channel;
        }) }) as unknown as AgentDeployer,
        capabilityNegotiator: { getCached: () => capabilities, negotiate: async () => capabilities, clearCache: vi.fn() } as unknown as CapabilityNegotiator,
        rpcRouter: { attachChannel: vi.fn(), sendToRemoteAgent: send, retireRepository: (profile: string, repo_id: string) => send(profile, "close", { repo_id }), replayQueue: vi.fn(async () => {}), clearQueue: vi.fn() } as unknown as RpcRouter,
      });
      await connectProfile(manager, profile);
      try {
        await manager.ensureAgent(profile.id);
        await manager.openRepository(profile.id, "/repository");
        await manager.openRepository(profile.id, "/repository");
        expect(send.mock.calls.filter(([, method]) => method === "open_local")).toHaveLength(1);
        channels[0].emit("close", 0);
        await manager.ensureAgent(profile.id);
        await manager.openRepository(profile.id, "/repository");
        expect(send.mock.calls.filter(([, method]) => method === "open_local")).toHaveLength(2);
      } finally { await manager.disconnect(profile.id); }
    });

    it("a failed repository replay leaves its sibling ready and retries only the failed repository", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const first = await manager.openRepository(profile.id, "/first");
      const second = await manager.openRepository(profile.id, "/second");
      const native = runtime.send.getMockImplementation()!;
      runtime.send.mockImplementation(async (id, method, params) => {
        if (method === "open_local" && params.repo_id === second.repo_id) throw new Error("Repository directory was removed");
        return native(id, method, params);
      });
      runtime.channels[0].emit("close", 0);
      try {
        await expect(manager.ensureAgent(profile.id)).resolves.toBeUndefined();
        expect(manager.repositoryRuntime(profile.id, String(first.repo_id)).sessionId).not.toBeNull();
        expect(manager.repositoryRuntime(profile.id, String(second.repo_id))).toMatchObject({ sessionId: null, error: "Repository directory was removed" });
        runtime.send.mockImplementation(native); runtime.send.mockClear();
        await manager.openRepository(profile.id, "/second");
        expect(manager.repositoryRuntime(profile.id, String(second.repo_id)).sessionId).not.toBeNull();
        expect(runtime.send.mock.calls.map(([, method, params]) => [method, params.repo_id])).toEqual([["open_local", second.repo_id]]);
        expect(runtime.channels).toHaveLength(2);
      } finally { await manager.disconnect(profile.id); }
    });

    it("storage replacement revokes only the affected repository without replacing SSH", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const first = await manager.openRepository(profile.id, "/first");
      const second = await manager.openRepository(profile.id, "/second");
      const process = manager.agentSession(profile.id);
      await manager.storageIdentityChanged([String(first.repo_id)], "changed");
      expect(manager.repositoryRuntime(profile.id, String(first.repo_id)).sessionId).not.toBe(first.native_session_id);
      expect(manager.repositoryRuntime(profile.id, String(second.repo_id)).sessionId).toBe(second.native_session_id);
      expect(manager.agentSession(profile.id)).toBe(process);
      expect(runtime.send.mock.calls.filter(([, method]) => method === "open_local")).toHaveLength(3);
      await manager.disconnect(profile.id);
    });

    it("storage removal drains native readers while retaining the repository for repair", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const first = await manager.openRepository(profile.id, "/first");
      await manager.storageIdentityChanged([String(first.repo_id)], "removed");
      expect(manager.repositoryRuntime(profile.id, String(first.repo_id))).toMatchObject({ sessionId: null, error: expect.stringContaining("saved connection was removed") });
      expect(runtime.send).toHaveBeenCalledWith(profile.id, "close", { repo_id: first.repo_id });
      await manager.storageIdentityChanged([String(first.repo_id)], "changed");
      expect(manager.repositoryRuntime(profile.id, String(first.repo_id)).sessionId).not.toBeNull();
      await manager.disconnect(profile.id);
    });

    it("reselection waits for the removed identity's native close before replacement", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const first = await manager.openRepository(profile.id, "/first");
      const native = runtime.send.getMockImplementation()!;
      let release!: () => void;
      const closing = new Promise<void>(resolve => { release = resolve; });
      runtime.send.mockImplementation(async (id, method, params) => {
        if (method === "close") await closing;
        return native(id, method, params);
      });
      const removed = manager.storageIdentityChanged([String(first.repo_id)], "removed");
      await vi.waitFor(() => expect(runtime.send.mock.calls.some(([, method]) => method === "close")).toBe(true));
      const replaced = manager.storageIdentityChanged([String(first.repo_id)], "changed");
      try {
        await new Promise<void>(resolve => setImmediate(resolve));
        expect(runtime.send.mock.calls.filter(([, method]) => method === "open_local")).toHaveLength(1);
      } finally { release(); await Promise.all([removed, replaced]); await manager.disconnect(profile.id); }
    });

    it("concurrent opens join the repository's native open outcome", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      let release!: (value: unknown) => void;
      const outcome = new Promise(resolve => { release = resolve; });
      runtime.send.mockImplementation(() => outcome);
      try {
        const joined = Promise.all([manager.openRepository(profile.id, "/first"), manager.openRepository(profile.id, "/first")]);
        await new Promise<void>(resolve => setImmediate(resolve));
        const sent = runtime.send.mock.calls.length;
        release({ repo_id: `ssh://${profile.id}/first` });
        await joined;
        expect(sent).toBe(1);
      } finally { await manager.disconnect(profile.id); }
    });

    it("a slow repository replay does not hold agent readiness or its healthy sibling", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const first = await manager.openRepository(profile.id, "/first");
      const second = await manager.openRepository(profile.id, "/second");
      const native = runtime.send.getMockImplementation()!;
      runtime.replay.mockClear();
      let release!: (value: unknown) => void;
      const blocked = new Promise(resolve => { release = resolve; });
      runtime.send.mockImplementation((id, method, params) => method === "open_local" && params.repo_id === second.repo_id ? blocked : native(id, method, params));
      runtime.channels[0].emit("close", 0);
      let ready = false;
      const restarting = manager.ensureAgent(profile.id).then(() => { ready = true; });
      try {
        await vi.waitFor(() => expect(ready).toBe(true));
        await expect(manager.openRepository(profile.id, "/first")).resolves.toMatchObject({ repo_id: first.repo_id });
        expect(manager.repositoryRuntime(profile.id, String(second.repo_id)).sessionId).toBeNull();
        expect(runtime.replay).toHaveBeenCalledWith(profile.id);
      } finally {
        release({ repo_id: second.repo_id });
        await restarting;
        await manager.disconnect(profile.id);
      }
    });

    it("forgetting a repository during replay closes the late native owner without restoring it", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const handle = await manager.openRepository(profile.id, "/first");
      const native = runtime.send.getMockImplementation()!;
      let release!: (value: unknown) => void;
      const outcome = new Promise(resolve => { release = resolve; });
      runtime.send.mockImplementation((id, method, params) => method === "open_local" ? outcome : native(id, method, params));
      runtime.send.mockClear();
      runtime.channels[0].emit("close", 0);
      try {
        const restarting = manager.ensureAgent(profile.id);
        await vi.waitFor(() => expect(runtime.send).toHaveBeenCalledWith(profile.id, "open_local", expect.objectContaining({ repo_id: handle.repo_id })));
        manager.forgetRepository(profile.id, String(handle.repo_id));
        release({ repo_id: handle.repo_id });
        await restarting;
        expect(manager.repositoryRuntimes(profile.id)).toEqual([]);
        await vi.waitFor(() => expect(runtime.send).toHaveBeenCalledWith(profile.id, "close", { repo_id: handle.repo_id }));
      } finally { await manager.disconnect(profile.id); }
    });

    it("late replay cleanup cannot revoke a replacement native session", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const repository = await manager.openRepository(profile.id, "/first");
      const native = runtime.send.getMockImplementation()!;
      let release!: (value: unknown) => void;
      const blocked = new Promise(resolve => { release = resolve; });
      runtime.send.mockImplementation(() => blocked);
      runtime.channels[0].emit("close", 0);
      await manager.ensureAgent(profile.id);
      runtime.channels[1].emit("close", 0);
      runtime.send.mockImplementation(native);
      try {
        await manager.ensureAgent(profile.id);
        const replacement = await manager.openRepository(profile.id, "/first");
        release({ repo_id: repository.repo_id });
        await new Promise<void>(resolve => setImmediate(resolve));
        expect(manager.repositoryRuntime(profile.id, String(repository.repo_id)).sessionId).toBe(replacement.native_session_id);
      } finally {
        release({ repo_id: repository.repo_id });
        await manager.disconnect(profile.id);
      }
    });

    it("disconnect sends native EOF before joining an in-flight repository replay", async () => {
      const profile = makeProfile();
      const runtime = nativeRuntime(manager);
      await connectProfile(manager, profile); await manager.ensureAgent(profile.id);
      const handle = await manager.openRepository(profile.id, "/first");
      let release!: (value: unknown) => void;
      const outcome = new Promise(resolve => { release = resolve; });
      runtime.send.mockImplementation(() => outcome); runtime.send.mockClear();
      runtime.channels[0].emit("close", 0);
      const restarting = manager.ensureAgent(profile.id);
      void restarting.catch(() => {});
      await vi.waitFor(() => expect(runtime.send).toHaveBeenCalled());
      const closing = manager.disconnect(profile.id);
      void closing.catch(() => {});
      await new Promise<void>(resolve => setImmediate(resolve));
      const sentEof = runtime.channels[1].end.mock.calls.length;
      release({ repo_id: handle.repo_id });
      await Promise.allSettled([restarting, closing]);
      expect(sentEof).toBe(1);
    });

    it.each([false, true])("disconnect joins authentication without resurrection (handshake started: %s)", async started => {
      const profile = makeProfile();
      const opening = manager.connect(profile);
      const checked = expect(opening).rejects.toThrow("SSH connection closed");
      const client = mockClientInstances[0];
      if (started) await vi.waitFor(() => expect(client.connect).toHaveBeenCalled());
      await manager.disconnect(profile.id);
      await checked;
      client.emit("ready");
      expect(manager.getStatus(profile.id)).toBe("disconnected");
      expect(client.destroy).toHaveBeenCalled();
    });

    it("concurrent jump profiles retain their own transports", async () => {
      const first = makeProfile({ id: "first", jumpHost: "first-hop" });
      const second = makeProfile({ id: "second", jumpHost: "second-hop" });
      const opening = Promise.all([manager.connect(first), manager.connect(second)]);
      await vi.waitFor(() => expect(mockClientInstances).toHaveLength(4));
      const [firstTarget, secondTarget, firstJump, secondJump] = mockClientInstances;
      for (const jump of [firstJump, secondJump]) {
        jump.forwardOut.mockImplementation((_from, _port, _host, _target, callback) => callback(undefined, new MockClientChannel() as unknown as ClientChannel));
        jump.emit("ready");
      }
      await vi.waitFor(() => expect(secondTarget.connect).toHaveBeenCalled());
      firstTarget.emit("ready"); secondTarget.emit("ready");
      await opening;
      expect(manager.getConnection(first.id)?.jumpClients).toEqual([firstJump as unknown as Client]);
      expect(manager.getConnection(second.id)?.jumpClients).toEqual([secondJump as unknown as Client]);
      await manager.disconnect(first.id);
      expect(firstJump.end).toHaveBeenCalled();
      expect(secondJump.end).not.toHaveBeenCalled();
      await manager.disconnect(second.id);
    });
  });

  // ── Reconnection logic ───────────────────────────────────────────

  describe("reconnection logic", () => {
    it("a transport lost during reconnect bootstrap remains reconnecting", async () => {
      const profile = makeProfile();
      const initial = await connectProfile(manager, profile);
      let fail!: (error: Error) => void;
      const ensureAgent = vi.fn(() => new Promise<never>((_resolve, reject) => { fail = reject; }));
      manager.setAgentDependencies({
        createDeployer: () => Object.assign(new EventEmitter(), { ensureAgent }) as unknown as AgentDeployer,
        capabilityNegotiator: { clearCache: vi.fn() } as unknown as CapabilityNegotiator,
        rpcRouter: { clearQueue: vi.fn() } as unknown as RpcRouter,
      });
      manager.getConnection(profile.id)!.profile.autoDeployAgent = true;
      initial.emit("end");
      await vi.advanceTimersByTimeAsync(1000);
      const replacement = mockClientInstances.at(-1)!;
      await vi.waitFor(() => expect(replacement.connect).toHaveBeenCalled());
      replacement.emit("ready");
      await vi.advanceTimersByTimeAsync(0);
      replacement.emit("end");
      fail(new Error("SSH transport ended during deployment"));
      await vi.advanceTimersByTimeAsync(0);
      expect(manager.getStatus(profile.id)).toBe("reconnecting");
      await manager.disconnect(profile.id);
    });

    it("repository reopen joins the background reconnect", async () => {
      const profile = makeProfile();
      const initial = await connectProfile(manager, profile);
      initial.emit("end");
      const reopened = manager.connect(profile);
      await vi.advanceTimersByTimeAsync(1000);
      const replacement = mockClientInstances.at(-1)!;
      await vi.waitFor(() => expect(replacement.connect).toHaveBeenCalled());
      replacement.emit("ready");
      await reopened;
      expect(mockClientInstances).toHaveLength(2);
      expect(manager.getStatus(profile.id)).toBe("connected");
    });

    it("rejects an SFTP callback from the replaced transport", async () => {
      const profile = makeProfile();
      const first = await connectProfile(manager, profile);
      let finish: ((error: undefined, sftp: SFTPWrapper) => void) | undefined;
      first.sftp.mockImplementation(callback => { finish = callback; });
      const opening = manager.getSftp(profile.id);
      const checked = expect(opening).rejects.toThrow("SSH connection closed while opening SFTP");
      first.emit("end");
      await vi.advanceTimersByTimeAsync(1000);
      const replacement = mockClientInstances.at(-1)!;
      await vi.waitFor(() => expect(replacement.connect).toHaveBeenCalled());
      replacement.emit("ready");
      await vi.advanceTimersByTimeAsync(0);
      const stale = new MockClientChannel();
      if (!finish) throw new Error("SFTP callback was not registered");
      finish(undefined, stale as unknown as SFTPWrapper);
      await checked;
      expect(stale.end).toHaveBeenCalled();
    });

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
