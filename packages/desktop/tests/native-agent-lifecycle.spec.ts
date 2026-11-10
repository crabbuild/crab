import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough, Writable } from "node:stream";
import type { ChildProcessWithoutNullStreams } from "node:child_process";

interface Frame { id: string; method: string; params: Record<string, unknown> }

class NativeProcess extends EventEmitter {
  frames: Frame[] = [];
  repositories = new Set<string>();
  stdout = new PassThrough();
  stderr = new PassThrough();
  exitCode: number | null = null;
  signalCode: NodeJS.Signals | null = null;
  holdPing = false;
  holdOpen: string | undefined;
  holdClose = false;
  methods = ["capabilities", "ping", "open_local", "open_url", "close", "read_text", "file_open", "file_materialize", "file_close", "hydrate", "dehydrate"];
  failedRepository: string | undefined;
  ended = false;
  stdin = new Writable({ write: (bytes, _encoding, done) => {
    const frame = JSON.parse(bytes.toString()) as Frame;
    this.frames.push(frame);
    queueMicrotask(() => this.answer(frame));
    done();
  } });

  constructor() {
    super();
    this.stdin.on("finish", () => { if (!this.ended && !this.holdClose) { this.exit(0, null); this.close(); } });
  }

  answer(frame: Frame) {
    if (this.ended || (frame.method === "ping" && this.holdPing)) return;
    let result: unknown = {};
    if (frame.method === "ping") result = { pong: "test" };
    if (frame.method === "capabilities") result = { protocol: 1, version: "test", methods: this.methods };
    if (frame.method === "open_local" || frame.method === "open_url") {
      if (frame.params.repo_id === this.holdOpen) return;
      if (frame.params.repo_id === this.failedRepository) {
        this.reply(frame.id, undefined, "Repository was removed"); return;
      }
      this.repositories.add(String(frame.params.repo_id));
      result = { repo_id: frame.params.repo_id, url: frame.params.path ?? frame.params.url, is_remote: frame.method === "open_url", cache_dir: "" };
    }
    if (frame.method === "close") this.repositories.delete(String(frame.params.repo_id));
    if (frame.method === "read_text") {
      if (!this.repositories.has(String(frame.params.repo_id))) { this.reply(frame.id, undefined, "Repository is not open"); return; }
      result = { content: "Recovered native content" };
    }
    this.reply(frame.id, result);
  }

  reply(id: string, result: unknown, message?: string) {
    this.stdout.write(JSON.stringify(message ? { jsonrpc: "2.0", id, error: { code: -32000, message } } : { jsonrpc: "2.0", id, result }) + "\n");
  }

  exit(code: number | null, signal: NodeJS.Signals | null) {
    this.ended = true; this.exitCode = code; this.signalCode = signal;
    this.emit("exit", code, signal);
  }

  close() { this.stdout.end(); this.stderr.end(); this.emit("close", this.exitCode, this.signalCode); }
  kill(signal: NodeJS.Signals = "SIGTERM") { this.exit(null, signal); this.close(); return true; }
}

const children: NativeProcess[] = [];
let configure: (child: NativeProcess) => void;
vi.mock("node:child_process", () => ({ spawn: () => {
  const child = new NativeProcess(); configure?.(child); children.push(child);
  return child as unknown as ChildProcessWithoutNullStreams;
} }));
vi.mock("electron", () => ({ app: { getPath: () => "/native-test-profile", quit: vi.fn() }, dialog: { showMessageBox: vi.fn(async () => ({ response: 0 })) } }));
vi.mock("node:fs", () => ({ existsSync: () => true, mkdirSync: vi.fn(), writeFileSync: vi.fn() }));

import { Agent } from "../electron/agent";
import { LocalProvider } from "../electron/local-provider";

