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

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    stdin = new PassThrough();
    stdout = new PassThrough();
    const child = Object.assign(new EventEmitter(), {
      stdin, stdout, stderr: new PassThrough(), pid: 99999, kill: vi.fn(),
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
    await adapter.spawn("Continue the task", "/tmp/test-project");
    const init = drainFrames(stdin).find(frame => frame.method === "initialize");
    const frames = await reply(init?.id, { result: {} });
    expect(frames.map(frame => frame.method)).toEqual(["initialized", "thread/resume"]);
    const resume = frames[1];
    expect(resume.params).toEqual({ threadId: "thr_existing" });
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
  });

  it("surfaces resume failure without silently starting a new conversation", async () => {
    const resume = await initialize();
    const frames = await reply(resume.id, { error: { code: -32000, message: "Thread not found or expired" } });
    expect(events).toContainEqual(expect.objectContaining({
      type: "error", error: "Codex thread start failed: Thread not found or expired",
    }));
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
