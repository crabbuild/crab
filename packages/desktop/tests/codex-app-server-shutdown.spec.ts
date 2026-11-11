/**
 * Graceful shutdown test for CodexAppServerAdapter.
 *
 * Spawns the adapter, starts a turn with a pending approval request,
 * then calls cancel() and asserts the shutdown sequence:
 *   1. Pending approval responded with { decision: "cancel" }
 *   2. turn/interrupt is sent
 *   3. stdin is closed (ended)
 *   4. SIGTERM fires after CANCEL_SIGTERM_MS (2000ms)
 *   5. SIGKILL fires after CANCEL_SIGKILL_MS (5000ms)
 *
 * Uses fake timers for determinism.
 *
 * Validates: Requirements CA2.6, CA5.3, NFR2
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { PassThrough } from "node:stream";
import { EventEmitter } from "node:events";
import type { AgentEvent } from "../electron/orchestrator-adapters";

vi.mock("node:child_process", () => ({
  spawn: vi.fn(),
  execFile: vi.fn(),
  execFileSync: vi.fn(() => "/usr/bin:/usr/local/bin"),
}));

import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

function createFakeProcess() {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  const proc = new EventEmitter() as any;
  proc.stdin = stdin;
  proc.stdout = stdout;
  proc.stderr = stderr;
  proc.kill = vi.fn();
  return { proc, stdin, stdout, stderr };
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

function serverWrite(stdout: PassThrough, msg: object): void {
  stdout.write(JSON.stringify(msg) + "\n");
}

/**
 * Completes the initialization handshake so the adapter is ready
 * with an active turn.
 */
async function completeHandshake(
  stdin: PassThrough,
  stdout: PassThrough,
): Promise<void> {
  // Wait for initialize request
  await new Promise(r => setTimeout(r, 20));
  let frames = drainFrames(stdin);
  const initFrame = frames.find(f => f.method === "initialize");
  if (!initFrame) throw new Error("No initialize request found");

  // Respond to initialize
  serverWrite(stdout, {
    id: initFrame.id,
    result: { serverInfo: { name: "codex", version: "0.1.0" } },
  });

  // Wait for initialized + thread/start
  await new Promise(r => setTimeout(r, 20));
  frames = drainFrames(stdin);
  const threadFrame = frames.find(f => f.method === "thread/start");
  if (!threadFrame) throw new Error("No thread/start request found");

  // Respond to thread/start
  serverWrite(stdout, {
    id: threadFrame.id,
    result: { thread: { id: "thr_shutdown_1" } },
  });

  // Send thread/started notification
  serverWrite(stdout, {
    method: "thread/started",
    params: { thread: { id: "thr_shutdown_1" } },
  });

  // Wait for turn/start
  await new Promise(r => setTimeout(r, 20));
  frames = drainFrames(stdin);
  const turnFrame = frames.find(f => f.method === "turn/start");
  if (!turnFrame) throw new Error("No turn/start request found");

  // Respond to turn/start
  serverWrite(stdout, {
    id: turnFrame.id,
    result: { turn: { id: "turn_shutdown_1" } },
  });

  // Send turn/started notification (sets activeTurnId)
  serverWrite(stdout, {
    method: "turn/started",
    params: { turn: { id: "turn_shutdown_1" } },
  });

  // Allow handlers to process
  await new Promise(r => setTimeout(r, 20));
}

