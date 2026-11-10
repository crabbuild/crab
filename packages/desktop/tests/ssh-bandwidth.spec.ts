import { mockStorageConnections } from "./mock-storage-connections";
/**
 * Tests for bandwidth tracking in the RPC router and connection
 * health metrics (uptime formatting, bandwidth display).
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { parseRepoId } from "../electron/ssh-types";
import { RpcRouter, type BandwidthStats } from "../electron/ssh-router";
import type { SshSessionManager } from "../electron/ssh-manager";
import { EventEmitter } from "node:events";

// ── Mock helpers ─────────────────────────────────────────────────

function createMockChannel() {
  const emitter = new EventEmitter();
  const written: string[] = [];
  return {
    on: emitter.on.bind(emitter),
    emit: emitter.emit.bind(emitter),
    write: (data: string, cb?: (err?: Error | null) => void) => {
      written.push(data);
      cb?.();
    },
    written,
  };
}

function createMockSessionManager(status = "connected") {
  const connections = new Map<string, { channels: { agent: any } }>();
  return { repositoryWork: () => undefined,
    getStatus: vi.fn().mockReturnValue(status),
    getConnection: vi.fn((profileId: string) => connections.get(profileId)),
    setConnection: (profileId: string, channel: any) => {
      connections.set(profileId, { channels: { agent: channel } });
    },
  } as unknown as SshSessionManager & { setConnection: (id: string, ch: any) => void };
}

function createMockLocalAgent() {
  return {
    request: vi.fn().mockResolvedValue({ ok: true }),
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("RpcRouter — bandwidth tracking", () => {
  let router: RpcRouter;
  let sessionManager: ReturnType<typeof createMockSessionManager>;
  let localAgent: ReturnType<typeof createMockLocalAgent>;
  let channel: ReturnType<typeof createMockChannel>;

  beforeEach(() => {
    sessionManager = createMockSessionManager();
    localAgent = createMockLocalAgent();
    router = new RpcRouter(sessionManager, localAgent, mockStorageConnections, { requestTimeoutMs: 5000 });

    channel = createMockChannel();
    sessionManager.setConnection("profile-1", channel);
    router.attachChannel("profile-1", channel as any);
  });

  it("initializes bandwidth counters to zero on attachChannel", () => {
    const stats = router.getBandwidth("profile-1");
    expect(stats.bytesIn).toBe(0);
    expect(stats.bytesOut).toBe(0);
  });

  it("returns zero bandwidth for unknown profiles", () => {
    const stats = router.getBandwidth("unknown-profile");
    expect(stats.bytesIn).toBe(0);
    expect(stats.bytesOut).toBe(0);
  });

  it("tracks outgoing bytes when sending RPC requests", async () => {
    // Send a request (it will time out since we don't respond, but bytes are tracked)
    const promise = router.sendToRemoteAgent("profile-1", "test_method", { key: "value" });

    // Simulate a response so the promise resolves
    const requestFrame = channel.written[0];
    const parsed = JSON.parse(requestFrame);
    const response = JSON.stringify({ jsonrpc: "2.0", id: parsed.id, result: "ok" }) + "\n";
    channel.emit("data", Buffer.from(response));

    await promise;

    const stats = router.getBandwidth("profile-1");
    expect(stats.bytesOut).toBe(Buffer.byteLength(requestFrame, "utf8"));
    expect(stats.bytesIn).toBe(Buffer.byteLength(response));
  });

  it("tracks incoming bytes from channel data events", () => {
    const data1 = Buffer.from('{"jsonrpc":"2.0","method":"notify","params":{}}\n');
    const data2 = Buffer.from('{"jsonrpc":"2.0","method":"notify2","params":{"x":1}}\n');

    channel.emit("data", data1);
    channel.emit("data", data2);

    const stats = router.getBandwidth("profile-1");
    expect(stats.bytesIn).toBe(data1.length + data2.length);
  });

  it("accumulates bandwidth across multiple requests", async () => {
    // Send two requests
    const p1 = router.sendToRemoteAgent("profile-1", "method_a", {});
    const req1 = JSON.parse(channel.written[0]);
    channel.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: req1.id, result: "a" }) + "\n"));
    await p1;

    const p2 = router.sendToRemoteAgent("profile-1", "method_b", { data: "hello" });
    const req2 = JSON.parse(channel.written[1]);
    channel.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: req2.id, result: "b" }) + "\n"));
    await p2;

    const stats = router.getBandwidth("profile-1");
    const expectedOut = Buffer.byteLength(channel.written[0], "utf8") + Buffer.byteLength(channel.written[1], "utf8");
    expect(stats.bytesOut).toBe(expectedOut);
    expect(stats.bytesIn).toBeGreaterThan(0);
  });

  it("resets bandwidth counters", async () => {
    // Generate some traffic
    const p = router.sendToRemoteAgent("profile-1", "test", {});
    const req = JSON.parse(channel.written[0]);
    channel.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: req.id, result: null }) + "\n"));
    await p;

    const before = router.getBandwidth("profile-1");
    expect(before.bytesOut).toBeGreaterThan(0);

    router.resetBandwidth("profile-1");

    const after = router.getBandwidth("profile-1");
    expect(after.bytesIn).toBe(0);
    expect(after.bytesOut).toBe(0);
  });

  it("tracks bandwidth independently per profile", () => {
    // Attach a second profile
    const channel2 = createMockChannel();
    sessionManager.setConnection("profile-2", channel2);
    router.attachChannel("profile-2", channel2 as any);

    // Send data to profile-1 only
    channel.emit("data", Buffer.from('{"jsonrpc":"2.0","method":"n","params":{}}\n'));

    const stats1 = router.getBandwidth("profile-1");
    const stats2 = router.getBandwidth("profile-2");

    expect(stats1.bytesIn).toBeGreaterThan(0);
    expect(stats2.bytesIn).toBe(0);
  });
});
