/**
 * Process death test for CodexAppServerAdapter.
 *
 * Kills the fake server mid-turn and asserts the adapter emits exactly
 * one `error` AgentEvent, one `done("")`, and calls `emitExit(1)`.
 * The orchestrator's existing exit handler transitions the task to `failed`
 * and preserves `task.codexThreadId` for the next iteration.
 *
 * Validates: Requirements CA9.4, CA6.3
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
    pid: 99999,
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

describe("CodexAppServerAdapter — Process Death Mid-Turn", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("emits one error + one done('') + emitExit(1) when process dies mid-turn", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    let exitCode: number | null = null;

    adapter.onEvent((e) => events.push(e));
    adapter.onExit((code) => { exitCode = code; });

    // Spawn the adapter
    adapter.spawn("Fix the bug in main.ts", "/tmp/test-project");

    // --- Complete the initialization handshake ---

    // 1. Client sends `initialize`
    let frames = await waitForFrames(stdin);
    const initRequest = frames.find((f) => f.method === "initialize");
    expect(initRequest).toBeDefined();

    // Server responds to initialize
    serverWrite(stdout, {
      id: initRequest!.id,
      result: { serverInfo: { name: "codex", version: "0.1.0" } },
    });

    // 2. Client sends `initialized` notification + `thread/start`
    await new Promise((r) => setTimeout(r, 20));
    frames = drainFrames(stdin);
    const threadStartRequest = frames.find((f) => f.method === "thread/start");
    expect(threadStartRequest).toBeDefined();

    // Server responds to thread/start
    serverWrite(stdout, {
      id: threadStartRequest!.id,
      result: { thread: { id: "thr_death_test" } },
    });

    // 3. Client sends `turn/start`
    await new Promise((r) => setTimeout(r, 20));
    frames = drainFrames(stdin);
    let turnStartRequest = frames.find((f) => f.method === "turn/start");
    if (!turnStartRequest) {
      const moreFrames = await waitForFrames(stdin);
      turnStartRequest = moreFrames.find((f) => f.method === "turn/start");
    }
    expect(turnStartRequest).toBeDefined();

    // Server responds to turn/start
    serverWrite(stdout, {
      id: turnStartRequest!.id,
      result: { turnId: "turn_death" },
    });

    // 4. Server sends turn/started notification (sets activeTurnId)
    serverWrite(stdout, {
      method: "turn/started",
      params: { turn: { id: "turn_death" }, threadId: "thr_death_test" },
    });

    // Let the adapter process the turn/started notification
    await new Promise((r) => setTimeout(r, 20));

    // Clear events collected during handshake so we only check post-death events
    events.length = 0;

    // --- Mid-turn: process dies (exit code 1) ---
    fakeProc.emit("exit", 1, null);

    // Give the adapter time to process the exit event
    await new Promise((r) => setTimeout(r, 30));

    // --- Assertions ---

    // Exactly one error event about process exit
    const errorEvents = events.filter((e) => e.type === "error");
    expect(errorEvents).toHaveLength(1);
    expect(errorEvents[0].error).toContain("process exited mid-turn");

    // Exactly one done event with empty content
    const doneEvents = events.filter((e) => e.type === "done");
    expect(doneEvents).toHaveLength(1);
    expect(doneEvents[0].content).toBe("");

    // emitExit(1) was called (verified via onExit listener)
    expect(exitCode).toBe(1);

    // Ordering: error before done
    const errorIdx = events.indexOf(errorEvents[0]);
    const doneIdx = events.indexOf(doneEvents[0]);
    expect(errorIdx).toBeLessThan(doneIdx);
  });

  it("does NOT emit error/done on exit when no turn is active", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    let exitCode: number | null = null;

    adapter.onEvent((e) => events.push(e));
    adapter.onExit((code) => { exitCode = code; });

    // Spawn the adapter
    adapter.spawn("Hello", "/tmp/test-project");

    // Complete initialization handshake
    let frames = await waitForFrames(stdin);
    const initRequest = frames.find((f) => f.method === "initialize");
    serverWrite(stdout, {
      id: initRequest!.id,
      result: { serverInfo: { name: "codex", version: "0.1.0" } },
    });

    await new Promise((r) => setTimeout(r, 20));
    frames = drainFrames(stdin);
    const threadStartRequest = frames.find((f) => f.method === "thread/start");
    serverWrite(stdout, {
      id: threadStartRequest!.id,
      result: { thread: { id: "thr_no_turn" } },
    });

    await new Promise((r) => setTimeout(r, 20));
    frames = drainFrames(stdin);
    let turnStartRequest = frames.find((f) => f.method === "turn/start");
    if (!turnStartRequest) {
      const moreFrames = await waitForFrames(stdin);
      turnStartRequest = moreFrames.find((f) => f.method === "turn/start");
    }
    serverWrite(stdout, {
      id: turnStartRequest!.id,
      result: { turnId: "turn_idle" },
    });

    // Send turn/started then turn/completed (turn finishes normally)
    serverWrite(stdout, {
      method: "turn/started",
      params: { turn: { id: "turn_idle" }, threadId: "thr_no_turn" },
    });
    await new Promise((r) => setTimeout(r, 10));
    serverWrite(stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_idle", status: "completed" },
        threadId: "thr_no_turn",
      },
    });

    await new Promise((r) => setTimeout(r, 20));

    // Clear events from handshake/turn
    events.length = 0;

    // Process exits when no turn is active
    fakeProc.emit("exit", 0, null);
    await new Promise((r) => setTimeout(r, 30));

    // Should NOT emit error or done (no active turn)
    const errorEvents = events.filter((e) => e.type === "error");
    const doneEvents = events.filter((e) => e.type === "done");
    expect(errorEvents).toHaveLength(0);
    expect(doneEvents).toHaveLength(0);

    // emitExit still called
    expect(exitCode).toBe(0);
  });
});
