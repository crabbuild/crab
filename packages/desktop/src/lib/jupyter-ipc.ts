/**
 * Typed wrapper around the Jupyter kernel IPC exposed by the preload.
 */

export interface KernelSpec {
  name: string;
  displayName: string;
  language: string;
}

export interface JupyterOutput {
  id: string;
  msgType: string;
  content: Record<string, unknown>;
  parentMsgId: string;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const w = window as any;

export const jupyter = {
  listKernels: (): Promise<KernelSpec[]> =>
    w.crab.jupyterListKernels() as Promise<KernelSpec[]>,

  start: (kernelName: string, cwd: string): Promise<{ id: string | null; status: string; message?: string }> =>
    w.crab.jupyterStart(kernelName, cwd),

  execute: (id: string, code: string): Promise<{ msgId?: string; error?: string }> =>
    w.crab.jupyterExecute(id, code),

  interrupt: (id: string): Promise<void> =>
    w.crab.jupyterInterrupt(id),

  restart: (id: string): Promise<void> =>
    w.crab.jupyterRestart(id),

  shutdown: (id: string): Promise<void> =>
    w.crab.jupyterShutdown(id),

  onOutput: (listener: (evt: JupyterOutput) => void): (() => void) =>
    w.crab.onJupyterOutput(listener),

  onStatus: (listener: (evt: { id: string; status: string }) => void): (() => void) =>
    w.crab.onJupyterStatus(listener),
};
