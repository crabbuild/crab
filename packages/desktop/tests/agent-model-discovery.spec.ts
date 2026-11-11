import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

vi.mock("node:child_process", () => ({ spawn: vi.fn(), execFile: vi.fn(), execFileSync: vi.fn(() => "/usr/bin") }));
import { spawn } from "node:child_process";
import { agentRegistry } from "../electron/orchestrator-registry";
import type { AgentAdapter } from "../electron/orchestrator-adapters";

interface Frame { id?: number; method?: string; params?: { cursor?: string | null }; type?: string; request_id?: string; request?: { subtype?: string } }

describe("installed provider model discovery", () => {
  let child: EventEmitter & { stdin: PassThrough; stdout: PassThrough; stderr: PassThrough; kill: ReturnType<typeof vi.fn> };
  let adapter: AgentAdapter;
  let frames: Frame[];
  let respond: (frame: Frame) => object;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
    frames = [];
    child.stdin.on("data", buffer => {
      for (const line of String(buffer).trim().split("\n")) {
        const frame = JSON.parse(line) as Frame;
        frames.push(frame);
        if (frame.id !== undefined || frame.type === "control_request") {
          queueMicrotask(() => child.stdout.write(JSON.stringify(respond(frame)) + "\n"));
        }
      }
    });
  });
  afterEach(async () => {
    adapter?.cancel();
    child.emit("close", 0);
    await vi.runAllTimersAsync();
    vi.useRealTimers();
  });

  it("reads every Codex model page without creating a thread or turn, preserving native effort names", async () => {
    const model = { model: "wire-model", displayName: "Provider Model", description: "From the installed provider", hidden: false, isDefault: true, defaultReasoningEffort: "provider-effort", supportedReasoningEfforts: [{ reasoningEffort: "none" }, { reasoningEffort: "provider-effort" }] };
    respond = frame => ({ id: frame.id, result: frame.method === "initialize" ? {} : {
      data: frame.params?.cursor ? [{ ...model, model: "second-model", isDefault: false }] : [model, { ...model, hidden: true }],
      nextCursor: frame.params?.cursor ? null : "next-page",
    } });
    adapter = agentRegistry.createAdapter("codex", { binaryPath: "/configured/codex", extraArgs: ["-c", "model_provider=fixture"] });
    const models = await adapter.listModels!("/fixture", { MODEL_FIXTURE_VALUE: "fixture" });
    expect({ models, methods: frames.map(frame => frame.method), cursors: frames.filter(frame => frame.method === "model/list").map(frame => frame.params?.cursor), binary: vi.mocked(spawn).mock.calls[0][0], context: vi.mocked(spawn).mock.calls[0][2]?.env?.MODEL_FIXTURE_VALUE }).toEqual({
      models: [
        { value: "wire-model", label: "Provider Model", description: "From the installed provider", intelligenceLevels: ["none", "provider-effort"], defaultIntelligence: "provider-effort", isDefault: true },
        { value: "second-model", label: "Provider Model", description: "From the installed provider", intelligenceLevels: ["none", "provider-effort"], defaultIntelligence: "provider-effort", isDefault: false },
      ], methods: ["initialize", "initialized", "model/list", "model/list"], cursors: [null, "next-page"], binary: "/configured/codex", context: "fixture",
    });
  });

  it("uses Claude initialization model metadata without sending a user message", async () => {
    respond = frame => ({ type: "control_response", response: { subtype: "success", request_id: frame.request_id, response: { models: [
      { value: "provider-alias", displayName: "Provider Alias", description: "Configured model", supportsEffort: true, supportedEffortLevels: ["high", "max"] },
      { value: "no-effort-model", displayName: "No effort", description: "Provider default effort", supportsEffort: false },
    ] } } });
    adapter = agentRegistry.createAdapter("claude-code", { binaryPath: "/configured/claude" });
    const models = await adapter.listModels!("/fixture");
    expect({ models, requests: frames.map(frame => frame.request), binary: vi.mocked(spawn).mock.calls[0][0] }).toEqual({
      models: [
        { value: "provider-alias", label: "Provider Alias", description: "Configured model", intelligenceLevels: ["high", "max"] },
        { value: "no-effort-model", label: "No effort", description: "Provider default effort", intelligenceLevels: [] },
      ], requests: [{ subtype: "initialize", supportedDialogKinds: [] }], binary: "/configured/claude",
    });
  });

  it.each(["codex", "claude-code"])("surfaces %s initialization rejection without advertising invented models", async agent => {
    respond = frame => agent === "codex"
      ? { id: frame.id, error: { code: -32000, message: "Provider unavailable" } }
      : { type: "control_response", response: { subtype: "error", request_id: frame.request_id, error: "Provider unavailable" } };
    adapter = agentRegistry.createAdapter(agent);
    await expect(adapter.listModels!("/fixture")).rejects.toThrow("Provider unavailable");
  });
});
