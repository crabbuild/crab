/**
 * Iterate / steer test for CodexAppServerAdapter.
 *
 * Sub-tests:
 *   1. Completed turn → iterate sends plain `turn/start`
 *   2. Active turn → iterate sends `turn/steer` with expectedTurnId
 *   3. Server rejects `turn/steer` → adapter sends `turn/interrupt`,
 *      awaits `turn/completed`, then `turn/start`
 *
 * Validates: Requirements CA9.6, CA4.4
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { PassThrough } from "node:stream";
import type { AgentEvent } from "../electron/orchestrator-adapters";

vi.mock("shell-env", () => ({ shellEnv: async () => ({ ...process.env }) }));

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
    pid: 12345,
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
 * Completes the full handshake sequence: initialize → initialized →
 * thread/start → thread/started → turn/start → turn/started.
 *
 * Returns the turn/start request id and the turnId so the caller can
 * complete or steer the turn.
 */
async function completeHandshake(
  stdin: PassThrough,
  stdout: PassThrough,
  prompt: string,
): Promise<{ turnStartId: number; turnId: string; threadId: string }> {
  // 1. initialize request
  let frames = await waitForFrames(stdin);
  const initReq = frames.find((f) => f.method === "initialize");
  serverWrite(stdout, { id: initReq!.id, result: { serverInfo: { name: "codex", version: "0.1.0" } } });

  // 2. initialized notification + thread/start
  await new Promise((r) => setTimeout(r, 20));
  frames = drainFrames(stdin);
  let threadStartReq = frames.find((f) => f.method === "thread/start");
  if (!threadStartReq) {
    const more = await waitForFrames(stdin);
    threadStartReq = more.find((f) => f.method === "thread/start");
  }
  serverWrite(stdout, { id: threadStartReq!.id, result: { thread: { id: "thr_iter_test" } } });

  // 3. turn/start
  await new Promise((r) => setTimeout(r, 20));
  frames = drainFrames(stdin);
  let turnStartReq = frames.find((f) => f.method === "turn/start");
  if (!turnStartReq) {
    const more = await waitForFrames(stdin);
    turnStartReq = more.find((f) => f.method === "turn/start");
  }
  const turnStartId = turnStartReq!.id as number;
  serverWrite(stdout, { id: turnStartId, result: { turnId: "turn_001" } });

  // 4. Server sends thread/started and turn/started notifications
  serverWrite(stdout, { method: "thread/started", params: { thread: { id: "thr_iter_test" } } });
  serverWrite(stdout, { method: "turn/started", params: { turn: { id: "turn_001" }, threadId: "thr_iter_test" } });

  await new Promise((r) => setTimeout(r, 20));

  return { turnStartId, turnId: "turn_001", threadId: "thr_iter_test" };
}

describe("CodexAppServerAdapter — Iterate / Steer", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("completed turn → iterate sends plain turn/start", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent((e) => events.push(e));

    await adapter.spawn("initial prompt", "/tmp/test");

    // Complete the handshake (which starts a turn)
    await completeHandshake(stdin, stdout, "initial prompt");

    // Send turn/completed to end the turn
    serverWrite(stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_001", status: "completed" },
        threadId: "thr_iter_test",
      },
    });

    // Wait for the notification to be processed
    await new Promise((r) => setTimeout(r, 30));

    // Drain any frames that were written during turn completion
    drainFrames(stdin);

    // Now call iterate with a follow-up message
    adapter.iterate("follow-up message");

    // The adapter should send a turn/start (no active turn)
    const frames = await waitForFrames(stdin);
    const turnStart = frames.find((f) => f.method === "turn/start");
    expect(turnStart).toBeDefined();
    expect(turnStart!.id).toBeDefined();
    expect((turnStart!.params as any).input).toEqual([
      { type: "text", text: "follow-up message" },
    ]);
    // Should NOT be a turn/steer
    const turnSteer = frames.find((f) => f.method === "turn/steer");
    expect(turnSteer).toBeUndefined();
  });

  it("active turn → iterate sends turn/steer with expectedTurnId", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent((e) => events.push(e));

    await adapter.spawn("initial prompt", "/tmp/test");

    // Complete the handshake — turn is now active (activeTurnId = "turn_001")
    await completeHandshake(stdin, stdout, "initial prompt");

    // Drain any pending frames
    drainFrames(stdin);

    // Call iterate while the turn is still active
    adapter.iterate("steer message");

    // The adapter should send a turn/steer request
    const frames = await waitForFrames(stdin);
    const turnSteer = frames.find((f) => f.method === "turn/steer");
    expect(turnSteer).toBeDefined();
    expect(turnSteer!.id).toBeDefined();
    expect((turnSteer!.params as any).turnId).toBe("turn_001");
    expect((turnSteer!.params as any).input).toEqual([
      { type: "text", text: "steer message" },
    ]);
  });

  it("server rejects turn/steer → adapter sends turn/interrupt, awaits turn/completed, then turn/start", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();
    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent((e) => events.push(e));

    await adapter.spawn("initial prompt", "/tmp/test");

    // Complete the handshake — turn is active
    await completeHandshake(stdin, stdout, "initial prompt");

    // Drain any pending frames
    drainFrames(stdin);

    // Call iterate while the turn is active
    adapter.iterate("steer message");

    // Read the turn/steer request from stdin
    let frames = await waitForFrames(stdin);
    const turnSteer = frames.find((f) => f.method === "turn/steer");
    expect(turnSteer).toBeDefined();
    const steerId = turnSteer!.id as number;

    // Server rejects turn/steer with method-not-found error
    serverWrite(stdout, {
      id: steerId,
      error: { code: -32601, message: "method not found" },
    });

    // Wait for the adapter to process the rejection and send turn/interrupt
    frames = await waitForFrames(stdin);
    const turnInterrupt = frames.find((f) => f.method === "turn/interrupt");
    expect(turnInterrupt).toBeDefined();
    expect((turnInterrupt!.params as any).turnId).toBe("turn_001");

    // Server sends turn/completed (status: interrupted)
    serverWrite(stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_001", status: "interrupted" },
        threadId: "thr_iter_test",
      },
    });

    // Wait for the adapter to process turn/completed and send a new turn/start
    await new Promise((r) => setTimeout(r, 30));
    frames = await waitForFrames(stdin);
    const turnStart = frames.find((f) => f.method === "turn/start");
    expect(turnStart).toBeDefined();
    expect(turnStart!.id).toBeDefined();
    expect((turnStart!.params as any).input).toEqual([
      { type: "text", text: "steer message" },
    ]);
  });
});
