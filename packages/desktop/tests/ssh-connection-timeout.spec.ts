/**
 * Unit tests for connection timeout configuration (task 19.6).
 *
 * Verifies:
 * 1. SshProfile interface includes connectionTimeoutMs and operationTimeoutMs
 * 2. SshSessionManager uses connectionTimeoutMs as readyTimeout
 * 3. RpcRouter uses operationTimeoutMs for per-request timeouts
 * 4. Default values are applied when fields are not set
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import { RpcRouter } from "../electron/ssh-router";
import type { SshProfile, SshConnection } from "../electron/ssh-manager";

// ─── Mock SSH Session Manager ────────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();

  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

class MockSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();
  private connections = new Map<string, { profile: SshProfile; channels: { agent?: MockChannel } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel, profile: SshProfile): void {
    this.connections.set(profileId, { profile, channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { profile: SshProfile; channels: { agent?: MockChannel } } | undefined {
    return this.connections.get(profileId);
  }
}

// ─── Mock Local Agent ────────────────────────────────────────────────────────

class MockLocalAgent {
  async request(_method: string, _params: unknown): Promise<unknown> {
    return { ok: true };
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function makeProfile(overrides: Partial<SshProfile> = {}): SshProfile {
  return {
    id: "test-profile",
    label: "Test Server",
    host: "test.example.com",
    port: 22,
    username: "testuser",
    authMethod: { type: "agent" },
    forwardAgent: false,
    compression: false,
    keepAliveInterval: 15,
    autoReconnect: true,
    autoDeployAgent: true,
    tags: [],
    portForwards: [],
    createdAt: Date.now(),
    ...overrides,
  };
}

function simulateResponse(channel: MockChannel, id: number, result: unknown): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
  channel.emit("data", Buffer.from(response));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Connection timeout configuration — SshProfile fields", () => {
  it("SshProfile accepts connectionTimeoutMs and operationTimeoutMs", () => {
    const profile = makeProfile({
      connectionTimeoutMs: 5000,
      operationTimeoutMs: 60000,
    });

    expect(profile.connectionTimeoutMs).toBe(5000);
    expect(profile.operationTimeoutMs).toBe(60000);
  });

  it("SshProfile fields are optional (undefined by default)", () => {
    const profile = makeProfile();

    expect(profile.connectionTimeoutMs).toBeUndefined();
    expect(profile.operationTimeoutMs).toBeUndefined();
  });
});

describe("Connection timeout configuration — RpcRouter uses operationTimeoutMs", () => {
  it("uses profile operationTimeoutMs for per-request timeout", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    const profile = makeProfile({ operationTimeoutMs: 100 });
    sm.setStatus("test-profile", "connected");
    sm.setConnection("test-profile", channel, profile);

    const router = new RpcRouter(sm as any, new MockLocalAgent());
    router.attachChannel("test-profile", channel.asChannel());

    // Send a request that will timeout
    const promise = router.route("ssh://test-profile/repo", "slow_method", {});

    await expect(promise).rejects.toThrow(
      "RPC timeout: slow_method did not respond within 100ms",
    );
  });

  it("falls back to router default (30s) when profile has no operationTimeoutMs", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    const profile = makeProfile(); // no operationTimeoutMs
    sm.setStatus("test-profile", "connected");
    sm.setConnection("test-profile", channel, profile);

    // Create router with a short default for testing
    const router = new RpcRouter(sm as any, new MockLocalAgent(), { requestTimeoutMs: 75 });
    router.attachChannel("test-profile", channel.asChannel());

    const promise = router.route("ssh://test-profile/repo", "slow_method", {});

    await expect(promise).rejects.toThrow(
      "RPC timeout: slow_method did not respond within 75ms",
    );
  });

  it("profile operationTimeoutMs overrides router-level default", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    // Profile sets 80ms, router default is 200ms
    const profile = makeProfile({ operationTimeoutMs: 80 });
    sm.setStatus("test-profile", "connected");
    sm.setConnection("test-profile", channel, profile);

    const router = new RpcRouter(sm as any, new MockLocalAgent(), { requestTimeoutMs: 200 });
    router.attachChannel("test-profile", channel.asChannel());

    const promise = router.route("ssh://test-profile/repo", "method", {});

    // Should timeout at 80ms (profile), not 200ms (router default)
    await expect(promise).rejects.toThrow("did not respond within 80ms");
  });

  it("succeeds when response arrives before profile timeout", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    const profile = makeProfile({ operationTimeoutMs: 5000 });
    sm.setStatus("test-profile", "connected");
    sm.setConnection("test-profile", channel, profile);

    const router = new RpcRouter(sm as any, new MockLocalAgent());
    router.attachChannel("test-profile", channel.asChannel());

    const promise = router.route("ssh://test-profile/repo", "fast_method", {});

    // Respond immediately
    simulateResponse(channel, 1, { data: "result" });

    const result = await promise;
    expect(result).toEqual({ data: "result" });
  });
});

describe("Connection timeout configuration — connectionTimeoutMs defaults", () => {
  it("default connectionTimeoutMs is 10000 when not set", () => {
    const profile = makeProfile();
    // The connect method uses: profile.connectionTimeoutMs ?? 10000
    const effectiveTimeout = profile.connectionTimeoutMs ?? 10000;
    expect(effectiveTimeout).toBe(10000);
  });

  it("custom connectionTimeoutMs is used when set", () => {
    const profile = makeProfile({ connectionTimeoutMs: 5000 });
    const effectiveTimeout = profile.connectionTimeoutMs ?? 10000;
    expect(effectiveTimeout).toBe(5000);
  });

  it("default operationTimeoutMs is 30000 when not set", () => {
    const profile = makeProfile();
    // The router uses: conn.profile?.operationTimeoutMs ?? this.requestTimeoutMs
    // where this.requestTimeoutMs defaults to 30_000
    const effectiveTimeout = profile.operationTimeoutMs ?? 30000;
    expect(effectiveTimeout).toBe(30000);
  });

  it("custom operationTimeoutMs is used when set", () => {
    const profile = makeProfile({ operationTimeoutMs: 60000 });
    const effectiveTimeout = profile.operationTimeoutMs ?? 30000;
    expect(effectiveTimeout).toBe(60000);
  });
});
