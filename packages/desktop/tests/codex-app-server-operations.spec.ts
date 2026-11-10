import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import type { AgentEvent, CodexGoal } from "../electron/orchestrator-adapters";

vi.mock("node:child_process", () => ({
  spawn: vi.fn(), execFile: vi.fn(), execFileSync: vi.fn(() => "/usr/bin"),
}));
import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";

describe("Codex app-server operation contracts", () => {
  let adapter: CodexAppServerAdapter;
  let stdout: PassThrough;
  let requests: Array<{ method: string; params: Record<string, unknown> }>;
  let responses: Record<string, unknown>;
  let events: AgentEvent[];
  let replies: unknown[];
  let holdReview: boolean;
  let pendingReviewId: number | undefined;
  let holdGoalRead: boolean;
  let closeNative: () => void;

  beforeEach(async () => {
    vi.useFakeTimers();
    const stdin = new PassThrough();
    stdout = new PassThrough();
    const child = Object.assign(new EventEmitter(), { stdin, stdout, stderr: new PassThrough(), kill: vi.fn() });
    closeNative = () => { child.emit("close", 0); };
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
    requests = [];
    replies = [];
    responses = { "thread/start": { thread: { id: "thread-original" } }, "turn/start": { turn: { id: "turn-1" } } };
    holdReview = false;
    pendingReviewId = undefined;
    holdGoalRead = false;
    stdin.on("data", chunk => {
      for (const line of String(chunk).trim().split("\n")) {
        const frame = JSON.parse(line);
        if (frame.id !== undefined && !frame.method) replies.push(frame);
        if (frame.id === undefined || !frame.method) continue;
        requests.push(frame);
        if (frame.method === "command/exec") continue;
        if (frame.method === "thread/goal/get" && holdGoalRead) continue;
        if (frame.method === "review/start" && holdReview) { pendingReviewId = frame.id; continue; }
        stdout.write(JSON.stringify({ id: frame.id, result: responses[frame.method] ?? {} }) + "\n");
      }
    });
    adapter = new CodexAppServerAdapter();
    events = [];
    adapter.onEvent(event => events.push(event));
    adapter.spawn("Initial prompt", "/tmp/contract-test");
    await vi.advanceTimersByTimeAsync(0);
  });

  afterEach(async () => {
    adapter.cancel();
    await vi.runAllTimersAsync();
    vi.useRealTimers();
  });

  it("opts into the experimental methods exposed by the product", () => {
    expect(requests[0].params).toMatchObject({ capabilities: { experimentalApi: true } });
  });

  it("sets, reads and clears the official goal object with user provenance", async () => {
    const native: CodexGoal = { threadId: "thread-original", objective: "Complete the task", status: "active", tokenBudget: 1200, tokensUsed: 15, timeUsedSeconds: 2, createdAt: 1, updatedAt: 2 };
    responses["thread/goal/set"] = responses["thread/goal/get"] = { goal: native };
    await adapter.setGoal({ objective: "Complete the task", status: "active", tokenBudget: 1200 });
    const goal = await adapter.getGoal();
    await adapter.clearGoal();
    expect({ requests: requests.slice(-3), goal }).toEqual({
      requests: [
        { id: expect.any(Number), method: "thread/goal/set", params: { threadId: "thread-original", objective: "Complete the task", status: "active", tokenBudget: 1200, origin: "user" } },
        { id: expect.any(Number), method: "thread/goal/get", params: { threadId: "thread-original" } },
        { id: expect.any(Number), method: "thread/goal/clear", params: { threadId: "thread-original", origin: "user" } },
      ], goal: native,
    });
    expect(events.filter(event => event.type === "goal_updated").map(event => event.goal)).toEqual([native, null]);
  });

  it("projects native goal accounting and terminal status without flattening fields", () => {
    const goal: CodexGoal = { threadId: "thread-original", objective: "Finish", status: "budgetLimited", tokenBudget: 100, tokensUsed: 105, timeUsedSeconds: 30, createdAt: 1, updatedAt: 31 };
    for (const threadId of ["other-thread", "thread-original", "thread-original"]) stdout.write(JSON.stringify({ method: "thread/goal/updated", params: { threadId, goal } }) + "\n");
    stdout.write(JSON.stringify({ method: "thread/goal/cleared", params: { threadId: "thread-original" } }) + "\n");
    expect(events.filter(event => event.type === "goal_updated").map(event => event.goal)).toEqual([goal, null]);
  });

  it("bounds a silent metadata read without interrupting the provider turn", async () => {
    holdGoalRead = true;
    const read = adapter.getGoal().catch(error => error);
    await vi.advanceTimersByTimeAsync(30_000);
    expect({ error: (await read).message, interrupted: requests.some(request => request.method === "turn/interrupt") }).toEqual({
      error: "request timeout: thread/goal/get", interrupted: false,
    });
  });

  it("reports unexpected process closure between autonomous goal turns as one failure", async () => {
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "turn-1", status: "completed" } } }) + "\n");
    responses["thread/goal/set"] = { goal: { threadId: "thread-original", objective: "Still active", status: "active" } };
    await adapter.setGoal({ objective: "Still active", status: "active" });
    closeNative();
    expect(events.filter(event => event.type === "done").map(event => event.outcome)).toEqual(["completed", "failed"]);
  });

  it("round-trips UTF-8 file contents through base64 fields", async () => {
    const content = "Crab 🦀\n\u0000";
    const dataBase64 = Buffer.from(content).toString("base64");
    responses["fs/readFile"] = { dataBase64 };
    await adapter.fsWriteFile("/tmp/proof.txt", content);
    const read = await adapter.fsReadFile("/tmp/proof.txt");
    expect({ requests: requests.slice(-2), read }).toEqual({
      requests: [
        { id: expect.any(Number), method: "fs/writeFile", params: { path: "/tmp/proof.txt", dataBase64 } },
        { id: expect.any(Number), method: "fs/readFile", params: { path: "/tmp/proof.txt" } },
      ], read: content,
    });
  });

  it("uses official metadata and copy fields", async () => {
    await adapter.fsGetMetadata("/tmp/source");
    await adapter.fsCopy("/tmp/source", "/tmp/destination");
    expect(requests.slice(-2)).toEqual([
      { id: expect.any(Number), method: "fs/getMetadata", params: { path: "/tmp/source" } },
      { id: expect.any(Number), method: "fs/copy", params: { sourcePath: "/tmp/source", destinationPath: "/tmp/destination" } },
    ]);
  });

  it("supplies a watch id and emits every changed path", async () => {
    const watchId = await adapter.fsWatch("/tmp/watched");
    stdout.write(JSON.stringify({ method: "fs/changed", params: { watchId, changedPaths: ["/tmp/watched/a", "/tmp/watched/b"] } }) + "\n");
    await adapter.fsUnwatch(watchId);
    expect({ watch: requests.at(-2)?.params, paths: events.filter(event => event.type === "filesystem_change").map(event => event.path) }).toEqual({
      watch: { path: "/tmp/watched", watchId }, paths: ["/tmp/watched/a", "/tmp/watched/b"],
    });
  });

  it("starts a native review with task-scoped instructions and tracks its terminal turn", async () => {
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "turn-1", status: "completed" } } }) + "\n");
    responses["review/start"] = { turn: { id: "review-1" } };
    await adapter.review("Review the complete task diff", "/tmp/contract-test");
    stdout.write(JSON.stringify({ method: "turn/started", params: { threadId: "thread-original", turn: { id: "review-child" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "item/completed", params: { threadId: "thread-original", turnId: "review-child", item: { type: "commandExecution", id: "review-command", command: "git diff", status: "completed", aggregatedOutput: "Inspected changes" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "review-child", status: "completed" } } }) + "\n");
    expect(events.filter(event => event.type === "done")).toHaveLength(1);
    stdout.write(JSON.stringify({ method: "item/completed", params: { threadId: "thread-original", turnId: "review-1", item: { type: "exitedReviewMode", id: "review-output", review: "A concrete finding" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "review-1", status: "completed" } } }) + "\n");
    expect({ request: requests.at(-1), output: events.filter(event => event.type === "text").map(event => event.content), terminal: events.at(-1) }).toMatchObject({
      request: { method: "review/start", params: { threadId: "thread-original", target: { type: "custom", instructions: "Review the complete task diff" } } },
      output: ["A concrete finding"], terminal: { type: "done", outcome: "completed", turnId: "review-1" },
    });
  });

  it("publishes review output before a delayed acknowledgement without reviving a completed delegate", async () => {
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "turn-1", status: "completed" } } }) + "\n");
    holdReview = true;
    const submitted = adapter.review("Review before acknowledgement", "/tmp/contract-test");
    await vi.advanceTimersByTimeAsync(0);
    stdout.write(JSON.stringify({ method: "turn/started", params: { threadId: "thread-original", turn: { id: "delegate" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "item/started", params: { threadId: "thread-original", turnId: "parent-review", item: { type: "enteredReviewMode", id: "review-entry", review: "Task changes" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "item/completed", params: { threadId: "thread-original", turnId: "parent-review", item: { type: "enteredReviewMode", id: "review-entry", review: "Task changes" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "item/completed", params: { threadId: "thread-original", turnId: "parent-review", item: { type: "exitedReviewMode", id: "review-exit", review: "Finding delivered before ack" } } }) + "\n");
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "parent-review", status: "completed" } } }) + "\n");
    expect(events.filter(event => event.type === "done").at(-1)).toMatchObject({ turnId: "parent-review", outcome: "completed" });
    stdout.write(JSON.stringify({ id: pendingReviewId, result: { turn: { id: "parent-review" } } }) + "\n");
    await submitted;
    await adapter.iterate("Next input");
    expect({ text: events.filter(event => event.type === "text").map(event => event.content), next: requests.at(-1)?.method,
      entries: events.filter(event => event.tool === "CodexReview").length }).toEqual({
      text: ["Finding delivered before ack"], next: "turn/start", entries: 1,
    });
  });

  it("interrupts the native review delegate and rejects same-turn steering", async () => {
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "turn-1", status: "completed" } } }) + "\n");
    responses["review/start"] = { turn: { id: "parent-review" } };
    await adapter.review("Review task changes", "/tmp/contract-test");
    stdout.write(JSON.stringify({ method: "turn/started", params: { threadId: "thread-original", turn: { id: "delegate" } } }) + "\n");
    await expect(adapter.iterate("Cannot steer a review")).rejects.toThrow("Wait for the review to finish");
    adapter.cancel();
    await vi.advanceTimersByTimeAsync(0);
    expect(requests.at(-1)).toMatchObject({ method: "turn/interrupt", params: { threadId: "thread-original", turnId: "delegate" } });
  });

  it("keeps the source thread selected after a fork notification", async () => {
    responses["thread/fork"] = { thread: { id: "thread-fork" } };
    await adapter.forkThread();
    stdout.write(JSON.stringify({ method: "thread/started", params: { thread: { id: "thread-fork" } } }) + "\n");
    await adapter.setThreadName("Original");
    expect(requests.at(-1)?.params.threadId).toBe("thread-original");
  });

  it("accepts an empty standalone MCP form after turn completion and deduplicates its request identity", () => {
    const frame = { id: 0, method: "mcpServer/elicitation/request", params: { threadId: "thread-original", turnId: null, serverName: "Fixture MCP", mode: "form", message: "Confirm", requestedSchema: { type: "object", properties: {} } } };
    stdout.write(JSON.stringify(frame) + "\n");
    stdout.write(JSON.stringify(frame) + "\n");
    stdout.write(JSON.stringify({ method: "turn/completed", params: { threadId: "thread-original", turn: { id: "turn-1", status: "completed" } } }) + "\n");
    const delivered = adapter.respondInput("0", { kind: "elicitation", action: "accept", content: {} });
    expect({ delivered, requests: events.filter(event => event.type === "input_request").length, replies }).toEqual({ delivered: true, requests: 1, replies: [{ id: 0, result: { action: "accept", content: {} } }] });
  });

  it("rejects invalid MCP contents without resolving the pending form", () => {
    stdout.write(JSON.stringify({ id: "form", method: "mcpServer/elicitation/request", params: {
      threadId: "thread-original", serverName: "Fixture MCP", mode: "form", message: "Enter a number", requestedSchema: { type: "object", properties: { count: { type: "integer", minimum: 1 } }, required: ["count"] },
    } }) + "\n");
    const id = JSON.stringify("form");
    expect(() => adapter.respondInput(id, { kind: "elicitation", action: "accept", content: { count: 1.5 } })).toThrow("integer");
    adapter.respondInput(id, { kind: "elicitation", action: "accept", content: { count: 2 } });
    expect(replies).toEqual([{ id: "form", result: { action: "accept", content: { count: 2 } } }]);
  });

  it.each(["decline", "cancel"] as const)("returns MCP URL %s without granting or sending contents", action => {
    stdout.write(JSON.stringify({ id: "url", method: "mcpServer/elicitation/request", params: {
      threadId: "thread-original", serverName: "Fixture MCP", mode: "url", message: "Open the page", elicitationId: "url-flow", url: "https://auth.example.invalid/connect",
    } }) + "\n");
    adapter.respondInput(JSON.stringify("url"), { kind: "elicitation", action });
    expect(replies).toEqual([{ id: "url", result: { action, content: null } }]);
  });

  it("answers a typed question on its original server request and rejects stale answers", () => {
    stdout.write(JSON.stringify({ id: "question-1", method: "item/tool/requestUserInput", params: {
      threadId: "thread-original", turnId: "turn-1", itemId: "question-tool", isBlocking: true,
      questions: [{ id: "mode", header: "Mode", question: "Which mode?", options: [{ label: "Thorough", description: "Run all checks" }], isOther: true }],
    } }) + "\n");
    const requestId = JSON.stringify("question-1");
    const delivered = adapter.respondInput(requestId, { kind: "questions", answers: { mode: ["Thorough"] } });
    expect({ delivered, replies, repeated: adapter.respondInput(requestId, { kind: "questions", answers: {} }) }).toEqual({
      delivered: true, replies: [{ id: "question-1", result: { answers: { mode: { answers: ["Thorough"] } } } }], repeated: false,
    });
  });

  it("returns only an offered approval decision, preserving policy amendments unchanged", () => {
    const amendment = { applyNetworkPolicyAmendment: { network_policy_amendment: { action: "deny", host: "example.invalid" } } };
    stdout.write(JSON.stringify({ id: 900, method: "item/commandExecution/requestApproval", params: {
      threadId: "thread-original", turnId: "turn-1", itemId: "command-tool", command: "curl example.invalid", availableDecisions: ["accept", amendment, "decline"],
    } }) + "\n");
    const rejected = adapter.respondApproval("900", "allow");
    const delivered = adapter.respondApproval("900", "network:1");
    expect({ rejected, delivered, replies }).toEqual({ rejected: false, delivered: true, replies: [{ id: 900, result: { decision: amendment } }] });
  });

  it("returns a command id before exit, streams UTF-8, and waits for the completion response", async () => {
    const { sessionId } = await adapter.execCommand({ command: "program", args: ["--argument"], cwd: "/tmp" });
    const request = requests.at(-1);
    const bytes = Buffer.from("🦀");
    for (const chunk of [bytes.subarray(0, 2), bytes.subarray(2)]) {
      stdout.write(JSON.stringify({ method: "command/exec/outputDelta", params: { processId: sessionId, stream: "stdout", deltaBase64: chunk.toString("base64"), capReached: false } }) + "\n");
    }
    await vi.advanceTimersByTimeAsync(60_000);
    expect(events.some(event => event.type === "command_exited")).toBe(false);
    stdout.write(JSON.stringify({ id: (request as { id?: number })?.id, result: { exitCode: 7, stdout: "", stderr: "" } }) + "\n");
    await vi.advanceTimersByTimeAsync(0);
    expect({ params: request?.params, output: events.filter(event => event.type === "command_output").map(event => event.content).join(""), exit: events.find(event => event.type === "command_exited")?.exitCode }).toEqual({
      params: { processId: sessionId, command: ["program", "--argument"], cwd: "/tmp", tty: true }, output: "🦀", exit: 7,
    });
  });

  it("sends command stdin, resize and terminate as requests with official fields", async () => {
    const { sessionId } = await adapter.execCommand({ command: "program" });
    await adapter.writeCommandStdin(sessionId, "🦀", true);
    await adapter.resizeCommandPty(sessionId, 80, 24);
    await adapter.terminateCommand(sessionId);
    expect(requests.slice(-3)).toEqual([
      { id: expect.any(Number), method: "command/exec/write", params: { processId: sessionId, deltaBase64: Buffer.from("🦀").toString("base64"), closeStdin: true } },
      { id: expect.any(Number), method: "command/exec/resize", params: { processId: sessionId, size: { cols: 80, rows: 24 } } },
      { id: expect.any(Number), method: "command/exec/terminate", params: { processId: sessionId } },
    ]);
  });
});
