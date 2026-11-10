/**
 * Unit tests for the RPC Router module.
 *
 * Tests routing logic, timeout handling, queue/replay during
 * reconnection, notification forwarding, and batch RPC.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import { RpcRouter, parseRepoId, routeRpcCall } from "../electron/ssh-router";
import type { RpcRouterOptions } from "../electron/ssh-router";

// ─── Mock SSH Session Manager ────────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();

  /** Cast to ClientChannel for type-safe usage in tests. */
  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

class MockSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockChannel } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { channels: { agent?: MockChannel } } | undefined {
    return this.connections.get(profileId);
  }
}

// ─── Mock Local Agent ────────────────────────────────────────────────────────

class MockLocalAgent {
  calls: Array<{ method: string; params: unknown }> = [];
  nextResult: unknown = { ok: true };
  shouldFail = false;

  async request(method: string, params: unknown): Promise<unknown> {
    this.calls.push({ method, params });
    if (this.shouldFail) throw new Error("local agent error");
    return this.nextResult;
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createRouter(
  sessionManager?: MockSessionManager,
  localAgent?: MockLocalAgent,
  options?: RpcRouterOptions,
) {
  const sm = sessionManager ?? new MockSessionManager();
  const la = localAgent ?? new MockLocalAgent();
  const router = new RpcRouter(sm as any, la, options);
  return { router, sessionManager: sm, localAgent: la };
}

function simulateResponse(channel: MockChannel, id: number, result: unknown): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n";
  channel.emit("data", Buffer.from(response));
}

function simulateErrorResponse(channel: MockChannel, id: number, code: number, message: string): void {
  const response = JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } }) + "\n";
  channel.emit("data", Buffer.from(response));
}

function simulateNotification(channel: MockChannel, method: string, params: unknown): void {
  const notification = JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n";
  channel.emit("data", Buffer.from(notification));
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("parseRepoId", () => {
  it("returns null for local paths", () => {
    expect(parseRepoId("/home/user/repo")).toBeNull();
    expect(parseRepoId("C:\\Users\\repo")).toBeNull();
    expect(parseRepoId("relative/path")).toBeNull();
  });

  it("parses ssh:// prefix correctly", () => {
    const result = parseRepoId("ssh://profile-1/home/user/project");
    expect(result).toEqual({ profileId: "profile-1", remotePath: "/home/user/project" });
  });

  it("handles profile ID with no path", () => {
    const result = parseRepoId("ssh://profile-1");
    expect(result).toEqual({ profileId: "profile-1", remotePath: "/" });
  });

  it("handles complex profile IDs", () => {
    const result = parseRepoId("ssh://my-gpu-server-123/data/models");
    expect(result).toEqual({ profileId: "my-gpu-server-123", remotePath: "/data/models" });
  });
});

describe("RpcRouter — routing logic", () => {
  it("routes local repo IDs to the local agent", async () => {
    const { router, localAgent } = createRouter();
    localAgent.nextResult = { status: "clean" };

    const result = await router.route("/home/user/repo", "git_status", {});
    expect(result).toEqual({ status: "clean" });
    expect(localAgent.calls).toHaveLength(1);
    expect(localAgent.calls[0].method).toBe("git_status");
  });

  it("routes ssh:// repo IDs to the remote agent", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("gpu-server", "connected");
    sm.setConnection("gpu-server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("gpu-server", channel.asChannel());

    const promise = router.route("ssh://gpu-server/data/repo", "git_status", { repo_id: "ssh://gpu-server/data/repo" });

    // Simulate response from remote agent
    simulateResponse(channel, 1, { status: "dirty" });

    const result = await promise;
    expect(result).toEqual({ status: "dirty" });
  });

  it("routes registered remote transports correctly", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("my-server", "connected");
    sm.setConnection("my-server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("my-server", channel.asChannel());
    router.registerTransport("custom-repo-id", { type: "remote", profileId: "my-server" });

    const promise = router.route("custom-repo-id", "ping", {});
    simulateResponse(channel, 1, "pong");

    const result = await promise;
    expect(result).toBe("pong");
  });

  it("throws when connection is not available", async () => {
    const sm = new MockSessionManager();
    sm.setStatus("offline-server", "disconnected");

    const { router } = createRouter(sm);

    await expect(
      router.route("ssh://offline-server/repo", "git_status", {}),
    ).rejects.toThrow("SSH connection not available");
  });
});

describe("RpcRouter — NDJSON framing", () => {
  it("handles partial lines correctly", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const promise = router.route("ssh://server/repo", "test", {});

    // Send response in two chunks (partial NDJSON)
    const fullResponse = JSON.stringify({ jsonrpc: "2.0", id: 1, result: "hello" }) + "\n";
    const mid = Math.floor(fullResponse.length / 2);
    channel.emit("data", Buffer.from(fullResponse.slice(0, mid)));
    channel.emit("data", Buffer.from(fullResponse.slice(mid)));

    const result = await promise;
    expect(result).toBe("hello");
  });

  it("handles multiple responses in a single chunk", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const p1 = router.route("ssh://server/repo", "method1", {});
    const p2 = router.route("ssh://server/repo", "method2", {});

    // Both responses in one chunk
    const combined =
      JSON.stringify({ jsonrpc: "2.0", id: 1, result: "r1" }) + "\n" +
      JSON.stringify({ jsonrpc: "2.0", id: 2, result: "r2" }) + "\n";
    channel.emit("data", Buffer.from(combined));

    expect(await p1).toBe("r1");
    expect(await p2).toBe("r2");
  });

  it("writes requests as NDJSON (JSON + newline)", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const promise = router.route("ssh://server/repo", "git_log", { limit: 10 });

    // Verify the written frame is valid NDJSON
    expect(channel.written).toHaveLength(1);
    const written = channel.written[0];
    expect(written.endsWith("\n")).toBe(true);

    const parsed = JSON.parse(written.trim());
    expect(parsed.jsonrpc).toBe("2.0");
    expect(parsed.id).toBe(1);
    expect(parsed.method).toBe("git_log");
    expect(parsed.params).toEqual({ limit: 10 });

    // Resolve to avoid hanging
    simulateResponse(channel, 1, []);
    await promise;
  });
});

