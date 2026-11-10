/**
 * Approval round-trip test for CodexAppServerAdapter.
 *
 * Verifies that server-initiated approval requests are correctly routed
 * through the adapter and that respondApproval() writes the correct
 * JSON-RPC response frame with the mapped decision.
 *
 * Validates: Requirements CA9.3, CA5.2
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { PassThrough } from "node:stream";
import { EventEmitter } from "node:events";
import type { AgentEvent } from "../electron/orchestrator-adapters";

// We need to mock child_process.spawn before importing the adapter.
vi.mock("node:child_process", () => ({
  spawn: vi.fn(),
  execFile: vi.fn(),
  execFileSync: vi.fn(() => "/usr/bin:/usr/local/bin"),
}));

import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

/** Creates a fake child process with PassThrough streams for stdin/stdout/stderr. */
function createFakeProcess() {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const stderr = new PassThrough();
  const proc = new EventEmitter() as any;
  proc.stdin = stdin;
  proc.stdout = stdout;
  proc.stderr = stderr;
  proc.pid = 12345;
  proc.kill = vi.fn();
  return { proc, stdin, stdout, stderr };
}

/** Read all frames written to stdin (adapter → server direction). */
function drainFrames(stdin: PassThrough): string[] {
  const buf = stdin.read();
  if (!buf) return [];
  return buf.toString().split("\n").filter((l: string) => l.length > 0);
}

/** Write a server message to stdout (server → adapter direction). */
function serverWrite(stdout: PassThrough, msg: object): void {
  stdout.write(JSON.stringify(msg) + "\n");
}

/**
 * Completes the full initialization handshake sequence:
 * initialize → initialized → thread/start → thread/started → turn/start → turn/started
 *
 * Returns after the adapter is fully initialized and ready to receive
 * server requests.
 */
async function completeHandshake(
  stdin: PassThrough,
  stdout: PassThrough,
): Promise<void> {
  // Wait for the initialize request
  await new Promise(r => setTimeout(r, 20));
  let frames = drainFrames(stdin);
  // Find the initialize request
  const initFrame = frames.find(f => {
    try { return JSON.parse(f).method === "initialize"; } catch { return false; }
  });
  if (!initFrame) throw new Error("No initialize request found");
  const initReq = JSON.parse(initFrame);

  // Respond to initialize
  serverWrite(stdout, {
    id: initReq.id,
    result: { serverInfo: { name: "codex", version: "0.1.0" } },
  });

  // Wait for initialized notification + thread/start request
  await new Promise(r => setTimeout(r, 20));
  frames = drainFrames(stdin);

  // Find thread/start request
  const threadFrame = frames.find(f => {
    try { return JSON.parse(f).method === "thread/start"; } catch { return false; }
  });
  if (!threadFrame) throw new Error("No thread/start request found");
  const threadReq = JSON.parse(threadFrame);

  // Respond to thread/start
  serverWrite(stdout, {
    id: threadReq.id,
    result: { thread: { id: "thr_test_1" } },
  });

  // Send thread/started notification
  serverWrite(stdout, {
    method: "thread/started",
    params: { thread: { id: "thr_test_1" } },
  });

  // Wait for turn/start request
  await new Promise(r => setTimeout(r, 20));
  frames = drainFrames(stdin);

  // Find turn/start request
  const turnFrame = frames.find(f => {
    try { return JSON.parse(f).method === "turn/start"; } catch { return false; }
  });
  if (!turnFrame) throw new Error("No turn/start request found");
  const turnReq = JSON.parse(turnFrame);

  // Respond to turn/start
  serverWrite(stdout, {
    id: turnReq.id,
    result: { turn: { id: "turn_1" } },
  });

  // Send turn/started notification
  serverWrite(stdout, {
    method: "turn/started",
    params: { turn: { id: "turn_1" } },
  });

  // Allow all handlers to process
  await new Promise(r => setTimeout(r, 20));
}

