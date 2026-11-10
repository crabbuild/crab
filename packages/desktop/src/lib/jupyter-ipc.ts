import type { KernelOutput, KernelTarget } from "../../electron/jupyter-types";

export interface NotebookOutput {
  output_type: string; name?: string; text?: string | string[];
  data?: Record<string, string | string[]>; ename?: string; evalue?: string; traceback?: string[];
  displayId?: string;
}
interface Snapshot {
  id: string | null; status: string; error: string | null;
  outputs: Map<number, NotebookOutput[]>; counts: Map<number, number>; running: Set<number>;
}

class NotebookKernel {
  private state: Snapshot = { id: null, status: "disconnected", error: null, outputs: new Map(), counts: new Map(), running: new Set() };
  private listeners = new Set<() => void>();
  private requests = new Map<string, number>();
  private clearOnOutput = new Set<string>();
  private generation = 0;
  private text: string | null = null;
  private disposed = false;
  private unsubscribe: Array<() => void>;

  constructor(private repoId: string) {
    this.unsubscribe = [window.crab.onJupyterOutput(event => {
      if (event.id === this.state.id) this.output(event);
    }), window.crab.onJupyterStatus(event => {
      if (event.id !== this.state.id) return;
      // Pending invokes own their terminal error/acknowledgement. A worker's
      // exit event must not erase the startup failure or revive cancellation.
      if (this.state.status === "starting" || this.state.status === "stopping") return;
      if (event.status === "dead" || event.status === "disconnected") {
        this.retireRequests();
        const error = event.status === "dead" ? event.error ?? "The kernel exited. Connect again to continue." : null;
        this.publish({ id: null, status: event.status, running: new Set(), error });
      } else this.publish({ status: event.status });
    })];
  }