describe("RpcRouter — timeout handling", () => {
  it("rejects with timeout error after configured duration", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm, undefined, { requestTimeoutMs: 50 });
    router.attachChannel("server", channel.asChannel());

    await expect(
      router.route("ssh://server/repo", "slow_method", {}),
    ).rejects.toThrow("RPC timeout: slow_method did not respond within 50ms");
  });

  it("does not block other calls when one times out", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm, undefined, { requestTimeoutMs: 50 });
    router.attachChannel("server", channel.asChannel());

    // First call will timeout
    const p1 = router.route("ssh://server/repo", "slow", {});

    // Second call responds quickly
    const p2 = router.route("ssh://server/repo", "fast", {});
    simulateResponse(channel, 2, "fast-result");

    expect(await p2).toBe("fast-result");
    await expect(p1).rejects.toThrow("RPC timeout");
  });

  it("clears timeout when response arrives in time", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm, undefined, { requestTimeoutMs: 200 });
    router.attachChannel("server", channel.asChannel());

    const promise = router.route("ssh://server/repo", "method", {});

    // Respond before timeout
    simulateResponse(channel, 1, "ok");

    const result = await promise;
    expect(result).toBe("ok");
  });
});

describe("RpcRouter — call queuing during reconnection", () => {
  it("queues read-only calls during reconnection", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "reconnecting");

    const { router } = createRouter(sm);

    // This should be queued, not rejected
    const promise = router.route("ssh://server/repo", "git_status_porcelain", {});

    // Simulate reconnection success
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);
    router.attachChannel("server", channel.asChannel());

    // Replay the queue
    const replayPromise = router.replayQueue("server");

    // Respond to the replayed call
    simulateResponse(channel, 1, { status: "clean" });
    await replayPromise;

    const result = await promise;
    expect(result).toEqual({ status: "clean" });
  });

  it.each(["git_worktree_list", "staging_stats", "pointer_info", "pointer_top", "forge_list_prs"].flatMap(method => ["repo", "profile", "batch"].map(entry => ({ method, entry }))))("replays supported $method reads through $entry admission", async ({ method, entry }) => {
    const sm = new MockSessionManager(); const channel = new MockChannel();
    const { router } = createRouter(sm); sm.setStatus("server", "reconnecting");
    const repo = "ssh://server/repo"; const params = { repo_id: repo }; const response = { observed: method };
    const pending = entry === "profile" ? router.sendToRemoteAgent("server", method, params)
      : entry === "batch" ? router.batchRpc(repo, [{ method, params }]) : router.route(repo, method, params);
    // Consume rejection immediately so a broken admission rule is a test failure,
    // not an unhandled promise left behind while replay is inspected.
    const result = pending.then(value => ({ value }), error => ({ error }));
    const before = channel.written.length;
    sm.setStatus("server", "connected"); sm.setConnection("server", channel); router.attachChannel("server", channel.asChannel());
    const replay = router.replayQueue("server");
    const request = channel.written[0] ? JSON.parse(channel.written[0]) as { id: number; method: string; params: unknown } : undefined;
    if (request) simulateResponse(channel, request.id, response);
    await replay;
    expect({ before, request: request && { method: request.method, params: request.params }, result: await result }).toEqual({ before: 0, request: { method, params }, result: { value: entry === "batch" ? [{ result: response }] : response } });
  });

  it("rejects write operations immediately during reconnection", async () => {
    const sm = new MockSessionManager();
    sm.setStatus("server", "reconnecting");

    const { router } = createRouter(sm);

    await expect(
      router.route("ssh://server/repo", "git_commit", { message: "test" }),
    ).rejects.toThrow("Connection lost: mutating or unclassified operations are unavailable during reconnection");
  });

  it("drops oldest calls when queue overflows", async () => {
    const sm = new MockSessionManager();
    sm.setStatus("server", "reconnecting");

    const { router } = createRouter(sm, undefined, { maxQueuedCalls: 3 });

    const promises: Promise<unknown>[] = [];
    const rejections: Error[] = [];

    // Queue 4 calls (max is 3, so first should be dropped)
    for (let i = 0; i < 4; i++) {
      const p = router.route("ssh://server/repo", "git_log", { i });
      p.catch((err) => rejections.push(err));
      promises.push(p);
    }

    // Wait a tick for the rejection to propagate
    await new Promise((r) => setTimeout(r, 10));

    // First call should have been dropped
    expect(rejections).toHaveLength(1);
    expect(rejections[0].message).toContain("queue overflow");
  });

  it("clears queue on permanent disconnect", async () => {
    const sm = new MockSessionManager();
    sm.setStatus("server", "reconnecting");

    const { router } = createRouter(sm);

    const rejections: Error[] = [];
    const p1 = router.route("ssh://server/repo", "git_status_porcelain", {});
    p1.catch((err) => rejections.push(err));

    router.clearQueue("server");

    await new Promise((r) => setTimeout(r, 10));
    expect(rejections).toHaveLength(1);
    expect(rejections[0].message).toContain("Connection lost permanently");
  });
});

