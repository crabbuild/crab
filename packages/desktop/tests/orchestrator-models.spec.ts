import { beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { IpcMain } from "electron";

vi.mock("../electron/orchestrator-adapters", async importOriginal => ({
  ...await importOriginal<typeof import("../electron/orchestrator-adapters")>(), createAdapter: vi.fn(),
}));
vi.mock("../electron/orchestrator-settings", () => ({ loadAgentConfig: () => ({ binaryPath: "/fixture/agent", extraArgs: ["--fixture"], env: { MODEL_FIXTURE: "value" } }) }));
import { AgentAdapter, createAdapter, type AgentModel } from "../electron/orchestrator-adapters";
import { registerModelDiscoveryIpc } from "../electron/orchestrator-models";

class ModelAdapter extends AgentAdapter {
  readonly name = "Fixture";
  readonly binary = "fixture";
  resolve?: (models: AgentModel[]) => void;
  reject?: (error: Error) => void;
  spawn(): void {}
  listModels = vi.fn((_cwd: string, _env?: Record<string, string>) => new Promise<AgentModel[]>((resolve, reject) => { this.resolve = resolve; this.reject = reject; }));
  cancel = vi.fn((): void | Promise<void> => this.reject?.(new Error("Provider connection closed")));
}

type Owner = EventEmitter & { id: number };
type Handler = (event: { sender: Owner }, payload: unknown) => unknown;

describe("renderer-owned model discovery IPC", () => {
  let handlers: Map<string, Handler>;
  let adapter: ModelAdapter;
  let owner: Owner;
  let shutdown: () => Promise<void>;
  const models: AgentModel[] = [{ value: "native-model", label: "Native model", intelligenceLevels: [] }];
  const list = (requestId = "request", sender = owner, cwd = "/fixture") => handlers.get("orchestrator:list-models")!({ sender }, { requestId, agentType: "claude-code", cwd });
  const cancel = (requestId = "request", sender = owner) => handlers.get("orchestrator:cancel-model-discovery")!({ sender }, requestId);

  beforeEach(() => {
    vi.clearAllMocks();
    adapter = new ModelAdapter();
    owner = Object.assign(new EventEmitter(), { id: 1 });
    vi.mocked(createAdapter).mockReturnValue(adapter);
    handlers = new Map();
    shutdown = registerModelDiscoveryIpc({ handle: (name: string, handler: unknown) => { handlers.set(name, handler as Handler); } } as unknown as IpcMain);
  });

  it("uses configured launch settings and retires the process/listener after returning models", async () => {
    const request = list();
    adapter.resolve?.(models);
    expect({ result: await request, factory: vi.mocked(createAdapter).mock.calls[0], context: adapter.listModels.mock.calls[0], stopped: adapter.cancel.mock.calls.length, listeners: owner.listenerCount("destroyed") }).toEqual({
      result: { success: true, models }, factory: ["claude-code", "/fixture/agent", undefined, ["--fixture"], { binaryPath: "/fixture/agent", extraArgs: ["--fixture"], env: { MODEL_FIXTURE: "value" } }],
      context: ["/fixture", { MODEL_FIXTURE: "value" }], stopped: 1, listeners: 0,
    });
  });

  it.each(["destroyed", "render-process-gone", "did-navigate"])("allows only the owning renderer to cancel, including on %s", async lifecycle => {
    const request = list();
    cancel("request", Object.assign(new EventEmitter(), { id: 2 }));
    expect(adapter.cancel).not.toHaveBeenCalled();
    owner.emit(lifecycle);
    expect({ result: await request, stopped: adapter.cancel.mock.calls.length, listeners: owner.eventNames() }).toEqual({ result: { success: false, error: "Provider connection closed" }, stopped: 1, listeners: [] });
  });

  it("does not let cancellation cleanup retire a new request with the same identifier", async () => {
    const first = list();
    cancel();
    const next = new ModelAdapter();
    vi.mocked(createAdapter).mockReturnValue(next);
    const second = list();
    await first;
    expect(next.cancel).not.toHaveBeenCalled();
    next.resolve?.(models);
    expect(await second).toEqual({ success: true, models });
  });

  it("reports provider failure and releases ownership for a retry", async () => {
    const first = list();
    adapter.reject?.(new Error("Catalog unavailable"));
    expect(await first).toEqual({ success: false, error: "Catalog unavailable" });
    const next = new ModelAdapter();
    vi.mocked(createAdapter).mockReturnValue(next);
    const retry = list();
    next.resolve?.(models);
    expect(await retry).toEqual({ success: true, models });
  });

  it("rejects a duplicate request without interrupting its existing provider", async () => {
    const original = list();
    expect(await list()).toEqual({ success: false, error: "Model discovery is already running" });
    expect(adapter.cancel).not.toHaveBeenCalled();
    adapter.resolve?.(models);
    expect(await original).toEqual({ success: true, models });
  });

  it("rejects remote discovery before launching a local provider", async () => {
    expect(await list("remote", owner, "ssh://profile/repo")).toEqual({ success: false, error: "Model discovery requires a local repository" });
    expect(createAdapter).not.toHaveBeenCalled();
  });

  it("joins already cancelled owners and closes discovery before app exit", async () => {
    let release!: () => void;
    adapter.cancel.mockImplementation(() => {
      adapter.reject?.(new Error("Provider connection closed"));
      return new Promise<void>(resolve => { release = resolve; });
    });
    const request = list();
    cancel();
    let closed = false;
    const closing = shutdown().then(() => { closed = true; });
    try {
      await vi.waitFor(() => expect(adapter.cancel).toHaveBeenCalledOnce());
      expect(closed).toBe(false);
      expect(await list("new")).toEqual({ success: false, error: "Agent service is shutting down" });
      release();
      await Promise.all([closing, shutdown(), request]);
      expect({ closed, calls: adapter.cancel.mock.calls.length, listeners: owner.eventNames() }).toEqual({ closed: true, calls: 1, listeners: [] });
    } finally { release?.(); await closing; await request; }
  });
});