describe("CodexAppServerAdapter — Graceful shutdown (CA-P6, D7)", () => {
  let adapter: CodexAppServerAdapter;
  let fakeProc: ReturnType<typeof createFakeProcess>;
  let events: AgentEvent[];

  beforeEach(async () => {
    vi.useFakeTimers();

    fakeProc = createFakeProcess();
    (spawn as any).mockReturnValue(fakeProc.proc);

    adapter = new CodexAppServerAdapter();
    events = [];
    adapter.onEvent((event) => events.push(event));

    // Spawn the adapter
    adapter.spawn("Do something", "/tmp/test-shutdown");

    // Complete the handshake (uses real-ish async but fake timers)
    // We need to advance timers for the setTimeout calls in completeHandshake
    await vi.advanceTimersByTimeAsync(20);
    let frames = drainFrames(fakeProc.stdin);
    const initFrame = frames.find(f => f.method === "initialize");
    if (!initFrame) throw new Error("No initialize request found");

    serverWrite(fakeProc.stdout, {
      id: initFrame.id,
      result: { serverInfo: { name: "codex", version: "0.1.0" } },
    });

    await vi.advanceTimersByTimeAsync(20);
    frames = drainFrames(fakeProc.stdin);
    const threadFrame = frames.find(f => f.method === "thread/start");
    if (!threadFrame) throw new Error("No thread/start request found");

    serverWrite(fakeProc.stdout, {
      id: threadFrame.id,
      result: { thread: { id: "thr_shutdown_1" } },
    });

    serverWrite(fakeProc.stdout, {
      method: "thread/started",
      params: { thread: { id: "thr_shutdown_1" } },
    });

    await vi.advanceTimersByTimeAsync(20);
    frames = drainFrames(fakeProc.stdin);
    const turnFrame = frames.find(f => f.method === "turn/start");
    if (!turnFrame) throw new Error("No turn/start request found");

    serverWrite(fakeProc.stdout, {
      id: turnFrame.id,
      result: { turn: { id: "turn_shutdown_1" } },
    });

    serverWrite(fakeProc.stdout, {
      method: "turn/started",
      params: { turn: { id: "turn_shutdown_1" } },
    });

    await vi.advanceTimersByTimeAsync(20);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("cancel() follows the correct shutdown sequence with pending approval", async () => {
    // Server sends an approval request (so there's a pending approval)
    serverWrite(fakeProc.stdout, {
      id: 42,
      method: "item/commandExecution/requestApproval",
      params: {
        request_id: "req-shutdown-1",
        command: "rm -rf /tmp/data",
        reason: "Destructive command",
      },
    });

    await vi.advanceTimersByTimeAsync(10);

    // Verify the approval_request event was emitted
    const approvalEvent = events.find(e => e.type === "approval_request");
    expect(approvalEvent).toBeDefined();
    expect(approvalEvent!.approval!.requestId).toBe("42");

    // Drain any frames from the handshake/approval processing
    drainFrames(fakeProc.stdin);

    // Spy on stdin.end to detect when it's closed
    const stdinEndSpy = vi.spyOn(fakeProc.stdin, "end");

    // Call cancel()
    adapter.cancel();

    // Allow microtasks to flush (the cancel responds to approvals synchronously)
    await vi.advanceTimersByTimeAsync(0);

    // Step 1: Assert the first frame written is the approval cancel response
    let frames = drainFrames(fakeProc.stdin);
    expect(frames.length).toBeGreaterThanOrEqual(1);

    const approvalResponse = frames[0];
    expect(approvalResponse).toEqual({
      id: 42,
      result: { decision: "cancel" },
    });

    // Step 2: Assert turn/interrupt is sent (may be in same batch or next)
    const turnInterruptFrame = frames.find(f => f.method === "turn/interrupt");
    expect(turnInterruptFrame).toBeDefined();
    expect(turnInterruptFrame!.method).toBe("turn/interrupt");

    // Respond to the turn/interrupt RPC request (so it doesn't become an
    // unhandled rejection when rpc.close() rejects pending promises)
    serverWrite(fakeProc.stdout, {
      id: turnInterruptFrame!.id,
      result: {},
    });

    // Now simulate the server sending turn/completed in response to interrupt
    // (the adapter waits for this before proceeding with close)
    serverWrite(fakeProc.stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_shutdown_1", status: "interrupted" },
        threadId: "thr_shutdown_1",
      },
    });

    // Allow the turn/completed handler to fire and the cancel to proceed
    await vi.advanceTimersByTimeAsync(10);

    // Drain any thread/unsubscribe notification
    frames = drainFrames(fakeProc.stdin);
    const unsubFrame = frames.find(f => f.method === "thread/unsubscribe");
    // thread/unsubscribe is best-effort, may or may not be present

    // Step 3: Assert stdin is closed (rpc.close() ends stdin)
    expect(stdinEndSpy).toHaveBeenCalled();

    // Step 4: Assert SIGTERM has NOT been sent yet (need to wait CANCEL_SIGTERM_MS)
    expect(fakeProc.proc.kill).not.toHaveBeenCalled();

    // Advance past CANCEL_SIGTERM_MS (2000ms)
    await vi.advanceTimersByTimeAsync(2000);

    // Assert SIGTERM is sent
    expect(fakeProc.proc.kill).toHaveBeenCalledWith("SIGTERM");
    expect(fakeProc.proc.kill).toHaveBeenCalledTimes(1);

    // Step 5: Advance past CANCEL_SIGKILL_MS (5000ms)
    await vi.advanceTimersByTimeAsync(5000);

    // Assert SIGKILL is sent
    expect(fakeProc.proc.kill).toHaveBeenCalledWith("SIGKILL");
    expect(fakeProc.proc.kill).toHaveBeenCalledTimes(2);
  });

  it("handles a rejected interrupt and process close without a false failure", async () => {
    events.length = 0;
    adapter.cancel();
    await vi.advanceTimersByTimeAsync(0);
    const interrupt = drainFrames(fakeProc.stdin).find(frame => frame.method === "turn/interrupt");
    serverWrite(fakeProc.stdout, { id: interrupt?.id, error: { code: -32000, message: "Turn already ended" } });
    fakeProc.proc.emit("close", 0);
    await vi.runAllTimersAsync();
    expect({ errors: events.filter(event => event.type === "error"), outcomes: events.filter(event => event.type === "done").map(event => event.outcome), killed: fakeProc.proc.kill.mock.calls }).toEqual({ errors: [], outcomes: ["interrupted"], killed: [] });
  });

  it("cancel() is idempotent — second call is a no-op", async () => {
    // Drain frames from handshake
    drainFrames(fakeProc.stdin);

    // Call cancel twice
    const stopped = adapter.cancel();
    expect(adapter.cancel()).toBe(stopped);
    let closed = false;
    void stopped.then(() => { closed = true; });

    // Allow microtasks to flush
    await vi.advanceTimersByTimeAsync(0);

    // The turn/interrupt should only be sent once
    const frames = drainFrames(fakeProc.stdin);
    const interruptFrames = frames.filter(f => f.method === "turn/interrupt");
    expect(interruptFrames.length).toBe(1);

    // Respond to the turn/interrupt RPC request to avoid unhandled rejection
    const interruptFrame = interruptFrames[0];
    serverWrite(fakeProc.stdout, {
      id: interruptFrame.id,
      result: {},
    });

    // Send turn/completed so shutdown proceeds
    serverWrite(fakeProc.stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_shutdown_1", status: "interrupted" },
        threadId: "thr_shutdown_1",
      },
    });

    await vi.advanceTimersByTimeAsync(10);

    // Advance through the full shutdown timeline
    await vi.advanceTimersByTimeAsync(2000);
    expect(fakeProc.proc.kill).toHaveBeenCalledWith("SIGTERM");

    await vi.advanceTimersByTimeAsync(5000);
    expect(fakeProc.proc.kill).toHaveBeenCalledWith("SIGKILL");

    // Only 2 kill calls total (SIGTERM + SIGKILL), not doubled
    expect(fakeProc.proc.kill).toHaveBeenCalledTimes(2);
    expect(closed).toBe(false);
    fakeProc.proc.emit("close", 1);
    await stopped;
    expect(closed).toBe(true);
  });
});
