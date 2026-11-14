import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import type { AgentAdapter, AgentEvent } from "../electron/orchestrator-adapters";

vi.mock("node:child_process", () => ({ spawn: vi.fn(), execFile: vi.fn(), execFileSync: vi.fn(() => "/usr/bin") }));
import { spawn } from "node:child_process";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";

interface Frame {
  id?: number;
  method?: string;
  params?: Record<string, unknown>;
  type?: string;
  request_id?: string;
  request?: Record<string, unknown>;
  message?: { content: string };
}

describe("native agent turn controls", () => {
  let child: EventEmitter & { stdin: PassThrough; stdout: PassThrough; stderr: PassThrough; kill: ReturnType<typeof vi.fn> };
  let adapter: AgentAdapter;
  let frames: Frame[];
  let events: AgentEvent[];
  let rejectModel: boolean;
  let configuredModel: string | null;
  let holdMethod: string | undefined;

  beforeEach(() => {
    vi.clearAllMocks();
    child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
    frames = [];
    events = [];
    rejectModel = false;
    configuredModel = "configured-default";
    holdMethod = undefined;
    const send = (value: object) => child.stdout.write(JSON.stringify(value) + "\n");
    child.stdin.on("data", buffer => {
      for (const line of String(buffer).trim().split("\n")) {
        const frame = JSON.parse(line) as Frame;
        frames.push(frame);
        queueMicrotask(() => {
          if (frame.method && frame.method === holdMethod) return;
          if (frame.type === "control_request") {
            const rejected = rejectModel && frame.request?.subtype === "set_model";
            send({ type: "control_response", response: { request_id: frame.request_id, subtype: rejected ? "error" : "success", ...(rejected ? { error: "Model unavailable" } : { response: {} }) } });
          } else if (frame.type === "user") {
            send({ type: "result", subtype: "success", session_id: "session", result: "Complete" });
          } else if (frame.id !== undefined) {
            const turn = { id: `turn-${frame.id}`, status: "completed" };
            const result = frame.method === "thread/start" ? { thread: { id: "thread" }, model: "startup-default", reasoningEffort: "medium" }
              : frame.method === "config/read" ? { config: { model: configuredModel, model_reasoning_effort: null } }
              : frame.method === "model/list" ? { data: [{ model: "catalog-default", displayName: "Catalog default", isDefault: true, hidden: false, defaultReasoningEffort: "high", supportedReasoningEfforts: [] }, { model: "configured-default", displayName: "Configured", isDefault: false, hidden: false, defaultReasoningEffort: "low", supportedReasoningEfforts: [] }], nextCursor: null }
              : frame.method === "turn/start" ? { turn } : {};
            send({ id: frame.id, result });
            if (frame.method === "turn/start") {
              send({ method: "turn/started", params: { threadId: "thread", turn } });
              send({ method: "turn/completed", params: { threadId: "thread", turn } });
            }
          }
        });
      }
    });
  });

  afterEach(async () => {
    adapter?.cancel();
    child.emit("close", 0);
    await Promise.resolve();
  });

  it.each(["codex", "claude-code"])("%s uses native plan/settings controls and resets defaults on the next turn", async agent => {
    adapter = agent === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    adapter.onEvent(event => events.push(event));
    adapter.spawn("Original prompt", "/fixture", undefined, { model: "selected-model", mode: "plan", reasoningLevel: "high" });
    await vi.waitFor(() => expect(events.filter(event => event.type === "done")).toHaveLength(1));
    await adapter.iterate!("Unchanged follow-up", { model: null, mode: "instant", reasoningLevel: null });
    await vi.waitFor(() => expect(events.filter(event => event.type === "done")).toHaveLength(2));
    const wire = agent === "codex" ? frames.filter(frame => frame.method === "turn/start").map(frame => frame.params)
      : frames.filter(frame => frame.type === "control_request" || frame.type === "user").map(frame => frame.request ?? frame.message);
    expect(wire).toEqual(agent === "codex" ? [
      { threadId: "thread", input: [{ type: "text", text: "Original prompt" }], collaborationMode: { mode: "plan", settings: { model: "selected-model", reasoning_effort: "high", developer_instructions: null } } },
      { threadId: "thread", input: [{ type: "text", text: "Unchanged follow-up" }], collaborationMode: { mode: "default", settings: { model: "configured-default", reasoning_effort: "low", developer_instructions: null } } },
    ] : [
      { subtype: "initialize", supportedDialogKinds: [], forwardSubagentText: true }, { subtype: "set_permission_mode", mode: "plan" },
      { subtype: "set_model", model: "selected-model" }, { subtype: "apply_flag_settings", settings: { effortLevel: "high" } }, { role: "user", content: "Original prompt" },
      { subtype: "set_permission_mode", mode: "default" }, { subtype: "set_model" }, { subtype: "apply_flag_settings", settings: { effortLevel: null } }, { role: "user", content: "Unchanged follow-up" },
    ]);
  });

  it("uses Codex's default catalog model when effective config leaves the model unset", async () => {
    configuredModel = null;
    adapter = new CodexAppServerAdapter();
    adapter.onEvent(event => events.push(event));
    adapter.spawn("First", "/fixture");
    await vi.waitFor(() => expect(events.some(event => event.type === "done")).toBe(true));
    await adapter.iterate!("Reset", { model: null, reasoningLevel: null });
    expect(frames.filter(frame => frame.method === "turn/start").at(-1)?.params).toMatchObject({ collaborationMode: { settings: { model: "catalog-default", reasoning_effort: "high" } } });
  });

  it("does not send a Claude user turn after a rejected control, retaining earlier acknowledged settings", async () => {
    adapter = new ClaudeCodeAdapter();
    adapter.onEvent(event => events.push(event));
    adapter.spawn("First", "/fixture");
    await vi.waitFor(() => expect(events.some(event => event.type === "done")).toBe(true));
    rejectModel = true;
    await expect(adapter.iterate!("Rejected", { mode: "plan", model: "unavailable" })).rejects.toThrow("Model unavailable");
    expect({ prompts: frames.filter(frame => frame.type === "user").map(frame => frame.message?.content), applied: events.filter(event => event.type === "configuration_changed").map(event => event.configuration) }).toEqual({ prompts: ["First"], applied: [{ mode: "plan" }] });
  });

  it.each(["initialize", "thread/start"])("lets slow Codex %s finish under the owning task deadline", async method => {
    vi.useFakeTimers();
    try {
      holdMethod = method;
      adapter = new CodexAppServerAdapter();
      adapter.onEvent(event => events.push(event));
      adapter.spawn("After startup", "/fixture");
      await vi.advanceTimersByTimeAsync(45_000);
      expect(events.some(event => event.type === "error" || event.type === "done")).toBe(false);
      const request = frames.find(frame => frame.method === method);
      holdMethod = undefined;
      child.stdout.write(JSON.stringify({ id: request?.id, result: method === "initialize" ? {} : { thread: { id: "thread" }, model: "startup-default", reasoningEffort: "medium" } }) + "\n");
      await vi.advanceTimersByTimeAsync(0);
      expect(events.filter(event => event.type === "done").map(event => event.outcome)).toEqual(["completed"]);
    } finally {
      adapter.cancel();
      child.emit("close", 0);
      await vi.advanceTimersByTimeAsync(0);
      vi.useRealTimers();
    }
  });
});
