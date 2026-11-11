import { mockStorageConnections } from "./mock-storage-connections";
import { EventEmitter } from "node:events";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { RemoteCache } from "../electron/ssh-cache";
import type { StorageConnections } from "../electron/storage-connections";

class Channel extends EventEmitter {
  frames: Array<{ id: number; method: string; params: Record<string, unknown> }> = [];
  end = vi.fn();
  write(text: string, callback?: (error?: Error | null) => void) {
    this.frames.push(JSON.parse(text));
    callback?.();
    return true;
  }
  reply(id: number, body: Record<string, unknown>) {
    this.emit("data", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, ...body }) + "\n"));
  }
  asChannel() { return this as unknown as ClientChannel; }
}

function fixture(timeout?: number, storage: Pick<StorageConnections, "request" | "redactValue"> = mockStorageConnections) {
  const channels = new Map<string, Channel>();
  const statuses = new Map<string, string>();
  const work = new Map<string, Promise<unknown>>();
  let nativeSession = "current";
  const manager = { repositoryWork: (repoId: string) => work.get(repoId),
    getStatus: (id: string) => statuses.get(id) ?? "connected",
    getConnection: (id: string) => ({ channels: { agent: channels.get(id)?.asChannel() } }),
    repositoryRuntime: (_id: string, repoId: string) => ({ repoId, sessionId: nativeSession }),
  };
  const router = new RpcRouter(manager as unknown as SshSessionManager, { request: async () => ({}) }, storage, { requestTimeoutMs: timeout });
  const attach = (profile: string) => {
    const channel = new Channel();
    channels.set(profile, channel);
    router.attachChannel(profile, channel.asChannel());
    return channel;
  };
  return { router, attach, setSession: (session: string) => { nativeSession = session; }, setStatus: (profile: string, status: string) => statuses.set(profile, status), setWork: (repoId: string, pending: Promise<unknown>) => work.set(repoId, pending) };
}

function observe(promise: Promise<unknown>) {
  const state: { settled: boolean; value?: unknown; error?: Error } = { settled: false };
  void promise.then(value => Object.assign(state, { settled: true, value }), error => Object.assign(state, { settled: true, error }));
  return state;
}

afterEach(() => { vi.clearAllTimers(); vi.useRealTimers(); });

