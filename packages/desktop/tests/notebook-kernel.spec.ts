import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { notebookKernel, retainNotebookRepositories } from "../src/lib/jupyter-ipc";
import type { KernelOutput, KernelExecution } from "../electron/jupyter-types";

const outputListeners = new Set<(event: KernelOutput & { id: string }) => void>();
const statusListeners = new Set<(event: { id: string; status: string }) => void>();
let kernelId = "";
const bridge = {
  jupyterStart: vi.fn(async (id: string) => { kernelId = id; }), jupyterExecute: vi.fn<[string, string, string, number], Promise<KernelExecution>>(),
  jupyterInterrupt: vi.fn(async () => {}), jupyterRestart: vi.fn(async () => {}), jupyterShutdown: vi.fn(async () => {}),
  onJupyterOutput: (listener: (event: KernelOutput & { id: string }) => void) => { outputListeners.add(listener); return () => { outputListeners.delete(listener); }; },
  onJupyterStatus: (listener: (event: { id: string; status: string }) => void) => { statusListeners.add(listener); return () => { statusListeners.delete(listener); }; },
};
const emit = (requestId: string, msgType: string, content: Record<string, unknown>) => { for (const listener of outputListeners) listener({ id: kernelId, requestId, msgType, content }); };
beforeEach(() => { vi.clearAllMocks(); bridge.jupyterExecute.mockReset(); vi.stubGlobal("window", { crab: bridge }); });
afterEach(() => { retainNotebookRepositories(new Set()); vi.unstubAllGlobals(); });

it("retains immediate outputs and completion after all view subscriptions detach", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  kernel.document("original"); await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  const unsubscribe = kernel.subscribe(() => {}); unsubscribe();
  bridge.jupyterExecute.mockImplementation(async (_id, _code, requestId) => { emit(requestId, "stream", { name: "stdout", text: "EARLY_OUTPUT" }); return { status: "ok" }; });
  expect(await kernel.run(0, "print('EARLY_OUTPUT')")).toBe(true);
  const reopened = notebookKernel("repo", "analysis.ipynb", null);
  expect(reopened.snapshot().outputs.get(0)?.[0].text).toBe("EARLY_OUTPUT");
  expect(reopened.snapshot().running.size).toBe(0);
});

it("keeps a repaired document's active association but retires it for replacement cells", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  kernel.document("original"); await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  let complete!: (result: KernelExecution) => void;
  let request = "";
  bridge.jupyterExecute.mockImplementation((_id, _code, requestId) => { request = requestId; return new Promise(resolve => { complete = resolve; }); });
  const pending = kernel.run(0, "long_running()");
  kernel.document("original"); emit(request, "stream", { name: "stdout", text: "SAME_DOCUMENT" });
  expect(kernel.snapshot().outputs.get(0)?.[0].text).toBe("SAME_DOCUMENT");
  kernel.document("replacement"); emit(request, "stream", { name: "stdout", text: "OBSOLETE_OUTPUT" });
  complete({ status: "ok" });
  expect(await pending).toBe(false);
  expect(kernel.snapshot().outputs.size).toBe(0);
});

it("applies deferred clearing and display updates across cell outputs", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  bridge.jupyterExecute.mockImplementation(async (_id, _code, requestId) => {
    emit(requestId, "stream", { name: "stdout", text: "OLD" });
    emit(requestId, "clear_output", { wait: true });
    expect(kernel.snapshot().outputs.get(0)?.[0].text).toBe("OLD");
    emit(requestId, "display_data", { data: { "text/plain": "INITIAL" }, transient: { display_id: "plot" } });
    emit(requestId, "update_display_data", { data: { "text/plain": "UPDATED" }, transient: { display_id: "plot" } });
    return { status: "ok" };
  });
  await kernel.run(0, "display()");
  expect(kernel.snapshot().outputs.get(0)).toHaveLength(1);
  expect(kernel.snapshot().outputs.get(0)?.[0].data).toEqual({ "text/plain": "UPDATED" });
});

it("reports terminal execution failure without leaving a cell running", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  bridge.jupyterExecute.mockRejectedValue(new Error("worker disconnected"));
  expect(await kernel.run(0, "work()")).toBe(false);
  expect(kernel.snapshot().error).toContain("worker disconnected");
  expect(kernel.snapshot().running.size).toBe(0);
});

it("keeps post-idle output routing until the cell is replaced", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  let request = "";
  bridge.jupyterExecute.mockImplementation(async (_id, _code, requestId) => { request = requestId; return { status: "ok" }; });
  await kernel.run(0, "background_work()");
  const previous = request;
  emit(previous, "stream", { name: "stdout", text: "AFTER_IDLE" });
  expect(kernel.snapshot().outputs.get(0)?.[0]?.text).toBe("AFTER_IDLE");
  await kernel.run(0, "replacement()");
  emit(previous, "stream", { name: "stdout", text: "OBSOLETE" });
  emit(request, "stream", { name: "stdout", text: "CURRENT" });
  expect(kernel.snapshot().outputs.get(0)?.[0].text).toBe("CURRENT");
});

it("an old execution completion cannot clear a replacement kernel's running cell", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  let finishOld!: (result: KernelExecution) => void, finishNew!: (result: KernelExecution) => void;
  bridge.jupyterExecute.mockImplementationOnce(() => new Promise(resolve => { finishOld = resolve; }));
  const old = kernel.run(0, "old_work()");
  await kernel.control("disconnect");
  await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  bridge.jupyterExecute.mockImplementationOnce(() => new Promise(resolve => { finishNew = resolve; }));
  const current = kernel.run(0, "new_work()");
  finishOld({ status: "ok" }); await old;
  expect(kernel.snapshot().running.has(0)).toBe(true);
  finishNew({ status: "ok" }); await current;
  expect(kernel.snapshot().running.size).toBe(0);
});

it("disconnect retires an unfinished connection before its reply", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  let finish!: () => void;
  bridge.jupyterStart.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const pending = kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  await kernel.control("disconnect");
  finish(); await pending;
  expect(kernel.snapshot().status).toBe("disconnected");
  expect(kernel.snapshot().id).toBeNull();
});

it("a retired restart reply cannot reconnect a disconnected kernel", async () => {
  const kernel = notebookKernel("repo", "analysis.ipynb", null);
  await kernel.connect({ kind: "local", name: "python3", nativeSessionId: "native" }, "/python");
  let finish!: () => void;
  bridge.jupyterRestart.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const pending = kernel.control("restart");
  await kernel.control("disconnect");
  finish(); await pending;
  expect(kernel.snapshot().status).toBe("disconnected");
});
