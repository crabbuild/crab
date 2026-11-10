import { EventEmitter } from "node:events";
import * as http from "node:http";
import type { IpcMain } from "electron";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Orchestrator } from "../electron/orchestrator";
import { registerAgentWorkflowIpc } from "../electron/agent-workflows/agent-workflow-ipc";
import { AgentWorkflowTriggerStore } from "../electron/agent-workflows/persistence";
import { AgentWorkflowRunner } from "../electron/agent-workflows/runner";
import { AgentWorkflowTriggerScheduler } from "../electron/agent-workflows/trigger-scheduler";
import { AgentWorkflowWebhookServer } from "../electron/agent-workflows/webhook-server";
import type { AgentWorkflowDefinition, AgentWorkflowRun, AgentWorkflowTrigger } from "../electron/agent-workflows/types";

const trigger: AgentWorkflowTrigger = { id: "fixture", repoPath: "/fixture", workflowDefinitionId: "definition", name: "Fixture", kind: "webhook",
  rrule: "", enabled: true, webhookToken: "owned-token", createdAt: 0, updatedAt: 0 };
const definition: AgentWorkflowDefinition = { schemaVersion: 2, id: "definition", name: "Fixture", definitionVersion: 1, revisionId: "revision",
  maxParallelNodes: 1, defaultAgentType: "codex", nodes: [], edges: [], layout: {}, createdAt: 0, updatedAt: 0 };
const run: AgentWorkflowRun = { id: "owned-run", repoPath: "/fixture", definitionId: definition.id, definitionSnapshot: definition,
  status: "running", startedAt: 0, nodeRuns: [], edgeRuns: [] };

type Handler = Parameters<IpcMain["handle"]>[1];
const event = {} as Parameters<Handler>[0];
afterEach(() => { vi.restoreAllMocks(); });

describe("workflow webhook listener", () => {
  it("rejects a refused bind, preserves its cause and retries the shipped endpoint", async () => {
    const blocker = http.createServer();
    await new Promise<void>(resolve => blocker.listen(0, "127.0.0.1", resolve));
    const address = blocker.address();
    if (!address || typeof address === "string") throw new Error("No fixture port");
    const listen = http.Server.prototype.listen;
    const calls = vi.spyOn(http.Server.prototype, "listen").mockImplementation(function(this: http.Server, ...args: unknown[]) {
      return Reflect.apply(listen, this, [address.port, ...args.slice(1)]);
    });
    const server = new AgentWorkflowWebhookServer({ list: async () => [] }, { get: async () => null }, { startRun: async () => run });
    try {
      await expect(server.start()).rejects.toMatchObject({ cause: { code: "EADDRINUSE" } });
      await new Promise<void>((resolve, reject) => blocker.close(error => error ? reject(error) : resolve()));
      await server.start();
      expect(calls.mock.calls.map(args => args.slice(0, 2))).toEqual([[17789, "127.0.0.1"], [17789, "127.0.0.1"]]);
    } finally {
      await server.stop();
      if (blocker.listening) await new Promise<void>(resolve => blocker.close(() => resolve()));
    }
  });

  it("publishes a run only after a real HTTP request reaches the ready listener", async () => {
    let listener: http.Server | undefined;
    const listen = http.Server.prototype.listen;
    vi.spyOn(http.Server.prototype, "listen").mockImplementation(function(this: http.Server, ...args: unknown[]) {
      listener = this;
      return Reflect.apply(listen, this, [0, ...args.slice(1)]);
    });
    const startRun = vi.fn<Parameters<AgentWorkflowRunner["startRun"]>, ReturnType<AgentWorkflowRunner["startRun"]>>().mockResolvedValue(run);
    const server = new AgentWorkflowWebhookServer({ list: async () => [trigger] }, { get: async () => definition }, { startRun });
    try {
      const ready = server.start();
      expect(server.start()).toBe(ready);
      await ready;
      const address = listener?.address();
      if (!address || typeof address === "string") throw new Error("No fixture listener");
      const response = await fetch(`http://127.0.0.1:${address.port}/${trigger.webhookToken}`, { method: "POST", body: JSON.stringify({ message: "visible input" }) });
      expect({ response: await response.json(), calls: startRun.mock.calls, publicUrl: server.getUrl(trigger.webhookToken!) }).toEqual({
        response: { runId: run.id, status: run.status }, calls: [[trigger.repoPath, definition, { input: { message: "visible input" }, trigger: { type: "webhook", triggerId: trigger.id } }]],
        publicUrl: "http://127.0.0.1:17789/owned-token",
      });
    } finally { await server.stop(); }
  });
});

describe("workflow webhook admission", () => {
  function register() {
    const handlers = new Map<string, Handler>();
    vi.spyOn(AgentWorkflowTriggerStore.prototype, "list").mockResolvedValue([]);
    vi.spyOn(AgentWorkflowRunner.prototype, "recoverActiveRuns").mockResolvedValue();
    vi.spyOn(AgentWorkflowTriggerScheduler.prototype, "start").mockImplementation(() => {});
    registerAgentWorkflowIpc({ handle: (name, handler) => { handlers.set(name, handler); } }, () => null, () => new EventEmitter() as Orchestrator);
    return handlers;
  }

  it("does not bind a webhook listener when there are no enabled webhooks", async () => {
    const start = vi.spyOn(AgentWorkflowWebhookServer.prototype, "start").mockResolvedValue();
    register();
    await new Promise(resolve => setImmediate(resolve));
    expect(start).not.toHaveBeenCalled();
  });

  it.each(["save", "enable"] as const)("does not persist an enabled webhook when %s cannot acquire its listener", async mode => {
    const handlers = register();
    vi.spyOn(AgentWorkflowWebhookServer.prototype, "start").mockRejectedValue(new Error("Owned endpoint is busy"));
    vi.spyOn(AgentWorkflowTriggerStore.prototype, "get").mockResolvedValue({ ...trigger, enabled: false });
    const save = vi.spyOn(AgentWorkflowTriggerStore.prototype, "save");
    const enable = vi.spyOn(AgentWorkflowTriggerStore.prototype, "setEnabled");
    const result = mode === "save"
      ? await handlers.get("agent-workflow:save-trigger")!(event, { trigger })
      : await handlers.get("agent-workflow:set-trigger-enabled")!(event, { triggerId: trigger.id, enabled: true });
    expect({ result, writes: save.mock.calls.length + enable.mock.calls.length }).toEqual({ result: { success: false, error: "Owned endpoint is busy" }, writes: 0 });
  });

  it("reports an unavailable saved endpoint through trigger loading", async () => {
    const handlers = register();
    await new Promise(resolve => setImmediate(resolve));
    vi.mocked(AgentWorkflowTriggerStore.prototype.list).mockResolvedValue([trigger]);
    vi.spyOn(AgentWorkflowWebhookServer.prototype, "start").mockRejectedValue(new Error("Owned endpoint is busy"));
    expect(await handlers.get("agent-workflow:list-triggers")!(event, { repoPath: trigger.repoPath })).toEqual({ success: false, error: "Owned endpoint is busy", triggers: [] });
  });
});
