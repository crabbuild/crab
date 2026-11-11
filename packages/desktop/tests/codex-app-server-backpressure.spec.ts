/**
 * Backpressure integration test for CodexAppServerAdapter.
 *
 * Verifies adapter-level behavior when the Codex app-server responds with
 * JSON-RPC error code -32001 (server overloaded). The underlying retry logic
 * lives in CodexRpcClient; these tests confirm the adapter correctly surfaces
 * success after retries and emits an error AgentEvent on exhaustion.
 *
 * Validates: Requirements CA9.5, CA6.1
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { PassThrough } from "node:stream";
import type { AgentEvent } from "../electron/orchestrator-adapters";

// Mock child_process.spawn before importing the adapter.
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
 * Completes the initialize → initialized → thread/start handshake so the
 * adapter is ready to send turn/start.
 */
async function completeHandshake(stdin: PassThrough, stdout: PassThrough) {
  // 1. initialize request
  let frames = await waitForFrames(stdin);
  const initReq = frames.find((f) => f.method === "initialize");
  expect(initReq).toBeDefined();
  serverWrite(stdout, {
    id: initReq!.id,
    result: { serverInfo: { name: "codex", version: "0.1.0" } },
  });

  // 2. initialized notification + thread/start request
  await new Promise((r) => setTimeout(r, 20));
  frames = drainFrames(stdin);
  let threadStartReq = frames.find((f) => f.method === "thread/start");
  if (!threadStartReq) {
    const more = await waitForFrames(stdin);
    threadStartReq = more.find((f) => f.method === "thread/start");
  }
  expect(threadStartReq).toBeDefined();

  // Respond to thread/start
  serverWrite(stdout, {
    id: threadStartReq!.id,
    result: { thread: { id: "thr_bp_test" } },
  });

  // Send thread/started notification
  serverWrite(stdout, {
    method: "thread/started",
    params: { thread: { id: "thr_bp_test" } },
  });

  await new Promise((r) => setTimeout(r, 10));
}

