import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { AgentSubmissionUncertainError, type AgentEvent } from "../electron/orchestrator-adapters";

vi.mock("node:child_process", () => ({ spawn: vi.fn(), execFile: vi.fn(), execFileSync: vi.fn(() => "/usr/bin") }));
import { spawn } from "node:child_process";
import type { AgentProcessFactory } from "../electron/agent-process";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";

interface Frame { type: string; request_id?: string; request?: Record<string, unknown>; message?: unknown; response?: unknown }

describe("Claude Code bidirectional transport", () => {
  let adapter: ClaudeCodeAdapter;
  let child: EventEmitter & { stdin: PassThrough; stdout: PassThrough; stderr: PassThrough; kill: ReturnType<typeof vi.fn> };
  let events: AgentEvent[];

  function drain(): Frame[] { return (child.stdin.read()?.toString() ?? "").trim().split("\n").filter(Boolean).map((line: string) => JSON.parse(line)); }
  function write(frame: object): void { child.stdout.write(JSON.stringify(frame) + "\n"); }
  function initialize(): Frame[] {
    const init = drain()[0];
    write({ type: "control_response", response: { subtype: "success", request_id: init.request_id, response: {} } });
    return drain();
  }

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
    const factory: AgentProcessFactory = (binary, args, options) => {
      const process = spawn(binary, args, { ...options, stdio: "pipe" });
      return { process, closed: new Promise<void>(resolve => process.once("close", () => resolve())) };
    };
    adapter = new ClaudeCodeAdapter([], "claude", factory);
    events = [];
    adapter.onEvent(event => events.push(event));
  });

  afterEach(async () => {
    adapter.cancel();
    child.emit("close", 0);
    await vi.runAllTimersAsync();
    vi.useRealTimers();
  });

  it("keeps startup pending until initial controls acknowledge and the user input is written", async () => {
    const startup = adapter.spawn("Initial input", "/project", undefined, { model: "selected-model" });
    let settled = false;
    void startup.then(() => { settled = true; }, () => {});
    const controls = initialize();
    await vi.advanceTimersByTimeAsync(0);
    expect({ settled, controls: controls.map(frame => frame.request) }).toEqual({ settled: false, controls: [{ subtype: "set_model", model: "selected-model" }] });
    write({ type: "control_response", response: { subtype: "success", request_id: controls[0].request_id, response: {} } });
    await startup;
    expect(drain().map(frame => frame.message)).toEqual([{ role: "user", content: "Initial input" }]);
  });

  it("initializes before sending user input, preserves stdin, and reuses the session for follow-ups", async () => {
    adapter.spawn("First prompt", "/project");
    const first = initialize();
    write({ type: "system", subtype: "init", session_id: "session-live" });
    write({ type: "result", subtype: "success", result: "First answer" });
    await adapter.iterate("Follow-up");
    expect({ args: vi.mocked(spawn).mock.calls[0][1], first: first[0], next: drain()[0], closed: child.stdin.writableEnded, spawns: vi.mocked(spawn).mock.calls.length }).toEqual({
      args: ["--print", "--verbose", "--output-format", "stream-json", "--input-format", "stream-json", "--include-partial-messages", "--permission-prompt-tool", "stdio"],
      first: { type: "user", message: { role: "user", content: "First prompt" }, parent_tool_use_id: null },
      next: { type: "user", message: { role: "user", content: "Follow-up" }, parent_tool_use_id: null, session_id: "session-live" },
      closed: false, spawns: 1,
    });
  });

  it("allows slow initialization to finish without submitting the user message early", async () => {
    adapter.spawn("Wait for startup", "/project");
    const init = drain()[0];
    await vi.advanceTimersByTimeAsync(45_000);
    expect({ messages: drain(), failed: events.some(event => event.type === "error"), killed: child.kill.mock.calls.length }).toEqual({ messages: [], failed: false, killed: 0 });
    write({ type: "control_response", response: { subtype: "success", request_id: init.request_id, response: {} } });
    expect(drain()[0].message).toEqual({ role: "user", content: "Wait for startup" });
  });

  it("waits for a slow native control until the owner cancels the connection", async () => {
    adapter.spawn("First", "/project");
    initialize();
    write({ type: "result", subtype: "success", result: "Complete" });
    const submission = adapter.iterate("Next", { model: "selected-model" });
    const failure = expect(submission).rejects.toThrow("connection closed");
    await vi.advanceTimersByTimeAsync(45_000);
    expect(drain().map(frame => frame.type)).toEqual(["control_request"]);
    adapter.cancel();
    await failure;
  });

  it.each([true, false])("waits for a model control acknowledgement before sending the next user message: %s", async accepted => {
    adapter.spawn("First prompt", "/project");
    initialize();
    write({ type: "result", subtype: "success", result: "First answer" });
    const iteration = adapter.iterate("New model prompt", { model: "configured-model" });
    const failure = accepted ? undefined : expect(iteration).rejects.toThrow("Model unavailable");
    await vi.advanceTimersByTimeAsync(0);
    const control = drain();
    write({ type: "control_response", response: { request_id: control[0].request_id, subtype: accepted ? "success" : "error", error: accepted ? undefined : "Model unavailable" } });
    if (accepted) await iteration; else await failure;
    expect({ control: control.map(frame => frame.request), messages: drain().map(frame => frame.message) }).toEqual({
      control: [{ subtype: "set_model", model: "configured-model" }],
      messages: accepted ? [{ role: "user", content: "New model prompt" }] : [],
    });
  });

  it("resumes the saved session explicitly at process startup", () => {
    adapter.resumeSessionId = "saved-session";
    adapter.spawn("Continue", "/project");
    initialize();
    expect(vi.mocked(spawn).mock.calls[0][1]).toEqual(expect.arrayContaining(["--resume", "saved-session"]));
  });

  it("preserves an uncertain write failure instead of reporting definitive rejection", async () => {
    adapter.spawn("Initial", "/project");
    initialize();
    write({ type: "result", subtype: "success", result: "Done" });
    const cause = new Error("Pipe failed during write");
    vi.spyOn(child.stdin, "write").mockImplementationOnce(() => { throw cause; });
    const error = await adapter.iterate("Possibly accepted").catch((error: unknown) => error);
    expect({ uncertain: error instanceof AgentSubmissionUncertainError, cause: error instanceof Error ? error.cause : undefined }).toEqual({ uncertain: true, cause });
  });

  it.each([true, false])("restores pending initialization prompts once across live redelivery: %s", liveFirst => {
    adapter.spawn("Continue", "/project");
    const init = drain()[0];
    const permission = { type: "control_request", request_id: "restored-permission", request: { subtype: "can_use_tool", tool_name: "Bash", input: { command: "pwd" } } };
    const elicitation = { type: "control_request", request_id: "restored-form", request: { subtype: "elicitation", mcp_server_name: "Fixture", message: "Confirm", requested_schema: { type: "object", properties: {} } } };
    if (liveFirst) { write(permission); write(elicitation); }
    write({ type: "control_response", response: { subtype: "success", request_id: init.request_id, response: {}, pending_permission_requests: [permission], pending_elicitation_requests: [elicitation] } });
    adapter.respondApproval(permission.request_id, "deny");
    adapter.respondInput(elicitation.request_id, { kind: "elicitation", action: "decline" });
    write(permission);
    write(elicitation);
    expect(events.filter(event => event.type === "approval_request" || event.type === "input_request").map(event => event.approval?.requestId ?? event.inputRequest?.requestId)).toEqual(["restored-permission", "restored-form"]);
  });

  it.each(["allow", "allow_once", "deny"] as const)("answers tool permission requests live: %s", decision => {
    adapter.spawn("Run the task", "/project");
    initialize();
    const input = { command: "pwd" };
    const suggestion = { type: "addRules", rules: [{ toolName: "Bash", ruleContent: "pwd" }], behavior: "allow", destination: "localSettings" };
    write({ type: "control_request", request_id: "approval-1", request: { subtype: "can_use_tool", tool_name: "Bash", input, permission_suggestions: [suggestion] } });
    const delivered = adapter.respondApproval("approval-1", decision);
    const response = decision === "deny" ? { behavior: "deny", message: "User denied the tool call" }
      : { behavior: "allow", updatedInput: input, ...(decision === "allow" ? { updatedPermissions: [{ ...suggestion, destination: "session" }] } : {}) };
    expect({ delivered, frame: drain()[0], repeated: adapter.respondApproval("approval-1", decision) }).toEqual({
      delivered: true, frame: { type: "control_response", response: { subtype: "success", request_id: "approval-1", response } }, repeated: false,
    });
  });

  it("does not acknowledge a permission request cancelled by Claude", () => {
    adapter.spawn("Run the task", "/project");
    initialize();
    write({ type: "control_request", request_id: "cancelled", request: { subtype: "can_use_tool", tool_name: "Read", input: {} } });
    write({ type: "control_cancel_request", request_id: "cancelled" });
    expect(adapter.respondApproval("cancelled", "allow_once")).toBe(false);
  });

  it("routes typed MCP answers to the original control request without another user message", () => {
    adapter.spawn("Initial prompt", "/project");
    initialize();
    const frame = { type: "control_request", request_id: "mcp-form", request: { subtype: "elicitation", mcp_server_name: "Fixture MCP", message: "Enter values", requested_schema: {
      type: "object", properties: { count: { type: "integer", minimum: 0 }, enabled: { type: "boolean", default: false } }, required: ["count", "enabled"],
    } } };
    write(frame);
    write(frame);
    const delivered = adapter.respondInput("mcp-form", { kind: "elicitation", action: "accept", content: { count: 0, enabled: false } });
    expect({ delivered, forms: events.filter(event => event.type === "input_request").length, frames: drain() }).toEqual({ delivered: true, forms: 1, frames: [
      { type: "control_response", response: { subtype: "success", request_id: "mcp-form", response: { action: "accept", content: { count: 0, enabled: false } } } },
    ] });
  });

  it("leaves an undeclared opaque user dialog unanswered", () => {
    adapter.spawn("Initial prompt", "/project");
    initialize();
    write({ type: "control_request", request_id: "other-client-dialog", request: { subtype: "request_user_dialog", dialog_kind: "undeclared-kind", payload: {} } });
    expect(drain()).toEqual([]);
  });

  it("returns every answer to the original AskUserQuestion request without another user turn", () => {
    adapter.spawn("Ask questions", "/project");
    initialize();
    const input = { questions: [
      { question: "Which mode?", header: "Mode", options: [{ label: "Fast" }, { label: "Thorough" }] },
      { question: "Which checks?", header: "Checks", multiSelect: true, options: [{ label: "Build" }, { label: "Test" }] },
    ] };
    write({ type: "control_request", request_id: "question-1", request: { subtype: "can_use_tool", tool_name: "AskUserQuestion", tool_use_id: "tool-question", input } });
    const delivered = adapter.respondInput("question-1", { kind: "questions", answers: { "0": ["Thorough"], "1": ["Build", "Test"] } });
    expect({ delivered, frame: drain()[0], event: events.find(event => event.type === "input_request")?.itemId, repeated: adapter.respondInput("question-1", { kind: "questions", answers: {} }) }).toEqual({
      delivered: true, frame: { type: "control_response", response: { subtype: "success", request_id: "question-1", response: { behavior: "allow", updatedInput: { ...input, answers: { "Which mode?": "Thorough", "Which checks?": "Build, Test" } } } } },
      event: "tool-question", repeated: false,
    });
  });

  it("does not offer a session permission that Claude explicitly suppresses", () => {
    adapter.spawn("Run the task", "/project");
    initialize();
    write({ type: "control_request", request_id: "one-shot", request: { subtype: "can_use_tool", tool_name: "Bash", input: {}, suppress_always_allow_rule: true,
      permission_suggestions: [{ type: "addRules", rules: [], behavior: "allow", destination: "localSettings" }] } });
    expect({ choices: events.find(event => event.type === "approval_request")?.approval?.options?.map(option => option.id), sent: adapter.respondApproval("one-shot", "allow") }).toEqual({ choices: ["allow_once", "deny"], sent: false });
  });

  it("interrupts and drains stdin on cancel, then bounds process termination", async () => {
    adapter.spawn("Run the task", "/project");
    initialize();
    let closed = false;
    const stopped = adapter.cancel().then(() => { closed = true; });
    const frames = drain();
    await vi.advanceTimersByTimeAsync(2000);
    await vi.advanceTimersByTimeAsync(5000);
    expect({ interrupt: frames.some(frame => frame.request?.subtype === "interrupt"), closed: child.stdin.writableEnded, signals: child.kill.mock.calls }).toEqual({ interrupt: true, closed: true, signals: [["SIGTERM"], ["SIGKILL"]] });
    expect(closed).toBe(false);
    child.emit("close", 1);
    await stopped;
    expect({ closed, outcomes: events.filter(event => event.type === "done").map(event => event.outcome) }).toEqual({ closed: true, outcomes: ["interrupted"] });
  });

  it("reports initialization failure once and ignores later process exit", () => {
    const exited = vi.fn();
    adapter.onExit(exited);
    adapter.spawn("Run the task", "/project");
    const init = drain()[0];
    write({ type: "control_response", response: { subtype: "error", request_id: init.request_id, error: "Initialization rejected" } });
    child.emit("close", 1);
    expect({ errors: events.filter(event => event.type === "error").length, done: events.filter(event => event.type === "done").length, exits: exited.mock.calls }).toEqual({ errors: 1, done: 1, exits: [[1]] });
  });
});
