import type { IpcMain } from "electron";
import { createAdapter, type AgentAdapter } from "./orchestrator-adapters";
import { loadAgentConfig } from "./orchestrator-settings";

/** Own discovery processes until physical closure, including on app quit. */
export function registerModelDiscoveryIpc(ipcMain: IpcMain): () => Promise<void> {
  type Discovery = { adapter: AgentAdapter; closure?: Promise<void> };
  const pending = new Map<string, Discovery>();
  const closing = new Set<Promise<void>>();
  let shutdown: Promise<void> | undefined;
  const retire = (discovery: Discovery) => {
    if (discovery.closure) return discovery.closure;
    const closure = Promise.resolve().then(() => discovery.adapter.cancel());
    discovery.closure = closure;
    closing.add(closure);
    void closure.then(() => closing.delete(closure), () => {});
    return closure;
  };
  const cancel = (key: string) => {
    const discovery = pending.get(key);
    pending.delete(key);
    return discovery ? retire(discovery) : Promise.resolve();
  };
  ipcMain.handle("orchestrator:list-models", async (event, payload: { requestId: string; agentType: string; cwd: string }) => {
    const key = JSON.stringify([event.sender.id, payload.requestId]);
    const stop = () => { void cancel(key).catch(error => console.error("[agent-models] cancellation failed:", error)); };
    let discovery: Discovery | undefined;
    try {
      if (shutdown) throw new Error("Agent service is shutting down");
      if (!payload.requestId) throw new Error("Model discovery request ID is required");
      if (payload.cwd.startsWith("ssh://")) throw new Error("Model discovery requires a local repository");
      if (pending.has(key)) throw new Error("Model discovery is already running");
      const config = loadAgentConfig(payload.agentType);
      const adapter = createAdapter(payload.agentType, config.binaryPath, undefined, config.extraArgs, config);
      discovery = { adapter };
      pending.set(key, discovery);
      event.sender.once("destroyed", stop);
      event.sender.once("render-process-gone", stop);
      event.sender.once("did-navigate", stop);
      const models = adapter.listModels ? await adapter.listModels(payload.cwd, config.env) : [];
      return { success: true, models };
    } catch (error: unknown) {
      return { success: false, error: error instanceof Error ? error.message : "Could not load models" };
    } finally {
      event.sender.removeListener("destroyed", stop);
      event.sender.removeListener("render-process-gone", stop);
      event.sender.removeListener("did-navigate", stop);
      // Release the request ID independently of physical process ownership.
      // An old reply must not retire a new request reusing that identifier.
      if (discovery) {
        if (pending.get(key) === discovery) pending.delete(key);
        await retire(discovery);
      }
    }
  });
  ipcMain.handle("orchestrator:cancel-model-discovery", (event, requestId: string) => {
    return cancel(JSON.stringify([event.sender.id, requestId]));
  });
  return () => {
    shutdown ??= Promise.resolve().then(async () => {
      for (const key of pending.keys()) void cancel(key);
      const results = await Promise.allSettled(closing);
      const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Model discovery shutdown failed");
    });
    return shutdown;
  };
}