describe("native agent process ownership", () => {
  let agent: Agent;
  beforeEach(() => { children.length = 0; configure = () => {}; agent = new Agent("fixture"); });
  afterEach(async () => {
    const stopped = agent.shutdown();
    for (const child of children) { if (!child.ended) child.exit(0, null); child.close(); }
    await stopped;
  });

  it("concurrent starts join one process", async () => {
    await Promise.all([agent.start(), agent.start()]);
    expect(children).toHaveLength(1);
  });

  it("does not send repository work before startup is ready", async () => {
    configure = child => { child.holdPing = true; };
    const started = agent.start();
    const opened = agent.request("open_local", { repo_id: "one", path: "/one" });
    void started.catch(() => {}); void opened.catch(() => {});
    await new Promise<void>(resolve => setImmediate(resolve));
    expect(children[0].frames.map(frame => frame.method)).toEqual(["ping"]);
    children[0].holdPing = false; children[0].answer(children[0].frames[0]);
    await Promise.all([started, opened]);
  });

  it("an explicit repository retry starts a fresh process after failed startup", async () => {
    configure = child => { child.holdPing = true; };
    const failed = expect(agent.start()).rejects.toThrow("Native agent exited");
    await new Promise<void>(resolve => setImmediate(resolve));
    children[0].kill("SIGKILL");
    await failed;
    configure = () => {};
    await expect(agent.request("open_local", { repo_id: "one", path: "/one" })).resolves.toMatchObject({ repo_id: "one", native_session_id: expect.any(String) });
  });

  it("signal crash restores repository identity before later reads", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    children[0].exit(null, "SIGKILL"); children[0].close();
    await vi.waitFor(() => expect(children).toHaveLength(2));
    await expect(agent.request("read_text", { repo_id: "one", path: "weights.txt" })).resolves.toEqual({ content: "Recovered native content" });
    expect(children[1].frames.filter(frame => frame.method === "open_local").map(frame => frame.params)).toEqual([{ repo_id: "one", path: "/one" }]);
  });

  it("restart waits for old process streams to close", async () => {
    await agent.start();
    children[0].exit(1, null);
    await new Promise<void>(resolve => setImmediate(resolve));
    expect(children).toHaveLength(1);
    children[0].close();
    await vi.waitFor(() => expect(children).toHaveLength(2));
  });

  it("a closed repository is not restored on a later start", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    await agent.request("close", { repo_id: "one" });
    await agent.shutdown(); await agent.start();
    expect(children[1].frames.some(frame => frame.method === "open_local")).toBe(false);
  });

  it("failed repository recovery leaves a sibling usable and reports its identity", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    await agent.request("open_local", { repo_id: "two", path: "/two" });
    configure = child => { child.failedRepository = "two"; };
    const notices: Array<{ method: string; params: unknown }> = [];
    agent.on("notification", (method, params) => notices.push({ method, params }));
    children[0].exit(1, null); children[0].close();
    await vi.waitFor(() => expect(children).toHaveLength(2));
    await expect(agent.request("read_text", { repo_id: "one", path: "weights.txt" })).resolves.toEqual({ content: "Recovered native content" });
    await expect(agent.request("read_text", { repo_id: "two", path: "weights.txt" })).rejects.toThrow("Repository was removed");
    expect(notices).toContainEqual({ method: "repo/reopen_failed", params: expect.objectContaining({ repo_id: "two", message: "Repository was removed" }) });
  });

  it("one blocked recovery does not block another repository", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    await agent.request("open_local", { repo_id: "two", path: "/two" });
    configure = child => { child.holdOpen = "one"; };
    children[0].kill("SIGKILL");
    await vi.waitFor(() => expect(children[1]?.frames.some(frame => frame.method === "open_local" && frame.params.repo_id === "one")).toBe(true));
    await expect(agent.request("read_text", { repo_id: "two" })).resolves.toEqual({ content: "Recovered native content" });
    children[1].holdOpen = undefined;
    children[1].answer(children[1].frames.find(frame => frame.method === "open_local" && frame.params.repo_id === "one")!);
    await agent.start();
  });

  it("close during recovery revokes the desired repository and drains its replay", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    configure = child => { child.holdOpen = "one"; };
    children[0].kill("SIGKILL");
    await vi.waitFor(() => expect(children[1]?.frames.some(frame => frame.method === "open_local")).toBe(true));
    const closing = agent.request("close", { repo_id: "one" });
    children[1].holdOpen = undefined;
    children[1].answer(children[1].frames.find(frame => frame.method === "open_local")!);
    await closing;
    expect(children[1].repositories.has("one")).toBe(false);
    await agent.shutdown(); await agent.start();
    expect(children[2].frames.some(frame => frame.method === "open_local")).toBe(false);
  });

  it("shutdown joins stdio closure and a waiting start cannot resurrect the process", async () => {
    await agent.start();
    children[0].exit(1, null);
    const restarting = agent.start();
    const stopped = agent.shutdown();
    const rejected = expect(restarting).rejects.toThrow("stopped during startup");
    let drained = false;
    void stopped.then(() => { drained = true; });
    await new Promise<void>(resolve => setImmediate(resolve));
    expect(drained).toBe(false);
    children[0].close();
    await Promise.all([stopped, rejected]);
    expect(children).toHaveLength(1);
  });

  it("keeps physical local editing available while revoking native file operations", async () => {
    const provider = new LocalProvider(agent);
    try {
      await agent.start();
      expect((await provider.capabilities("one")).canEdit).toBe(false);
      await agent.request("open_local", { repo_id: "one", path: "/one" });
      expect(await provider.capabilities("one")).toMatchObject({ canEdit: true, canHydrate: true, canMaterialize: true });
      children[0].exit(1, null);
      expect(provider.nativeSession("one")).toBeNull();
      expect(await provider.capabilities("one")).toMatchObject({ canEdit: true, canWatch: true, canHydrate: false, canMaterialize: false, canForge: false });
    } finally { provider.dispose(); }
  });

  it("requires the complete capture and materialization method set", async () => {
    configure = child => { child.methods = child.methods.filter(method => method !== "file_close"); };
    const provider = new LocalProvider(agent);
    try {
      await agent.start();
      await agent.request("open_local", { repo_id: "one", path: "/one" });
      expect(await provider.capabilities("one")).toMatchObject({ canHydrate: true, canMaterialize: false });
    } finally { provider.dispose(); }
  });

  it("reopening in the same process replaces the native repository session", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    const original = agent.repositorySession("one");
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    expect(agent.repositorySession("one")).not.toBe(original);
  });

  it("a failed explicit reopen leaves its former native session unavailable", async () => {
    await agent.start();
    await agent.request("open_local", { repo_id: "one", path: "/one" });
    children[0].failedRepository = "one";
    await expect(agent.request("open_local", { repo_id: "one", path: "/one" })).rejects.toThrow("Repository was removed");
    expect(agent.repositorySession("one")).toBeNull();
  });
});
