import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import type { RemoteEndpoint } from "../electron/ssh-types";

class Channel extends EventEmitter {
  writes: string[] = [];
  write(frame: string, done?: (error?: Error | null) => void): boolean { this.writes.push(frame); done?.(); return true; }
  asChannel(): ClientChannel { return this as unknown as ClientChannel; }
  reply(id: number, result: unknown): void { this.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n")); }
}

function fixture(timeoutMs = 30_000, status = "connected") {
  const channels = new Map<string, Channel>();
  const endpoints = new Map<string, RemoteEndpoint>();
  const manager = {
    getStatus: () => status,
    getConnection: (profile: string) => ({ endpoint: endpoints.get(profile), channels: { agent: channels.get(profile)?.asChannel() }, profile: { operationTimeoutMs: timeoutMs } }),
  } as unknown as SshSessionManager;
  const router = new RpcRouter(manager, { request: async () => { throw new Error("Unexpected local request"); } });
  const attach = (profile: string) => {
    const channel = new Channel(); channels.set(profile, channel); router.attachChannel(profile, channel.asChannel()); return channel;
  };
  return { router, attach, endpoints };
}

describe("RPC channel lifecycle", () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => { vi.clearAllTimers(); vi.useRealTimers(); });

  it.each([
    { field: "host", value: "other.example.invalid" }, { field: "port", value: 2222 },
    { field: "username", value: "another-account" }, { field: "hostKey", value: "SHA256:replacement" },
  ])("rejects a changed verified $field before issuing a task request", ({ field, value }) => {
    const { router, attach, endpoints } = fixture(); const channel = attach("profile");
    const original = { host: "fixture.example.invalid", port: 22, username: "fixture", hostKey: "SHA256:original" };
    endpoints.set("profile", original);
    const saved = router.remoteEndpoint("profile");
    endpoints.set("profile", { ...original, [field]: value });
    expect(() => router.assertRemoteEndpoint("profile", saved)).toThrow("endpoint changed");
    expect(channel.writes).toEqual([]);
  });

  it("does not label a replacement channel with another connection's endpoint", () => {
    const { router, attach, endpoints } = fixture(); attach("profile");
    endpoints.set("profile", { host: "fixture.example.invalid", port: 22, username: "fixture", hostKey: "SHA256:original" });
    router.attachChannel("profile", new Channel().asChannel());
    expect(() => router.remoteEndpoint("profile")).toThrow("authenticated SSH agent");
  });

  it.each(["end", "close", "error"])("settles only the owning calls immediately on %s", async event => {
    const { router, attach } = fixture();
    const first = attach("first"); const second = attach("second");
    let state = "pending";
    void router.sendToRemoteAgent("first", "read", {}).then(() => { state = "success"; }, () => { state = "failed"; });
    const sibling = router.sendToRemoteAgent("second", "read", {});
    first.emit(event, new Error("Connection lost"));
    second.reply(2, "sibling result");
    await vi.advanceTimersByTimeAsync(0);
    expect({ state, sibling: await sibling, timers: vi.getTimerCount() }).toEqual({ state: "failed", sibling: "sibling result", timers: 0 });
  });