describe("RpcRouter — repository ownership across channels", () => {
  it("reopens acknowledged repos but excludes failed and closed opens", async () => {
    const sm = new MockSessionManager(); sm.setStatus("server", "connected");
    const { router } = createRouter(sm); const first = new MockChannel(); router.attachChannel("server", first.asChannel());
    async function request(method: string, params: Record<string, string>, error = false) {
      const pending = router.sendToRemoteAgent("server", method, params).catch((failure: unknown) => failure);
      const frame = JSON.parse(first.written.at(-1)!) as { id: number };
      if (error) simulateErrorResponse(first, frame.id, -32000, "Open failed"); else simulateResponse(first, frame.id, { repo_id: params.repo_id });
      await pending;
    }
    await request("open_local", { repo_id: "kept", path: "/repo" });
    await request("open_local", { repo_id: "another", path: "/another" });
    await request("open_local", { repo_id: "failed", path: "/missing" }, true);
    await request("open_local", { repo_id: "closed", path: "/closed" }); await request("close", { repo_id: "closed" });
    const second = new MockChannel(); router.attachChannel("server", second.asChannel());
    const restored = router.restoreRepositories("server");
    for (let i = 0; i < 2; i++) {
      await vi.waitFor(() => expect(second.written.length).toBe(i + 1));
      const frame = JSON.parse(second.written[i]) as { id: number }; simulateResponse(second, frame.id, {});
    }
    await restored;
    expect(second.written.map(frame => { const { method, params } = JSON.parse(frame) as { method: string; params: unknown }; return { method, params }; })).toEqual([
      { method: "open_local", params: { repo_id: "kept", path: "/repo" } }, { method: "open_local", params: { repo_id: "another", path: "/another" } },
    ]);
  });

  it("rejects a retired restore and never writes its next open into the newer channel", async () => {
    const sm = new MockSessionManager(); sm.setStatus("server", "connected");
    const { router } = createRouter(sm); const first = new MockChannel(); router.attachChannel("server", first.asChannel());
    for (const id of ["first", "second"]) {
      const opening = router.sendToRemoteAgent("server", "open_local", { repo_id: id, path: `/${id}` });
      const request = JSON.parse(first.written.at(-1)!) as { id: number }; simulateResponse(first, request.id, {}); await opening;
    }
    const second = new MockChannel(); router.attachChannel("server", second.asChannel());
    const restoring = router.restoreRepositories("server").catch((error: unknown) => error);
    const third = new MockChannel(); router.attachChannel("server", third.asChannel());
    expect({ result: await restoring, writes: third.written }).toEqual({ result: expect.objectContaining({ message: expect.stringContaining("replaced") }), writes: [] });
    router.forgetRepositories("server"); await router.restoreRepositories("server"); expect(third.written).toEqual([]);
  });
});

