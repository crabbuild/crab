/**
 * Mid-turn error notification test for CodexAppServerAdapter.
 *
 * Verifies D11: `error` notifications with `willRetry: true` emit a `generic`
 * event (tool: ServerWarning), while `willRetry: false` emits an `error` event.
 * JSON-RPC code is preserved on `event.input` in both cases.
 *
 * Validates: Requirements CA6.2
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { PassThrough } from "node:stream";
import type { AgentEvent } from "../electron/orchestrator-adapters";

vi.mock("node:child_process", () => ({
  spawn: vi.fn(),
  execFile: vi.fn(),
  execFileSync: vi.fn(() => "/usr/bin:/usr/local/bin"),
}));

import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

const mockedSpawn = vi.mocked(spawn);

function createFakeProcess() {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const stderr = new PassThrough();

  const fakeProc = Object.assign(new (require("node:events").EventEmitter)(), {
    stdin,
    stdout,
    stderr,
    kill: vi.fn(),
    ref: vi.fn(),
    unref: vi.fn(),
  });

  return { fakeProc, stdin, stdout, stderr };
}

function drainFrames(stdin: PassThrough): Array<Record<string, unknown>> {
  const buf = stdin.read();
  if (!buf) return [];
  return buf
    .toString()
    .split("\n")
    .filter((l: string) => l.length > 0)
    .map((l: string) => JSON.parse(l));
}

async function waitForFrames(
  stdin: PassThrough,
  timeoutMs = 2000,
): Promise<Array<Record<string, unknown>>> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const frames = drainFrames(stdin);
    if (frames.length > 0) return frames;
    await new Promise((r) => setTimeout(r, 5));
  }
  return [];
}

function serverWrite(stdout: PassThrough, msg: object): void {
  stdout.write(JSON.stringify(msg) + "\n");
}

/**
 * Completes the full handshake sequence (initialize → initialized →
 * thread/start → thread/started → turn/start → turn/started) so the
 * adapter is in an active-turn state ready to receive mid-turn notifications.
 */
async function completeHandshake(
  stdin: PassThrough,
  stdout: PassThrough,
): Promise<void> {
  // 1. Respond to initialize
  let frames = await waitForFrames(stdin);
  const initReq = frames.find((f) => f.method === "initialize");
  serverWrite(stdout, {
    id: initReq!.id,
    result: { serverInfo: { name: "codex", version: "0.1.0" } },
  });

  // 2. Wait for initialized notification + thread/start
  await new Promise((r) => setTimeout(r, 20));
  frames = drainFrames(stdin);
  let threadStartReq = frames.find((f) => f.method === "thread/start");
  if (!threadStartReq) {
    const more = await waitForFrames(stdin);
    threadStartReq = more.find((f) => f.method === "thread/start");
  }
  serverWrite(stdout, {
    id: threadStartReq!.id,
    result: { thread: { id: "thr_1" } },
  });

  // 3. Wait for turn/start
  await new Promise((r) => setTimeout(r, 20));
  frames = drainFrames(stdin);
  let turnStartReq = frames.find((f) => f.method === "turn/start");
  if (!turnStartReq) {
    const more = await waitForFrames(stdin);
    turnStartReq = more.find((f) => f.method === "turn/start");
  }
  serverWrite(stdout, {
    id: turnStartReq!.id,
    result: { turn: { id: "turn_1" } },
  });

  // 4. Send thread/started and turn/started notifications
  serverWrite(stdout, {
    method: "thread/started",
    params: { thread: { id: "thr_1" } },
  });
  serverWrite(stdout, {
    method: "turn/started",
    params: { turn: { id: "turn_1" }, threadId: "thr_1" },
  });

  // Let the adapter process all notifications
  await new Promise((r) => setTimeout(r, 30));
}

describe("CodexAppServerAdapter — Mid-turn error notifications (D11)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("willRetry: true → emits generic with tool ServerWarning, NOT error", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent((e) => events.push(e));

    adapter.spawn("test prompt", "/tmp/test");
    await completeHandshake(stdin, stdout);

    // Clear events from handshake
    events.length = 0;

    // Server sends mid-turn error with willRetry: true
    serverWrite(stdout, {
      method: "error",
      params: {
        error: { code: -32000, message: "Rate limited" },
        willRetry: true,
        threadId: "thr_1",
        turnId: "turn_1",
      },
    });

    await new Promise((r) => setTimeout(r, 30));

    // Should emit a generic event with tool ServerWarning
    const genericEvents = events.filter((e) => e.type === "generic");
    expect(genericEvents.length).toBe(1);
    expect(genericEvents[0].tool).toBe("ServerWarning");
    expect(genericEvents[0].input).toEqual(
      expect.objectContaining({
        error: "Rate limited",
        willRetry: true,
        code: -32000,
      }),
    );

    // Should NOT emit an error event
    const errorEvents = events.filter((e) => e.type === "error");
    expect(errorEvents.length).toBe(0);
  });

  it("willRetry: false → emits error with code preserved on event.input", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent((e) => events.push(e));

    adapter.spawn("test prompt", "/tmp/test");
    await completeHandshake(stdin, stdout);

    // Clear events from handshake
    events.length = 0;

    // Server sends mid-turn error with willRetry: false
    serverWrite(stdout, {
      method: "error",
      params: {
        error: { code: -32000, message: "Fatal error" },
        willRetry: false,
        threadId: "thr_1",
        turnId: "turn_1",
      },
    });

    await new Promise((r) => setTimeout(r, 30));

    // Should emit an error event
    const errorEvents = events.filter((e) => e.type === "error");
    expect(errorEvents.length).toBe(1);
    expect(errorEvents[0].error).toBe("Fatal error");
    expect(errorEvents[0].input).toEqual(
      expect.objectContaining({ code: -32000 }),
    );

    // Should NOT emit a generic event
    const genericEvents = events.filter((e) => e.type === "generic");
    expect(genericEvents.length).toBe(0);
  });
});
