/**
 * Protocol integration test for CodexAppServerAdapter.
 *
 * Drives the adapter via a fake stdio server (PassThrough streams) that
 * scripts the full handshake and a happy-path turn. Collects emitted
 * AgentEvents and asserts the expected sequence.
 *
 * Validates: Requirements CA9.2
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { PassThrough } from "node:stream";
import type { AgentEvent } from "../electron/orchestrator-adapters";

// We need to mock child_process.spawn before importing the adapter.
vi.mock("node:child_process", () => ({
  spawn: vi.fn(),
  execFile: vi.fn(),
  execFileSync: vi.fn(() => "/usr/bin:/usr/local/bin"),
}));

import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

const mockedSpawn = vi.mocked(spawn);

/**
 * Creates a fake child process with PassThrough streams for stdin/stdout/stderr.
 * Returns the streams so the test can script the server side of the protocol.
 */
function createFakeProcess() {
  const stdin = new PassThrough();   // adapter writes here → test reads
  const stdout = new PassThrough();  // test writes here → adapter reads
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

/**
 * Reads all pending frames from the stdin stream (what the adapter wrote).
 */
function drainFrames(stdin: PassThrough): Array<Record<string, unknown>> {
  const buf = stdin.read();
  if (!buf) return [];
  return buf
    .toString()
    .split("\n")
    .filter((l: string) => l.length > 0)
    .map((l: string) => JSON.parse(l));
}

/**
 * Waits for the next frame(s) written to stdin by the adapter.
 * Polls with a short interval until data is available or timeout.
 */
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

/**
 * Writes a server response/notification frame to stdout.
 */
function serverWrite(stdout: PassThrough, msg: object): void {
  stdout.write(JSON.stringify(msg) + "\n");
}

describe("CodexAppServerAdapter — Protocol Integration", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("scripts the full handshake + happy-path turn and emits expected AgentEvents", async () => {
    const { fakeProc, stdin, stdout } = createFakeProcess();

    mockedSpawn.mockReturnValue(fakeProc as any);

    const adapter = new CodexAppServerAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent((e) => events.push(e));

    // Start the adapter — this triggers the async initialization.
    adapter.spawn("Write a hello world function", "/tmp/test-project");

    // --- Step 1: Client sends `initialize` → server responds with success ---
    let frames = await waitForFrames(stdin);
    expect(frames.length).toBeGreaterThanOrEqual(1);
    const initRequest = frames.find((f) => f.method === "initialize");
    expect(initRequest).toBeDefined();
    expect(initRequest!.id).toBeDefined();
    expect((initRequest!.params as any).clientInfo.name).toBe("crab_desktop");

    // Server responds to initialize
    serverWrite(stdout, {
      id: initRequest!.id,
      result: { serverInfo: { name: "codex", version: "0.1.0" } },
    });

    // --- Step 2: Client sends `initialized` notification ---
    // Give the adapter time to process the response and send initialized
    await new Promise((r) => setTimeout(r, 20));
    frames = drainFrames(stdin);
    const initializedNotif = frames.find((f) => f.method === "initialized");
    expect(initializedNotif).toBeDefined();
    expect(initializedNotif).not.toHaveProperty("id"); // notification, no id

    // --- Step 3: Client sends `thread/start` → server responds with success ---
    // The adapter may send thread/start in the same batch or next tick
    let threadStartRequest = frames.find((f) => f.method === "thread/start");
    if (!threadStartRequest) {
      const moreFrames = await waitForFrames(stdin);
      threadStartRequest = moreFrames.find((f) => f.method === "thread/start");
    }
    expect(threadStartRequest).toBeDefined();
    expect(threadStartRequest!.id).toBeDefined();

    // Server responds to thread/start
    serverWrite(stdout, {
      id: threadStartRequest!.id,
      result: { thread: { id: "thr_test_123" } },
    });

    // --- Step 5: Client sends `turn/start` → server responds with success ---
    // Wait for the adapter to wire notification handlers and send turn/start
    await new Promise((r) => setTimeout(r, 20));
    frames = drainFrames(stdin);
    let turnStartRequest = frames.find((f) => f.method === "turn/start");
    if (!turnStartRequest) {
      const moreFrames = await waitForFrames(stdin);
      turnStartRequest = moreFrames.find((f) => f.method === "turn/start");
    }
    expect(turnStartRequest).toBeDefined();
    expect(turnStartRequest!.id).toBeDefined();
    expect((turnStartRequest!.params as any).input).toEqual([
      { type: "text", text: "Write a hello world function" },
    ]);

    // Server responds to turn/start
    serverWrite(stdout, {
      id: turnStartRequest!.id,
      result: { turn: { id: "turn_001" } },
    });

    // --- Step 4 (after handlers wired): Server sends `thread/started` notification ---
    serverWrite(stdout, {
      method: "thread/started",
      params: { thread: { id: "thr_test_123" } },
    });

    // --- Step 6: Server sends `turn/started` notification ---
    serverWrite(stdout, {
      method: "turn/started",
      params: { turn: { id: "turn_001" }, threadId: "thr_test_123" },
    });

    // --- Step 7: Server sends `item/agentMessage/delta` notifications (×3) ---
    await new Promise((r) => setTimeout(r, 10));

    serverWrite(stdout, {
      method: "item/agentMessage/delta",
      params: { itemId: "item_001", delta: "Here is a ", threadId: "thr_test_123", turnId: "turn_001" },
    });
    serverWrite(stdout, {
      method: "item/agentMessage/delta",
      params: { itemId: "item_001", delta: "hello world ", threadId: "thr_test_123", turnId: "turn_001" },
    });
    serverWrite(stdout, {
      method: "item/agentMessage/delta",
      params: { itemId: "item_001", delta: "function.", threadId: "thr_test_123", turnId: "turn_001" },
    });

    // --- Step 8: Server sends `item/completed` notification ---
    serverWrite(stdout, {
      method: "item/completed",
      params: {
        item: {
          type: "agentMessage",
          id: "item_001",
          text: "Here is a hello world function.",
        },
        threadId: "thr_test_123",
        turnId: "turn_001",
      },
    });

    // Usage is a separate notification in the official app-server protocol.
    serverWrite(stdout, {
      method: "thread/tokenUsage/updated",
      params: { threadId: "thr_test_123", turnId: "turn_001", tokenUsage: {
        last: { inputTokens: 150, outputTokens: 42, cachedInputTokens: 10, reasoningOutputTokens: 0, totalTokens: 192 },
        total: { inputTokens: 150, outputTokens: 42, cachedInputTokens: 10, reasoningOutputTokens: 0, totalTokens: 192 },
      } },
    });

    // A repeated total is a replay; another identical-sized response changes
    // total and must be counted even though last is identical.
    for (const input of [150, 300]) {
      serverWrite(stdout, { method: "thread/tokenUsage/updated", params: {
        threadId: "thr_test_123", turnId: "turn_001", tokenUsage: {
          last: { inputTokens: 150, outputTokens: 42, cachedInputTokens: 10 },
          total: { inputTokens: input, outputTokens: input === 150 ? 42 : 84, cachedInputTokens: input === 150 ? 10 : 20, reasoningOutputTokens: 0, totalTokens: input === 150 ? 192 : 384 },
        },
      } });
    }
    serverWrite(stdout, { method: "thread/tokenUsage/updated", params: {
      threadId: "thr_other", turnId: "turn_001", tokenUsage: { last: { inputTokens: 999 }, total: { inputTokens: 999 } },
    } });

    serverWrite(stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_001", status: "completed" },
        threadId: "thr_test_123",
      },
    });

    // Wait for all notifications to be processed
    await new Promise((r) => setTimeout(r, 50));

    // --- Assertions: verify the emitted AgentEvent sequence ---
    const eventTypes = events.map((e) => e.type);

    // Must include thread_started
    expect(eventTypes).toContain("thread_started");
    const threadEvent = events.find((e) => e.type === "thread_started");
    expect(threadEvent!.threadId).toBe("thr_test_123");

    // Must include text deltas
    const textEvents = events.filter((e) => e.type === "text");
    expect(textEvents.length).toBeGreaterThanOrEqual(3);
    expect(textEvents[0].content).toBe("Here is a ");
    expect(textEvents[1].content).toBe("hello world ");
    expect(textEvents[2].content).toBe("function.");

    // Must include usage
    expect(eventTypes).toContain("usage");
    expect(events.filter(event => event.type === "usage")).toHaveLength(2);
    const usageEvent = events.find((e) => e.type === "usage");
    expect(usageEvent!.usage).toEqual({
      inputTokens: 140,
      outputTokens: 42,
      cacheCreationInputTokens: 0,
      cacheReadInputTokens: 10,
    });

    // Must include done
    expect(eventTypes).toContain("done");
    const doneEvent = events.find((e) => e.type === "done");
    expect(doneEvent!.content).toBe("Here is a hello world function.");

    // Verify ordering: thread_started comes before text, text before usage, usage before done
    const threadIdx = eventTypes.indexOf("thread_started");
    const firstTextIdx = eventTypes.indexOf("text");
    const usageIdx = eventTypes.indexOf("usage");
    const doneIdx = eventTypes.indexOf("done");

    expect(threadIdx).toBeLessThan(firstTextIdx);
    expect(firstTextIdx).toBeLessThan(usageIdx);
    expect(usageIdx).toBeLessThan(doneIdx);

    // Regression: Codex can replay item/completed after turn/completed.
    // That late envelope must not repopulate the adapter's current
    // message buffer, or the next turn's done event starts with the
    // previous answer.
    serverWrite(stdout, {
      method: "item/completed",
      params: {
        item: {
          type: "agentMessage",
          id: "item_001",
          text: "Here is a hello world function.",
        },
        threadId: "thr_test_123",
        turnId: "turn_001",
      },
    });
    serverWrite(stdout, {
      method: "turn/started",
      params: { turn: { id: "turn_002" }, threadId: "thr_test_123" },
    });
    serverWrite(stdout, {
      method: "item/agentMessage/delta",
      params: { itemId: "item_001", delta: "Second response.", threadId: "thr_test_123", turnId: "turn_002" },
    });
    serverWrite(stdout, {
      method: "turn/completed",
      params: {
        turn: { id: "turn_002", status: "completed" },
        threadId: "thr_test_123",
      },
    });

    await new Promise((r) => setTimeout(r, 50));

    const doneEvents = events.filter((e) => e.type === "done");
    expect(doneEvents.map((e) => e.content)).toEqual([
      "Here is a hello world function.",
      "Second response.",
    ]);
  });
});