  snapshot = () => this.state;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };

  document(text: string): void {
    if (this.text === text) return;
    this.text = text; this.retireRequests();
    this.publish({ outputs: new Map(), counts: new Map(), running: new Set() });
  }

  async connect(target: KernelTarget, pythonPath: string): Promise<void> {
    if (this.state.id || this.state.status === "starting") return;
    this.retireRequests();
    const id = crypto.randomUUID(), generation = this.generation;
    this.publish({ id, status: "starting", error: null, outputs: new Map(), counts: new Map(), running: new Set() });
    try {
      await window.crab.jupyterStart(id, this.repoId, pythonPath, target);
      if (!this.disposed && this.state.id === id && generation === this.generation) this.publish({ status: "idle" });
    } catch (error) { if (this.state.id === id && generation === this.generation) this.publish({ id: null, status: "disconnected", error: String(error) }); }
  }

  async run(cell: number, code: string): Promise<boolean> {
    const id = this.state.id, generation = this.generation;
    if (!id || this.state.running.has(cell)) return false;
    const requestId = crypto.randomUUID();
    // Correlate before IPC admission: fast outputs may arrive before the
    // invoke reply, and tab disposal must not discard this association.
    for (const [previous, index] of this.requests) if (index === cell) { this.requests.delete(previous); this.clearOnOutput.delete(previous); }
    this.requests.set(requestId, cell);
    const outputs = new Map(this.state.outputs); outputs.set(cell, []);
    this.publish({ outputs, running: new Set([...this.state.running, cell]), error: null });
    try {
      const result = await window.crab.jupyterExecute(id, code, requestId, cell);
      return result.status === "ok" && generation === this.generation;
    } catch (error) {
      if (this.state.id === id && this.state.status !== "stopping" && this.state.status !== "starting" && generation === this.generation) this.publish({ error: String(error) });
      return false;
    } finally {
      // Completion releases the running state. The current output area still
      // accepts asynchronous messages until rerun or document/kernel replacement.
      if (generation === this.generation) { const running = new Set(this.state.running); running.delete(cell); this.publish({ running }); }
    }
  }

  async control(action: "interrupt" | "restart" | "disconnect"): Promise<void> {
    const id = this.state.id;
    if (!id) return;
    if (action !== "interrupt") this.retireRequests();
    const generation = this.generation;
    if (action !== "interrupt") this.publish({ status: action === "restart" ? "starting" : "stopping", error: null });
    try {
      if (action === "interrupt") await window.crab.jupyterInterrupt(id);
      else if (action === "restart") {
        await window.crab.jupyterRestart(id);
        if (this.state.id === id && generation === this.generation) this.publish({ outputs: new Map(), counts: new Map(), running: new Set(), status: "idle" });
      } else {
        await window.crab.jupyterShutdown(id);
        if (this.state.id === id && generation === this.generation) this.publish({ id: null, status: "disconnected", running: new Set() });
      }
    } catch (error) {
      if (this.state.id === id && generation === this.generation) this.publish(action === "interrupt" ? { error: String(error) } : { id: null, status: "dead", running: new Set(), error: String(error) });
    }
  }

  dispose(): void { this.disposed = true; for (const off of this.unsubscribe) off(); this.listeners.clear(); this.requests.clear(); }

  private output(event: KernelOutput): void {
    const cell = this.requests.get(event.requestId);
    if (cell === undefined) return;
    const content = event.content;
    if (event.msgType === "execute_input") {
      if (typeof content.execution_count === "number") { const counts = new Map(this.state.counts); counts.set(cell, content.execution_count); this.publish({ counts }); }
      return;
    }
    const outputs = new Map(this.state.outputs);
    if (event.msgType === "clear_output") {
      if (content.wait) this.clearOnOutput.add(event.requestId);
      else { outputs.set(cell, []); this.publish({ outputs }); }
      return;
    }
    const output = toOutput(event);
    if (!output) return;
    if (this.clearOnOutput.delete(event.requestId)) outputs.set(cell, []);
    if (event.msgType === "update_display_data") {
      if (!output.displayId) return;
      for (const [index, values] of outputs) outputs.set(index, values.map(value => value.displayId === output.displayId ? output : value));
    } else {
      const values = [...outputs.get(cell) ?? []], previous = values[values.length - 1];
      if (output.output_type === "stream" && previous?.output_type === "stream" && previous.name === output.name) values[values.length - 1] = { ...previous, text: String(previous.text) + String(output.text) };
      else values.push(output);
      outputs.set(cell, values);
    }
    this.publish({ outputs });
  }

  private publish(patch: Partial<Snapshot>): void {
    if (this.disposed) return;
    this.state = { ...this.state, ...patch };
    for (const listener of this.listeners) listener();
  }

  private retireRequests(): void {
    this.generation++; this.requests.clear(); this.clearOnOutput.clear();
  }
}

function toOutput(event: KernelOutput): NotebookOutput | null {
  const content = event.content;
  if (event.msgType === "stream") return { output_type: "stream", name: content.name as string, text: content.text as string };
  if (event.msgType === "error") return { output_type: "error", ename: content.ename as string, evalue: content.evalue as string, traceback: content.traceback as string[] };
  if (["execute_result", "display_data", "update_display_data"].includes(event.msgType)) {
    const transient = content.transient as { display_id?: string } | undefined;
    return { output_type: "display_data", data: content.data as Record<string, string | string[]>, displayId: transient?.display_id };
  }
  return null;
}

const sessions = new Map<string, { repoId: string; kernel: NotebookKernel }>();
export function notebookKernel(repoId: string, path: string, rev: string | null): NotebookKernel {
  const key = JSON.stringify([repoId, path, rev]);
  let entry = sessions.get(key);
  if (!entry) { entry = { repoId, kernel: new NotebookKernel(repoId) }; sessions.set(key, entry); }
  return entry.kernel;
}
export function retainNotebookRepositories(repoIds: Set<string>): void {
  for (const [key, entry] of sessions) if (!repoIds.has(entry.repoId)) { entry.kernel.dispose(); sessions.delete(key); }
}