describe("CodexAppServerAdapter — Backpressure", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("retries on -32001 twice then succeeds — turn starts without error", async () => {
    vi.useFakeTimers();
    try {
      const { fakeProc, stdin, stdout } = createFakeProcess();
      mockedSpawn.mockReturnValue(fakeProc as any);

      const adapter = new CodexAppServerAdapter();
      const events: AgentEvent[] = [];
      adapter.onEvent((e) => events.push(e));

      adapter.spawn("Hello backpressure test", "/tmp/bp-test");

      // Complete the handshake (uses real-ish timing via advanceTimersByTimeAsync)
      // Step 1: initialize
      await vi.advanceTimersByTimeAsync(10);
      let frames = drainFrames(stdin);
      const initReq = frames.find((f) => f.method === "initialize");
      expect(initReq).toBeDefined();
      serverWrite(stdout, {
        id: initReq!.id,
        result: { serverInfo: { name: "codex", version: "0.1.0" } },
      });

      // Step 2: initialized + thread/start
      await vi.advanceTimersByTimeAsync(50);
      frames = drainFrames(stdin);
      let threadStartReq = frames.find((f) => f.method === "thread/start");
      if (!threadStartReq) {
        await vi.advanceTimersByTimeAsync(50);
        frames = drainFrames(stdin);
        threadStartReq = frames.find((f) => f.method === "thread/start");
      }
      expect(threadStartReq).toBeDefined();

      serverWrite(stdout, {
        id: threadStartReq!.id,
        result: { thread: { id: "thr_bp_test" } },
      });

      serverWrite(stdout, {
        method: "thread/started",
        params: { thread: { id: "thr_bp_test" } },
      });

      // Step 3: turn/start — respond with -32001 twice, then success
      await vi.advanceTimersByTimeAsync(50);
      frames = drainFrames(stdin);
      let turnStartReq = frames.find((f) => f.method === "turn/start");
      if (!turnStartReq) {
        await vi.advanceTimersByTimeAsync(50);
        frames = drainFrames(stdin);
        turnStartReq = frames.find((f) => f.method === "turn/start");
      }
      expect(turnStartReq).toBeDefined();

      // First -32001 rejection
      serverWrite(stdout, {
        id: turnStartReq!.id,
        error: { code: -32001, message: "Server overloaded; retry later." },
      });

      // Advance time for the first retry (exponential backoff ~1s + jitter)
      await vi.advanceTimersByTimeAsync(10_000);
      frames = drainFrames(stdin);
      let retryReq = frames.find((f) => f.method === "turn/start");
      expect(retryReq).toBeDefined();

      // Second -32001 rejection
      serverWrite(stdout, {
        id: retryReq!.id,
        error: { code: -32001, message: "Server overloaded; retry later." },
      });

      // Advance time for the second retry
      await vi.advanceTimersByTimeAsync(10_000);
      frames = drainFrames(stdin);
      retryReq = frames.find((f) => f.method === "turn/start");
      expect(retryReq).toBeDefined();

      // Third attempt succeeds
      serverWrite(stdout, {
        id: retryReq!.id,
        result: { turn: { id: "turn_bp_1" } },
      });

      // Send turn/started notification
      serverWrite(stdout, {
        method: "turn/started",
        params: { turn: { id: "turn_bp_1" }, threadId: "thr_bp_test" },
      });

      await vi.advanceTimersByTimeAsync(50);

      // Verify no error events were emitted (the turn started successfully
      // after two -32001 retries)
      const errorEvents = events.filter((e) => e.type === "error");
      expect(errorEvents).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("4 consecutive -32001 rejections → request rejects, adapter emits error AgentEvent", async () => {
    vi.useFakeTimers();
    try {
      const { fakeProc, stdin, stdout } = createFakeProcess();
      mockedSpawn.mockReturnValue(fakeProc as any);

      const adapter = new CodexAppServerAdapter();
      const events: AgentEvent[] = [];
      adapter.onEvent((e) => events.push(e));

      adapter.spawn("Hello exhaustion test", "/tmp/bp-exhaust");

      // Complete handshake
      // Step 1: initialize
      await vi.advanceTimersByTimeAsync(10);
      let frames = drainFrames(stdin);
      const initReq = frames.find((f) => f.method === "initialize");
      expect(initReq).toBeDefined();
      serverWrite(stdout, {
        id: initReq!.id,
        result: { serverInfo: { name: "codex", version: "0.1.0" } },
      });

      // Step 2: initialized + thread/start
      await vi.advanceTimersByTimeAsync(50);
      frames = drainFrames(stdin);
      let threadStartReq = frames.find((f) => f.method === "thread/start");
      if (!threadStartReq) {
        await vi.advanceTimersByTimeAsync(50);
        frames = drainFrames(stdin);
        threadStartReq = frames.find((f) => f.method === "thread/start");
      }
      expect(threadStartReq).toBeDefined();

      serverWrite(stdout, {
        id: threadStartReq!.id,
        result: { thread: { id: "thr_bp_exhaust" } },
      });

      serverWrite(stdout, {
        method: "thread/started",
        params: { thread: { id: "thr_bp_exhaust" } },
      });

      // Step 3: turn/start — respond with -32001 four times (exceeds 3-retry limit)
      await vi.advanceTimersByTimeAsync(50);
      frames = drainFrames(stdin);
      let turnReq = frames.find((f) => f.method === "turn/start");
      if (!turnReq) {
        await vi.advanceTimersByTimeAsync(50);
        frames = drainFrames(stdin);
        turnReq = frames.find((f) => f.method === "turn/start");
      }
      expect(turnReq).toBeDefined();

      // Rejection 1 (initial attempt)
      serverWrite(stdout, {
        id: turnReq!.id,
        error: { code: -32001, message: "Server overloaded; retry later." },
      });

      // Rejection 2 (retry 1)
      await vi.advanceTimersByTimeAsync(10_000);
      frames = drainFrames(stdin);
      turnReq = frames.find((f) => f.method === "turn/start");
      expect(turnReq).toBeDefined();
      serverWrite(stdout, {
        id: turnReq!.id,
        error: { code: -32001, message: "Server overloaded; retry later." },
      });

      // Rejection 3 (retry 2)
      await vi.advanceTimersByTimeAsync(10_000);
      frames = drainFrames(stdin);
      turnReq = frames.find((f) => f.method === "turn/start");
      expect(turnReq).toBeDefined();
      serverWrite(stdout, {
        id: turnReq!.id,
        error: { code: -32001, message: "Server overloaded; retry later." },
      });

      // Rejection 4 (retry 3 — exhaustion, this is the final attempt)
      await vi.advanceTimersByTimeAsync(10_000);
      frames = drainFrames(stdin);
      turnReq = frames.find((f) => f.method === "turn/start");
      expect(turnReq).toBeDefined();
      serverWrite(stdout, {
        id: turnReq!.id,
        error: { code: -32001, message: "Server overloaded; retry later." },
      });

      // Allow the rejection to propagate
      await vi.advanceTimersByTimeAsync(100);

      // Verify the adapter emitted an error AgentEvent
      const errorEvents = events.filter((e) => e.type === "error");
      expect(errorEvents.length).toBeGreaterThanOrEqual(1);

      // The error should mention the turn/start failure
      const turnError = errorEvents.find(
        (e) => e.type === "error" && (e as any).error?.includes("turn/start"),
      );
      expect(turnError).toBeDefined();
    } finally {
      vi.useRealTimers();
    }
  });
});
