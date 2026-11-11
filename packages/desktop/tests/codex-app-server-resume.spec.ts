import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { PassThrough } from "node:stream";
import { EventEmitter } from "node:events";
import type { AgentEvent } from "../electron/orchestrator-adapters";

vi.mock("shell-env", () => ({ shellEnv: async () => ({ ...process.env }) }));

vi.mock("node:child_process", () => ({
  spawn: vi.fn(),
  execFile: vi.fn(),
  execFileSync: vi.fn(() => "/usr/bin:/usr/local/bin"),
}));

import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

function drainFrames(stdin: PassThrough): Array<{ id?: number; method?: string; params?: unknown }> {
  return (stdin.read()?.toString() ?? "").split("\n").filter(Boolean).map((line: string) => JSON.parse(line));
}

describe("Codex thread resume", () => {
  let adapter: CodexAppServerAdapter;
  let stdin: PassThrough;
  let stdout: PassThrough;
  let events: AgentEvent[];
  let exited: ReturnType<typeof vi.fn>;
  let startup: Promise<void>;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    stdin = new PassThrough();
    stdout = new PassThrough();
    const child = Object.assign(new EventEmitter(), {
      stdin, stdout, stderr: new PassThrough(), kill: vi.fn(),
    });
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
    adapter = new CodexAppServerAdapter();
    adapter.resumeThreadId = "thr_existing";
    events = [];
    exited = vi.fn();
    adapter.onEvent(event => events.push(event));
    adapter.onExit(exited);
  });

  afterEach(async () => {
    adapter.cancel();
    await vi.runAllTimersAsync();
    vi.useRealTimers();
  });

  async function reply(id: number | undefined, response: object) {
    stdout.write(JSON.stringify({ id, ...response }) + "\n");
    await vi.advanceTimersByTimeAsync(0);
    return drainFrames(stdin);
  }

  async function initialize() {
    startup = adapter.spawn("Continue the task", "/tmp/test-project");
    await vi.advanceTimersByTimeAsync(0);
    const init = drainFrames(stdin).find(frame => frame.method === "initialize");
    const frames = await reply(init?.id, { result: {} });
    expect(frames.map(frame => frame.method)).toEqual(["initialized", "thread/resume"]);
    const resume = frames[1];
    expect(resume.params).toEqual({ threadId: "thr_existing", cwd: "/tmp/test-project" });
    return resume;
  }

  it("loads the stored thread before starting a turn on it", async () => {
    const resume = await initialize();
    const frames = await reply(resume.id, { result: { thread: { id: "thr_existing" } } });
    const turn = frames.find(frame => frame.method === "turn/start");
    expect(turn?.params).toEqual({
      threadId: "thr_existing", input: [{ type: "text", text: "Continue the task" }],
    });
    await reply(turn?.id, { result: { turn: { id: "turn_1" } } });
    expect(events.filter(event => event.type === "thread_started")).toEqual([
      expect.objectContaining({ threadId: "thr_existing" }),
    ]);
  });

  it("accepts a saved goal snapshot delivered with the resume response before its continuation runs", async () => {
    const resume = await initialize();
    stdout.write(JSON.stringify({ id: resume.id, result: { thread: { id: "thr_existing" } } }) + "\n"
      + JSON.stringify({ method: "thread/started", params: { thread: { id: "foreign-child" } } }) + "\n"
      + JSON.stringify({ method: "thread/goal/updated", params: { threadId: "thr_existing", goal: { threadId: "thr_existing", objective: "Saved objective", status: "active", tokensUsed: 20 } } }) + "\n");
    await vi.advanceTimersByTimeAsync(0);
    const turn = drainFrames(stdin).find(frame => frame.method === "turn/start");
    await reply(turn?.id, { result: { turn: { id: "ordinary-turn" } } });
    expect({ goal: events.find(event => event.type === "goal_updated")?.goal, threads: events.filter(event => event.type === "thread_started").map(event => event.threadId) }).toEqual({
      goal: { threadId: "thr_existing", objective: "Saved objective", status: "active", tokensUsed: 20 }, threads: ["thr_existing"],
    });
  });

  it("resolves startup only after the first native turn acknowledges submission", async () => {
    const resume = await initialize();
    let settled = false;
    void startup.then(() => { settled = true; }, () => {});
    const frames = await reply(resume.id, { result: { thread: { id: "thr_existing" } } });
    expect(settled).toBe(false);
    const request = frames.find(frame => frame.method === "turn/start");
    await reply(request?.id, { result: { turn: { id: "turn_1" } } });
    await startup;
    expect(settled).toBe(true);
  });

  it("captures thread/started before the thread/start response without emitting it twice", async () => {
    adapter.resumeThreadId = null;
    adapter.spawn("Start a task", "/tmp/test-project");
    await vi.advanceTimersByTimeAsync(0);
    const init = drainFrames(stdin).find(frame => frame.method === "initialize");
    const frames = await reply(init?.id, { result: {} });
    const start = frames.find(frame => frame.method === "thread/start");
    expect(start?.params).toEqual({ cwd: "/tmp/test-project" });
    stdout.write(JSON.stringify({ method: "thread/started", params: { thread: { id: "thr_new" } } }) + "\n");
    const turnFrames = await reply(start?.id, { result: { thread: { id: "thr_new" } } });
    const turn = turnFrames.find(frame => frame.method === "turn/start");
    await reply(turn?.id, { result: { turn: { id: "turn_new" } } });
    expect(events.filter(event => event.type === "thread_started")).toEqual([
      expect.objectContaining({ threadId: "thr_new" }),
    ]);
  });

  it("surfaces resume failure without silently starting a new conversation", async () => {
    const resume = await initialize();
    const frames = await reply(resume.id, { error: { code: -32000, message: "Thread not found or expired" } });
    await expect(startup).rejects.toThrow("Thread not found or expired");
    expect(events).toContainEqual(expect.objectContaining({
      type: "error", error: "Codex thread start failed: Thread not found or expired",
    }));
    expect(exited).not.toHaveBeenCalled();
    vi.mocked(spawn).mock.results.at(-1)?.value.emit("close", 1);
    expect(exited).toHaveBeenCalledWith(1);
    expect(frames).toEqual([]);
  });

  it("rejects a response without the protocol's thread.id", async () => {
    const resume = await initialize();
    const frames = await reply(resume.id, { result: {} });
    expect(events).toContainEqual(expect.objectContaining({
      type: "error", error: "Codex thread start failed: thread/resume returned no thread.id",
    }));
    expect(frames).toEqual([]);
  });
});