describe("RpcRouter — notification forwarding", () => {
  it("forwards notifications with profileId tag", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const notifications: Array<{ profileId: string; method: string; params: unknown }> = [];
    router.on("notification", (n) => notifications.push(n));

    // Simulate a notification from the remote agent
    simulateNotification(channel, "progress/update", { percent: 50 });

    expect(notifications).toHaveLength(1);
    expect(notifications[0]).toEqual({
      profileId: "server",
      method: "progress/update",
      params: { percent: 50 },
    });
  });

  it("handles interleaved notifications and responses", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const notifications: Array<{ profileId: string; method: string; params: unknown }> = [];
    router.on("notification", (n) => notifications.push(n));

    const promise = router.route("ssh://server/repo", "hydrate", { files: ["*"] });

    // Notification arrives before response
    simulateNotification(channel, "progress/update", { percent: 25 });
    simulateNotification(channel, "progress/update", { percent: 75 });
    simulateResponse(channel, 1, { hydrated: 10 });

    const result = await promise;
    expect(result).toEqual({ hydrated: 10 });
    expect(notifications).toHaveLength(2);
  });
});

describe("RpcRouter — batch RPC", () => {
  it("sends independent NDJSON requests and returns ordered results", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const calls = [
      { method: "repo_summary", params: {} },
      { method: "list_refs", params: {} },
      { method: "git_status_porcelain", params: {} },
    ];

    const promise = router.batchRpc("ssh://server/repo", calls);

    // Each NDJSON line must be accepted by the sidecar request dispatcher.
    expect(channel.written.map(frame => JSON.parse(frame).method)).toEqual(calls.map(call => call.method));

    // Simulate individual responses (may arrive out of order)
    simulateResponse(channel, 2, { branches: ["main"] });
    simulateResponse(channel, 1, { name: "my-repo" });
    simulateResponse(channel, 3, "M file.txt");

    const results = await promise;
    expect(results).toHaveLength(3);
    expect(results[0]).toEqual({ result: { name: "my-repo" } });
    expect(results[1]).toEqual({ result: { branches: ["main"] } });
    expect(results[2]).toEqual({ result: "M file.txt" });
  });

  it("handles errors in batch responses", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const calls = [
      { method: "repo_summary", params: {} },
      { method: "bad_method", params: {} },
    ];

    const promise = router.batchRpc("ssh://server/repo", calls);

    simulateResponse(channel, 1, { name: "repo" });
    simulateErrorResponse(channel, 2, -32601, "Method not found");

    const results = await promise;
    expect(results[0]).toEqual({ result: { name: "repo" } });
    expect(results[1]).toEqual({ error: { code: -32601, message: "Method not found" } });
  });

  it("routes local calls through the local agent", async () => {
    const la = new MockLocalAgent();
    la.nextResult = "local-result";
    const { router } = createRouter(undefined, la);

    const calls = [
      { method: "method1", params: { a: 1 } },
      { method: "method2", params: { b: 2 } },
    ];

    const results = await router.batchRpc("/local/repo", calls);
    expect(results).toHaveLength(2);
    expect(results[0]).toEqual({ result: "local-result" });
    expect(results[1]).toEqual({ result: "local-result" });
    expect(la.calls).toHaveLength(2);
  });
});

describe("RpcRouter — error responses", () => {
  it("rejects with error details from remote agent", async () => {
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("server", "connected");
    sm.setConnection("server", channel);

    const { router } = createRouter(sm);
    router.attachChannel("server", channel.asChannel());

    const promise = router.route("ssh://server/repo", "git_push", {});
    simulateErrorResponse(channel, 1, -32000, "push rejected: non-fast-forward");

    await expect(promise).rejects.toThrow("push rejected: non-fast-forward");
  });
});

describe("routeRpcCall — integration helper", () => {
  it("routes to local agent when no ssh:// prefix in params", async () => {
    const la = new MockLocalAgent();
    la.nextResult = "local";
    const sm = new MockSessionManager();
    const router = new RpcRouter(sm as any, la);

    const result = await routeRpcCall(router, la, "git_status", { repo_id: "/local/path" });
    expect(result).toBe("local");
  });

  it("routes to remote agent when ssh:// prefix in params", async () => {
    const la = new MockLocalAgent();
    const sm = new MockSessionManager();
    const channel = new MockChannel();
    sm.setStatus("remote-1", "connected");
    sm.setConnection("remote-1", channel);

    const router = new RpcRouter(sm as any, la);
    router.attachChannel("remote-1", channel.asChannel());

    const promise = routeRpcCall(router, la, "git_status", { repo_id: "ssh://remote-1/repo" });
    simulateResponse(channel, 1, "remote-status");

    const result = await promise;
    expect(result).toBe("remote-status");
    expect(la.calls).toHaveLength(0); // Local agent not called
  });

  it("routes to local agent when params have no repo_id", async () => {
    const la = new MockLocalAgent();
    la.nextResult = "pong";
    const sm = new MockSessionManager();
    const router = new RpcRouter(sm as any, la);

    const result = await routeRpcCall(router, la, "ping", {});
    expect(result).toBe("pong");
  });
});