  it("rejects replaced calls and keeps the replacement's partial frame after a late close", async () => {
    const { router, attach } = fixture(); const old = attach("profile");
    let oldState = "pending"; let value: unknown;
    void router.sendToRemoteAgent("profile", "read", {}).then(() => { oldState = "success"; }, () => { oldState = "failed"; });
    const current = attach("profile");
    void router.sendToRemoteAgent("profile", "read", {}).then(result => { value = result; });
    const frame = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: 2, result: "current" }) + "\n");
    current.emit("data", frame.subarray(0, 12)); old.emit("close"); current.emit("data", frame.subarray(12));
    await vi.advanceTimersByTimeAsync(0);
    expect({ oldState, value, timers: vi.getTimerCount() }).toEqual({ oldState: "failed", value: "current", timers: 0 });
  });

  it("attaches the same channel once without erasing buffered data", async () => {
    const { router, attach } = fixture(); const channel = attach("profile");
    let value: unknown; const notices = vi.fn(); router.on("notification", notices);
    void router.sendToRemoteAgent("profile", "read", {}).then(result => { value = result; });
    channel.emit("data", Buffer.from('{"jsonrpc":"2.0","id":1,"result":'));
    router.attachChannel("profile", channel.asChannel());
    channel.emit("data", Buffer.from('"complete"}\n{"jsonrpc":"2.0","method":"update","params":{}}\n'));
    await vi.advanceTimersByTimeAsync(0);
    expect({ value, notices: notices.mock.calls.length, timers: vi.getTimerCount() }).toEqual({ value: "complete", notices: 1, timers: 0 });
  });

  it("does not let a different profile answer another channel's request", async () => {
    const { router, attach } = fixture(); const first = attach("first"); const second = attach("second");
    let value: unknown;
    void router.sendToRemoteAgent("first", "read", {}).then(result => { value = result; });
    second.reply(1, "wrong owner"); await vi.advanceTimersByTimeAsync(0);
    expect(value).toBeUndefined();
    first.reply(1, "correct owner"); await vi.advanceTimersByTimeAsync(0);
    expect(value).toBe("correct owner");
  });

  it("preserves UTF-8 when an SSH packet splits a codepoint", async () => {
    const { router, attach } = fixture(); const channel = attach("profile");
    const result = router.sendToRemoteAgent("profile", "read", {});
    const frame = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: 1, result: "雪" }) + "\n");
    const split = frame.indexOf(Buffer.from("雪")) + 1;
    channel.emit("data", frame.subarray(0, split)); channel.emit("data", frame.subarray(split));
    expect(await result).toBe("雪");
  });

  it("settles a partially answered batch on closure without losing successful peers", async () => {
    const { router, attach } = fixture(); const channel = attach("profile");
    const results = router.batchRpc("ssh://profile/repo", [{ method: "one", params: {} }, { method: "two", params: {} }]);
    channel.reply(1, "first"); channel.emit("close"); await vi.advanceTimersByTimeAsync(0);
    expect({ results: await results, timers: vi.getTimerCount() }).toEqual({ results: [{ result: "first" }, { error: { code: -32000, message: expect.stringContaining("channel closed") } }], timers: 0 });
  });

  it("uses the configured profile deadline for batches too", async () => {
    const { router, attach } = fixture(25); attach("profile");
    const results = router.batchRpc("ssh://profile/repo", [{ method: "read", params: {} }]);
    await vi.advanceTimersByTimeAsync(25);
    expect({ results: await results, timers: vi.getTimerCount() }).toEqual({ results: [{ error: { code: -32000, message: expect.stringContaining("25ms") } }], timers: 0 });
  });

  it("does not send an empty batch or allocate a deadline", async () => {
    const { router, attach } = fixture(); const channel = attach("profile"); let value: unknown;
    void router.batchRpc("ssh://profile/repo", []).then(result => { value = result; });
    await vi.advanceTimersByTimeAsync(0);
    expect({ value, writes: channel.writes, timers: vi.getTimerCount() }).toEqual({ value: [], writes: [], timers: 0 });
  });

  it("releases its deadline when writing the frame throws", async () => {
    const { router, attach } = fixture(); const channel = attach("profile");
    vi.spyOn(channel, "write").mockImplementation(() => { throw new Error("Write failed"); });
    await expect(router.sendToRemoteAgent("profile", "read", {})).rejects.toThrow("Write failed");
    expect(vi.getTimerCount()).toBe(0);
  });
  it("routes a registered remote batch through its channel instead of the local agent", async () => {
    const { router, attach } = fixture(); const channel = attach("profile");
    router.registerTransport("registered-repo", { type: "remote", profileId: "profile" });
    const results = router.batchRpc("registered-repo", [{ method: "read", params: {} }]);
    channel.reply(1, "remote");
    expect(await results).toEqual([{ result: "remote" }]);
  });

  it("applies reconnect write admission to batched calls", async () => {
    const manager = { getStatus: () => "reconnecting" } as unknown as SshSessionManager;
    const router = new RpcRouter(manager, { request: vi.fn() }); const channel = new Channel();
    router.attachChannel("profile", channel.asChannel());
    const results = await router.batchRpc("ssh://profile/repo", [{ method: "git_commit", params: {} }]);
    expect({ results, writes: channel.writes, timers: vi.getTimerCount() }).toEqual({ results: [{ error: { code: -32000, message: expect.stringContaining("unavailable during reconnection") } }], writes: [], timers: 0 });
  });

  it("preserves remote error code and structured data in a batched call", async () => {
    const { router, attach } = fixture(); const channel = attach("profile");
    const results = router.batchRpc("ssh://profile/repo", [{ method: "read", params: {} }]);
    const error = { code: 1002, message: "Git failed", data: { status: 128, stderr: "missing base" } };
    channel.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: 1, error }) + "\n"));
    expect(await results).toEqual([{ error }]);
  });

  it.each(["git_stash_apply", "task_spawn", "task_apply", "write_text", "unknown_operation"])("never defers %s through any remote entry point", async method => {
    const { router, attach } = fixture(30_000, "reconnecting"); const channel = attach("profile");
    const outcomes: string[] = [];
    const failed = (reason: unknown) => { outcomes.push((reason as Error & { code?: string }).code ?? "other-error"); };
    void router.route("ssh://profile/repo", method, {}).then(() => outcomes.push("routed"), failed);
    void router.sendToRemoteAgent("profile", method, {}).then(() => outcomes.push("sent"), failed);
    void router.batchRpc("ssh://profile/repo", [{ method, params: {} }]).then(result => outcomes.push(result[0].error ? "batch-rejected" : "batch-sent"));
    try {
      await vi.advanceTimersByTimeAsync(0);
      expect({ outcomes: outcomes.sort(), writes: channel.writes }).toEqual({ outcomes: ["SSH_RECONNECTING", "SSH_RECONNECTING", "batch-rejected"].sort(), writes: [] });
    } finally {
      router.clearQueue("profile"); channel.emit("close");
    }
  });

});