describe("CodexAppServerAdapter — Approval round-trip (CA9.3, CA5.2)", () => {
  let adapter: CodexAppServerAdapter;
  let fakeProc: ReturnType<typeof createFakeProcess>;
  let events: AgentEvent[];

  beforeEach(async () => {
    fakeProc = createFakeProcess();
    (spawn as any).mockReturnValue(fakeProc.proc);

    adapter = new CodexAppServerAdapter();
    events = [];
    adapter.onEvent((event) => events.push(event));

    // Spawn the adapter (triggers initialization)
    adapter.spawn("Hello, write a test", "/tmp/test-cwd");

    // Complete the handshake
    await completeHandshake(fakeProc.stdin, fakeProc.stdout);
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it.each([["allow_once", "turn"], ["allow", "session"], ["deny", "turn"]])("returns exactly the requested profile for %s with %s scope", async (decision, scope) => {
    const permissions = { network: { enabled: true }, fileSystem: { entries: [{ path: { type: "path", path: "/project/reference" }, access: "read" }, { path: { type: "path", path: "/project/output" }, access: "write" }] } };
    serverWrite(fakeProc.stdout, { id: "permissions", method: "item/permissions/requestApproval", params: {
      itemId: "permissions-item", cwd: "/project", permissions, reason: "Read reference and write output",
    } });
    await new Promise(resolve => setTimeout(resolve, 10));
    drainFrames(fakeProc.stdin);
    adapter.respondApproval(JSON.stringify("permissions"), decision);
    expect(drainFrames(fakeProc.stdin).map(frame => JSON.parse(frame))).toEqual([{ id: "permissions", result: { permissions: decision === "deny" ? {} : permissions, scope } }]);
  });

  it("allow → acceptForSession", async () => {
    // Server sends an approval request with rpc id 100
    serverWrite(fakeProc.stdout, {
      id: 100,
      method: "item/commandExecution/requestApproval",
      params: {
        itemId: "item-command",
        threadId: "thread-1",
        turnId: "turn-1",
        startedAtMs: 1,
        command: "rm -rf /",
        reason: "Destructive command",
      },
    });

    // Wait for the adapter to process and emit the approval_request event
    await new Promise(r => setTimeout(r, 20));

    const approvalEvent = events.find(e => e.type === "approval_request");
    expect(approvalEvent).toBeDefined();
    expect(approvalEvent!.approval!.requestId).toBe("100");
    expect(approvalEvent!.approval!.kind).toBe("command");

    // Drain any frames written during handshake/approval processing
    drainFrames(fakeProc.stdin);

    // Respond with "allow" decision
    adapter.respondApproval!("100", "allow");

    // Read the response frame from stdin
    await new Promise(r => setTimeout(r, 10));
    const frames = drainFrames(fakeProc.stdin);
    expect(frames.length).toBe(1);
    const response = JSON.parse(frames[0]);
    expect(response).toEqual({
      id: 100,
      result: { decision: "acceptForSession" },
    });
  });

  it("allow_once → accept", async () => {
    // Server sends an approval request with rpc id 100
    serverWrite(fakeProc.stdout, {
      id: 100,
      method: "item/commandExecution/requestApproval",
      params: {
        itemId: "item-command",
        threadId: "thread-1",
        turnId: "turn-1",
        startedAtMs: 1,
        command: "rm -rf /",
        reason: "Destructive command",
      },
    });

    // Wait for the adapter to process
    await new Promise(r => setTimeout(r, 20));

    const approvalEvent = events.find(e => e.type === "approval_request");
    expect(approvalEvent).toBeDefined();
    expect(approvalEvent!.approval!.requestId).toBe("100");

    // Drain any frames
    drainFrames(fakeProc.stdin);

    // Respond with "allow_once" decision
    adapter.respondApproval!("100", "allow_once");

    // Read the response frame
    await new Promise(r => setTimeout(r, 10));
    const frames = drainFrames(fakeProc.stdin);
    expect(frames.length).toBe(1);
    const response = JSON.parse(frames[0]);
    expect(response).toEqual({
      id: 100,
      result: { decision: "accept" },
    });
  });

  it("deny → decline", async () => {
    // Server sends an approval request with rpc id 100
    serverWrite(fakeProc.stdout, {
      id: 100,
      method: "item/commandExecution/requestApproval",
      params: {
        itemId: "item-command",
        threadId: "thread-1",
        turnId: "turn-1",
        startedAtMs: 1,
        command: "rm -rf /",
        reason: "Destructive command",
      },
    });

    // Wait for the adapter to process
    await new Promise(r => setTimeout(r, 20));

    const approvalEvent = events.find(e => e.type === "approval_request");
    expect(approvalEvent).toBeDefined();
    expect(approvalEvent!.approval!.requestId).toBe("100");

    // Drain any frames
    drainFrames(fakeProc.stdin);

    // Respond with "deny" decision
    adapter.respondApproval!("100", "deny");

    // Read the response frame
    await new Promise(r => setTimeout(r, 10));
    const frames = drainFrames(fakeProc.stdin);
    expect(frames.length).toBe(1);
    const response = JSON.parse(frames[0]);
    expect(response).toEqual({
      id: 100,
      result: { decision: "decline" },
    });
  });

  it("respondApproval with unknown requestId is a no-op", async () => {
    // Drain any frames from handshake
    drainFrames(fakeProc.stdin);

    // Call respondApproval with a requestId that doesn't exist
    adapter.respondApproval!("nonexistent-id", "allow");

    await new Promise(r => setTimeout(r, 10));
    const frames = drainFrames(fakeProc.stdin);
    // No frame should be written
    expect(frames.length).toBe(0);
  });

  it("correlates simultaneous approvals using their wire ids, including string ids", async () => {
    for (const id of [301, "301"]) {
      serverWrite(fakeProc.stdout, {
        id, method: "item/commandExecution/requestApproval",
        params: { itemId: "item-command", threadId: "thread-1", turnId: "turn-1", startedAtMs: 1, command: "pwd" },
      });
    }
    await new Promise(resolve => setTimeout(resolve, 20));
    adapter.respondApproval("301", "allow_once");
    adapter.respondApproval(JSON.stringify("301"), "deny");
    expect(drainFrames(fakeProc.stdin).map(frame => JSON.parse(frame))).toEqual([
      { id: 301, result: { decision: "accept" } },
      { id: "301", result: { decision: "decline" } },
    ]);
  });

  it("file change approval also works correctly", async () => {
    // Server sends a file change approval request
    serverWrite(fakeProc.stdout, {
      id: 200,
      method: "item/fileChange/requestApproval",
      params: {
        itemId: "item-file",
        threadId: "thread-1",
        turnId: "turn-1",
        startedAtMs: 1,
        grantRoot: "/etc/passwd",
        reason: "Path outside workspace",
      },
    });

    // Wait for the adapter to process
    await new Promise(r => setTimeout(r, 20));

    const approvalEvent = events.find(e =>
      e.type === "approval_request" && e.approval?.requestId === "200"
    );
    expect(approvalEvent).toBeDefined();
    expect(approvalEvent!.approval!.kind).toBe("file");
    expect(approvalEvent!.approval!.summary).toContain("/etc/passwd");

    // Drain any frames
    drainFrames(fakeProc.stdin);

    // Respond with "allow" decision
    adapter.respondApproval!("200", "allow");

    await new Promise(r => setTimeout(r, 10));
    const frames = drainFrames(fakeProc.stdin);
    expect(frames.length).toBe(1);
    const response = JSON.parse(frames[0]);
    expect(response).toEqual({
      id: 200,
      result: { decision: "acceptForSession" },
    });
  });
});