describe("SSH captured source ownership", () => {
  it("queued metadata for a healthy repository bypasses blocked sibling setup and cancellation drains unsent work", async () => {
    const { router, attach, setStatus, setWork } = fixture();
    const slow = "ssh://one/slow", fast = "ssh://one/fast";
    let release!: () => void;
    const opening = new Promise<void>(resolve => { release = resolve; });
    setWork(slow, opening); setStatus("one", "reconnecting");
    const blocked = observe(router.route(slow, "git_log", { repo_id: slow, op_id: "blocked" }));
    const healthy = observe(router.route(fast, "repo_summary", { repo_id: fast }));
    const channel = attach("one"); setStatus("one", "connected");
    const replay = router.replayQueue("one");
    try {
      await vi.waitFor(() => expect(channel.frames.some(frame => frame.params.repo_id === fast)).toBe(true));
      const sent = channel.frames.find(frame => frame.params.repo_id === fast)!;
      channel.reply(sent.id, { result: { healthy: true } });
      await vi.waitFor(() => expect(healthy.value).toEqual({ healthy: true }));
      expect(channel.frames.some(frame => frame.params.repo_id === slow)).toBe(false);
      await expect(router.route(slow, "rpc/cancel", { repo_id: slow, op_id: "blocked" })).resolves.toEqual({ ok: true });
      await vi.waitFor(() => expect(blocked.error).toMatchObject({ code: 1010 }));
      await replay;
    } finally { release(); channel.emit("end"); router.clearQueue("one"); await replay; }
  });

  it("queued repositories progress independently while reads within one repository keep their order", async () => {
    const { router, attach, setStatus } = fixture();
    const slow = "ssh://one/slow", fast = "ssh://one/fast";
    setStatus("one", "reconnecting");
    const first = observe(router.route(slow, "git_log", { repo_id: slow, op_id: "first" }));
    const second = observe(router.route(slow, "list_refs", { repo_id: slow, op_id: "second" }));
    const other = observe(router.route(fast, "repo_summary", { repo_id: fast }));
    const channel = attach("one"); setStatus("one", "connected");
    const replay = router.replayQueue("one");
    try {
      await vi.waitFor(() => expect(channel.frames).toHaveLength(2));
      expect(channel.frames.some(frame => frame.params.op_id === "second")).toBe(false);
      const sent = channel.frames.find(frame => frame.params.repo_id === fast)!;
      channel.reply(sent.id, { result: { healthy: true } });
      await vi.waitFor(() => expect(other.settled).toBe(true));
      await router.route(slow, "rpc/cancel", { repo_id: slow, op_id: "second" });
      await vi.waitFor(() => expect(second.error).toMatchObject({ code: 1010 }));
      const initial = channel.frames.find(frame => frame.params.op_id === "first")!;
      channel.reply(initial.id, { result: { commits: [] } });
      await replay;
      expect(first.value).toEqual({ commits: [] });
    } finally { channel.emit("end"); router.clearQueue("one"); await replay; }
  });

  it("credential preparation cannot move a request onto a replacement SSH channel", async () => {
    let release!: () => void;
    const preparation = new Promise<void>(resolve => { release = resolve; });
    const storage: Pick<StorageConnections, "request" | "redactValue"> = {
      ...mockStorageConnections,
      request: async (method, params, send) => { await preparation; return send(method, params); },
    };
    const { router, attach } = fixture(undefined, storage);
    const old = attach("one");
    const requested = router.sendToRemoteAgent("one", "open_local", { repo_id: "ssh://one/repo", path: "/repo" });
    const result = observe(requested);
    old.emit("end");
    const replacement = attach("one");
    release();
    try {
      await vi.waitFor(() => expect(result.settled).toBe(true));
      expect(result.error).toMatchObject({ code: "SSH_CHANNEL_CLOSED" });
      expect(replacement.frames).toHaveLength(0);
    } finally { replacement.emit("end"); await requested.catch(() => {}); }
  });

  it("cancelled replay authentication never dispatches when preparation finishes late", async () => {
    let release!: () => void;
    let preparing = false;
    const preparation = new Promise<void>(resolve => { release = resolve; });
    const storage: Pick<StorageConnections, "request" | "redactValue"> = {
      ...mockStorageConnections,
      request: async (method, params, send) => { preparing = true; await preparation; return send(method, params); },
    };
    const { router, attach, setStatus } = fixture(undefined, storage);
    const repo_id = "ssh://one/repo";
    setStatus("one", "reconnecting");
    const result = observe(router.route(repo_id, "git_log", { repo_id, op_id: "preparing" }));
    const channel = attach("one"); setStatus("one", "connected");
    const replay = router.replayQueue("one");
    try {
      await vi.waitFor(() => expect(preparing).toBe(true));
      await router.route(repo_id, "rpc/cancel", { repo_id, op_id: "preparing" });
      await replay;
      expect(result.error).toMatchObject({ code: 1010 });
      release();
      await new Promise<void>(resolve => setImmediate(resolve));
      expect(channel.frames).toHaveLength(0);
    } finally { release(); channel.emit("end"); router.clearQueue("one"); await replay; }
  });

  it("a lost replay releases blocked admission without consuming the replacement channel's queue", async () => {
    const { router, attach, setStatus, setWork } = fixture();
    const slow = "ssh://one/slow", fast = "ssh://one/fast";
    let release!: () => void;
    setWork(slow, new Promise<void>(resolve => { release = resolve; }));
    setStatus("one", "reconnecting");
    const oldResult = observe(router.route(slow, "git_log", { repo_id: slow }));
    const old = attach("one"); setStatus("one", "connected");
    const firstReplay = router.replayQueue("one");
    await new Promise<void>(resolve => setImmediate(resolve));
    old.emit("end"); setStatus("one", "reconnecting");
    const newResult = observe(router.route(fast, "repo_summary", { repo_id: fast }));
    const current = attach("one"); setStatus("one", "connected");
    const secondReplay = router.replayQueue("one");
    try {
      await vi.waitFor(() => expect(oldResult.error).toMatchObject({ code: "SSH_CHANNEL_CLOSED" }));
      await vi.waitFor(() => expect(current.frames).toHaveLength(1));
      expect(current.frames[0].params.repo_id).toBe(fast);
      current.reply(current.frames[0].id, { result: { healthy: true } });
      await Promise.all([firstReplay, secondReplay]);
      expect(newResult.value).toEqual({ healthy: true });
      release();
      await new Promise<void>(resolve => setImmediate(resolve));
      expect(current.frames).toHaveLength(1);
    } finally { release(); current.emit("end"); router.clearQueue("one"); await Promise.all([firstReplay, secondReplay]); }
  });

  it("replay bounds native concurrency and isolates a failed response from waiting siblings", async () => {
    const { router, attach, setStatus } = fixture();
    setStatus("one", "reconnecting");
    const repositories = Array.from({ length: 6 }, (_, index) => `ssh://one/repo-${index}`);
    const results = repositories.map((repo_id, index) => observe(router.route(repo_id, "repo_summary", { repo_id, op_id: `read-${index}` })));
    const channel = attach("one"); setStatus("one", "connected");
    const replay = router.replayQueue("one");
    try {
      await vi.waitFor(() => expect(channel.frames).toHaveLength(4));
      await router.route(repositories[4], "rpc/cancel", { repo_id: repositories[4], op_id: "read-4" });
      channel.reply(channel.frames[0].id, { error: { code: 500, message: "Metadata unavailable" } });
      await vi.waitFor(() => expect(channel.frames).toHaveLength(5));
      expect(channel.frames[4].params.repo_id).toBe(repositories[5]);
      for (const frame of channel.frames.slice(1)) channel.reply(frame.id, { result: { repo_id: frame.params.repo_id } });
      await replay;
      expect(results[0].error).toMatchObject({ code: 500 });
      expect(results[4].error).toMatchObject({ code: 1010 });
      for (const index of [1, 2, 3, 5]) expect(results[index].value).toEqual({ repo_id: repositories[index] });
    } finally { channel.emit("end"); router.clearQueue("one"); await replay; }
  });

  it("validates the expected native identity at the wire boundary and strips it from native params", async () => {
    const { router, attach } = fixture();
    const channel = attach("one");
    const params = { repo_id: "ssh://one/repo", file_id: "captured", offset: 0, length: 1 };
    for (const expected of ["old", null, undefined]) {
      await expect(router.sendToRemoteAgent("one", "read_range", { ...params, native_session_id: expected })).rejects.toMatchObject({ code: "NATIVE_SESSION_LOST" });
    }
    await expect(router.sendToRemoteAgent("one", "file_close", { ...params, native_session_id: "old" })).resolves.toEqual({ closed: "captured" });
    expect(channel.frames).toHaveLength(0);
    const read = router.sendToRemoteAgent("one", "read_range", { ...params, native_session_id: "current" });
    expect(channel.frames[0].params).not.toHaveProperty("native_session_id");
    channel.reply(channel.frames[0].id, { result: { bytes_b64: "YQ==" } });
    await expect(read).resolves.toEqual({ bytes_b64: "YQ==" });
  });

  it.each(["read_text", "write_text"])("%s cannot cross repository replacement during credential preparation", async method => {
    let release!: () => void;
    const preparation = new Promise<void>(resolve => { release = resolve; });
    const storage: Pick<StorageConnections, "request" | "redactValue"> = {
      ...mockStorageConnections,
      request: async (name, params, send) => { await preparation; return send(name, params); },
    };
    const { router, attach, setSession } = fixture(undefined, storage);
    const channel = attach("one");
    const requested = router.route("ssh://one/repo", method, { repo_id: "ssh://one/repo", path: "settings.json", content: "pending", native_session_id: "current" });
    const rejected = expect(requested).rejects.toMatchObject({ code: "NATIVE_SESSION_LOST" });
    setSession("replacement"); release();
    try { await rejected; expect(channel.frames).toHaveLength(0); }
    finally { release(); channel.emit("end"); }
  });
  it("does not freshen stale metadata when a reconnect prefetch uses cache fallback", async () => {
    vi.useFakeTimers();
    const { router, setStatus } = fixture();
    const cache = new RemoteCache();
    router.setCache(cache);
    setStatus("one", "reconnecting");
    const repo = "ssh://one/repo";
    for (const method of ["repo_summary", "list_refs", "git_status_porcelain", "hydration_status"]) {
      await cache.getOrFetch(`${repo}:${method}`, async () => ({ old: true }), 100);
    }
    await vi.advanceTimersByTimeAsync(101);
    await cache.prefetch(router, repo);
    expect(await cache.getOrFetch(`${repo}:repo_summary`, async () => ({ fresh: true }))).toEqual({ fresh: true });
  });
  it.each([1010, 1012, null])("explicit timeout cancels and waits for terminal reply %s", async code => {
    vi.useFakeTimers();
    const { router, attach } = fixture(20);
    const channel = attach("one");
    const result = observe(router.route("ssh://one/repo", "file_open", { repo_id: "ssh://one/repo", path: "large.bin", op_id: "owned" }));
    await vi.advanceTimersByTimeAsync(21);
    expect(channel.frames[1]).toMatchObject({ method: "rpc/cancel", params: { repo_id: "ssh://one/repo", op_id: "owned" } });
    channel.reply(channel.frames[1].id, { result: { ok: true } });
    await vi.advanceTimersByTimeAsync(0);
    expect(result.settled).toBe(false);
    channel.reply(channel.frames[0].id, code === null ? { result: { file_id: "finished" } } : { error: { code, message: code === 1010 ? "Cancelled" : "Integrity failure", data: { kind: code === 1010 ? "op_cancelled" : "content_read" } } });
    await vi.advanceTimersByTimeAsync(0);
    if (code === null) expect(result.value).toEqual({ file_id: "finished" });
    else expect(result.error).toMatchObject(code === 1010 ? { code: "SSH_RPC_TIMEOUT" } : { code: 1012, message: "Integrity failure" });
  });

  it("lets native-owned content operations outlive the metadata timeout by default", async () => {
    vi.useFakeTimers();
    const { router, attach } = fixture();
    const channel = attach("one");
    const result = observe(router.route("ssh://one/repo", "read_bytes", { repo_id: "ssh://one/repo", path: "large.bin" }));
    await vi.advanceTimersByTimeAsync(31_000);
    expect(result.settled).toBe(false);
    expect(channel.frames[0].params.op_id).toEqual(expect.any(String));
    channel.reply(channel.frames[0].id, { result: { bytes_b64: "YWJj" } });
    await vi.advanceTimersByTimeAsync(0);
    expect(result.value).toEqual({ bytes_b64: "YWJj" });
  });

  it.each(["open_local", "open_url"])("%s retains its owner beyond the metadata timeout", async method => {
    vi.useFakeTimers();
    const { router, attach } = fixture();
    const channel = attach("one");
    const repo_id = "ssh://one/repo";
    const result = observe(router.sendToRemoteAgent("one", method, { repo_id, path: "/repo", url: "s3://bucket/repo" }));
    await vi.advanceTimersByTimeAsync(31_000);
    const beforeReply = result.settled;
    channel.reply(channel.frames[0].id, { result: { repo_id } });
    await vi.advanceTimersByTimeAsync(0);
    expect(beforeReply).toBe(false);
    expect(result.value).toEqual({ repo_id });
    expect(channel.frames[0].params.op_id).toEqual(expect.any(String));
  });

  it.each(["open_local", "open_url"])("%s deadline signals cancellation and awaits its terminal open", async method => {
    vi.useFakeTimers();
    const { router, attach } = fixture(20);
    const channel = attach("one");
    const repo_id = "ssh://one/repo";
    const result = observe(router.sendToRemoteAgent("one", method, { repo_id, path: "/repo", url: "s3://bucket/repo" }));
    await vi.advanceTimersByTimeAsync(21);
    const signalled = channel.frames.find(frame => frame.method === "rpc/cancel");
    const beforeReply = result.settled;
    if (signalled) channel.reply(signalled.id, { result: { ok: true } });
    channel.reply(channel.frames[0].id, { error: { code: 1010, message: "Opening cancelled" } });
    await vi.advanceTimersByTimeAsync(0);
    expect(signalled?.params).toMatchObject({ repo_id, op_id: channel.frames[0].params.op_id });
    expect(beforeReply).toBe(false);
    expect(result.error).toMatchObject({ code: "SSH_RPC_TIMEOUT" });
  });

  it("preserves UTF-8 when every wire byte arrives separately", async () => {
    const { router, attach } = fixture();
    const channel = attach("one");
    const result = router.route("ssh://one/repo", "read_text", { repo_id: "ssh://one/repo", path: "你好.ipynb" });
    const text = "こんにちは 🦀 中文";
    const bytes = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: channel.frames[0].id, result: { text } }) + "\n");
    for (const byte of bytes) channel.emit("data", Buffer.from([byte]));
    expect(await result).toEqual({ text });
  });

  it("does not accept another profile's response for an owned request", async () => {
    vi.useFakeTimers();
    const { router, attach } = fixture();
    const one = attach("one");
    const two = attach("two");
    const result = observe(router.route("ssh://one/repo", "file_open", { repo_id: "ssh://one/repo", path: "file" }));
    two.reply(one.frames[0].id, { result: { file_id: "wrong-profile" } });
    await vi.advanceTimersByTimeAsync(0);
    expect(result.settled).toBe(false);
    one.reply(one.frames[0].id, { result: { file_id: "right-profile" } });
    await vi.advanceTimersByTimeAsync(0);
    expect(result.value).toEqual({ file_id: "right-profile" });
  });

  it("settles only the closed channel's requests and keeps sibling profiles usable", async () => {
    vi.useFakeTimers();
    const { router, attach } = fixture();
    const one = attach("one");
    const two = attach("two");
    const first = observe(router.route("ssh://one/repo", "file_open", { repo_id: "ssh://one/repo", path: "one" }));
    const second = observe(router.route("ssh://two/repo", "file_open", { repo_id: "ssh://two/repo", path: "two" }));
    one.emit("close");
    await vi.advanceTimersByTimeAsync(0);
    expect(first.error).toMatchObject({ code: "SSH_CHANNEL_CLOSED" });
    expect(second.settled).toBe(false);
    two.reply(two.frames[0].id, { result: { file_id: "two" } });
    await vi.advanceTimersByTimeAsync(0);
    expect(second.value).toEqual({ file_id: "two" });
  });
  it.each(["cancel", "disconnect"])("%s removes an unsent call during queue replay", async action => {
    const { router, attach, setStatus } = fixture();
    const channel = attach("one");
    const repo_id = "ssh://one/repo";
    setStatus("one", "reconnecting");
    const first = router.route(repo_id, "repo_summary", { repo_id });
    const second = router.route(repo_id, "list_refs", { repo_id, op_id: "unsent" });
    const rejected = expect(second).rejects.toThrow(action === "cancel" ? "Queued operation cancelled" : "Connection lost permanently");
    setStatus("one", "connected");
    const replay = router.replayQueue("one");
    await vi.waitFor(() => expect(channel.frames).toHaveLength(1));
    if (action === "cancel") await router.route(repo_id, "rpc/cancel", { repo_id, op_id: "unsent" });
    else router.clearQueue("one");
    await rejected;
    channel.reply(channel.frames[0].id, { result: { done: true } });
    await Promise.all([first, replay]);
    expect(channel.frames.map(frame => frame.method)).toEqual(["repo_summary"]);
  });

  it("ignores old channel frames and close events after replacement", async () => {
    const { router, attach } = fixture();
    const old = attach("one");
    const pending = router.route("ssh://one/repo", "file_open", { path: "old" });
    const rejected = expect(pending).rejects.toMatchObject({ code: "SSH_CHANNEL_REPLACED" });
    const current = attach("one");
    await rejected;
    const result = router.route("ssh://one/repo", "file_open", { path: "current" });
    const frame = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: current.frames[0].id, result: { file_id: "current" } }) + "\n");
    current.emit("data", frame.subarray(0, 10));
    old.reply(current.frames[0].id, { result: { file_id: "wrong" } });
    old.emit("close");
    current.emit("data", frame.subarray(10));
    expect(await result).toEqual({ file_id: "current" });
  });

  it("rejects invalid wire UTF-8 instead of silently substituting bytes", async () => {
    const { router, attach } = fixture();
    const channel = attach("one");
    const result = router.route("ssh://one/repo", "read_text", { path: "text" });
    const rejected = expect(result).rejects.toMatchObject({ code: "SSH_PROTOCOL_ERROR" });
    channel.emit("data", Buffer.from([0xc3, 0x28, 10]));
    await rejected;
    expect(channel.end).toHaveBeenCalledOnce();
  });

  it("limits a batch to four native NDJSON calls and keeps input order", async () => {
    const { router, attach } = fixture();
    const channel = attach("one");
    expect(await router.batchRpc("ssh://one/repo", [])).toEqual([]);
    const result = router.batchRpc("ssh://one/repo", Array.from({ length: 6 }, (_, i) => ({ method: "repo_summary", params: { i } })));
    expect(channel.frames).toHaveLength(4);
    channel.reply(channel.frames[3].id, { result: 3 });
    channel.reply(channel.frames[0].id, { result: 0 });
    await vi.waitFor(() => expect(channel.frames).toHaveLength(6));
    for (const i of [5, 4, 2, 1]) channel.reply(channel.frames[i].id, { result: i });
    expect(await result).toEqual(Array.from({ length: 6 }, (_, result) => ({ result })));
  });

});
