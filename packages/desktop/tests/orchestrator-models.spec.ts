import { beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { IpcMain } from "electron";

vi.mock("../electron/orchestrator-adapters", async importOriginal => ({
  ...await importOriginal<typeof import("../electron/orchestrator-adapters")>(), createAdapter: vi.fn(),
}));
vi.mock("../electron/orchestrator-settings", () => ({ loadAgentConfig: () => ({ binaryPath: "/fixture/agent", extraArgs: ["--fixture"], env: { MODEL_FIXTURE: "value" } }) }));
import { AgentAdapter, createAdapter, type AgentModel } from "../electron/orchestrator-adapters";
import { registerModelDiscoveryIpc } from "../electron/orchestrator-models";
import type { Orchestrator, Task } from "../electron/orchestrator";
import type { RpcRouter } from "../electron/ssh-router";

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
  let orchestrator: Pick<Orchestrator, "getTask" | "getRpcRouter" | "agentProcessFactory">;
  const models: AgentModel[] = [{ value: "native-model", label: "Native model", intelligenceLevels: [] }];
  const list = (requestId = "request", sender = owner, cwd = "/fixture", taskId?: string) => handlers.get("orchestrator:list-models")!({ sender }, { requestId, agentType: "claude-code", cwd, taskId });
  const cancel = (requestId = "request", sender = owner) => handlers.get("orchestrator:cancel-model-discovery")!({ sender }, requestId);

  beforeEach(() => {
    vi.clearAllMocks();
    adapter = new ModelAdapter();
    owner = Object.assign(new EventEmitter(), { id: 1 });
    vi.mocked(createAdapter).mockReturnValue(adapter);
    handlers = new Map();
    orchestrator = { getTask: vi.fn(), getRpcRouter: vi.fn(), agentProcessFactory: vi.fn() };
    shutdown = registerModelDiscoveryIpc({ handle: (name: string, handler: unknown) => { handlers.set(name, handler as Handler); } } as unknown as IpcMain, orchestrator);
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

  it("rejects disconnected remote discovery before launching a local provider", async () => {
    expect(await list("remote", owner, "ssh://profile/repo")).toEqual({ success: false, error: "Remote model discovery requires a connected remote agent" });
    expect(createAdapter).not.toHaveBeenCalled();
  });

  it("uses the selected SSH profile without copying client launch settings", async () => {
    const channel = new EventEmitter();
    const router = { remoteChannelOwner: vi.fn(() => channel) } as unknown as RpcRouter;
    vi.mocked(orchestrator.getRpcRouter).mockReturnValue(router);
    const request = list("remote", owner, "ssh://profile/repo 雪");
    adapter.resolve?.(models);
    expect({ result: await request, factory: vi.mocked(createAdapter).mock.calls[0], context: adapter.listModels.mock.calls[0] }).toEqual({
      result: { success: true, models }, factory: ["claude-code", undefined, undefined, undefined, { processFactory: expect.any(Function) }], context: ["/repo 雪", undefined],
    });
    expect(router.remoteChannelOwner).toHaveBeenCalledWith("profile");
  });

  it("resolves an existing task's path and process owner from saved state", async () => {
    const task = { id: "saved", isRemote: true, remoteProfileId: "profile", worktreePath: "/server/task" } as Task;
    const processFactory = vi.fn();
    vi.mocked(orchestrator.getTask).mockReturnValue(task);
    vi.mocked(orchestrator.agentProcessFactory).mockReturnValue(processFactory);
    const request = list("remote-task", owner, "/client/stale", task.id);
    adapter.resolve?.(models);
    expect({ result: await request, factory: vi.mocked(createAdapter).mock.calls[0], context: adapter.listModels.mock.calls[0] }).toEqual({
      result: { success: true, models }, factory: ["claude-code", undefined, undefined, undefined, { processFactory }], context: ["/server/task", undefined],
    });
    expect(orchestrator.agentProcessFactory).toHaveBeenCalledWith(task);
  });

  it("does not probe a local path when the saved task is missing", async () => {
    expect(await list("missing", owner, "/client/stale", "missing")).toEqual({ success: false, error: "Task not found: missing" });
    expect(createAdapter).not.toHaveBeenCalled();
  });

  it("keeps configured local discovery on the saved local task path", async () => {
    vi.mocked(orchestrator.getTask).mockReturnValue({ id: "saved", worktreePath: "/local/task" } as Task);
    const request = list("local-task", owner, "ssh://stale/repo", "saved");
    adapter.resolve?.(models);
    expect({ result: await request, binary: vi.mocked(createAdapter).mock.calls[0][1], context: adapter.listModels.mock.calls[0] }).toEqual({
      result: { success: true, models }, binary: "/fixture/agent", context: ["/local/task", { MODEL_FIXTURE: "value" }],
    });
    expect(orchestrator.getRpcRouter).not.toHaveBeenCalled();
  });

  it("rejects unsupported remote providers before reading client launch settings", async () => {
    expect(await handlers.get("orchestrator:list-models")!({ sender: owner }, { agentType: "custom", requestId: "custom", cwd: "ssh://profile/repo" })).toEqual({ success: false, error: "Remote model discovery requires Codex or Claude Code" });
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
