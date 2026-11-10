export interface KernelSpec { name: string; displayName: string; language: string; }
export interface JupyterRuntime { pythonPath: string | null; available: boolean; error?: string; }
export interface KernelOutput { requestId: string; msgType: string; content: Record<string, unknown>; }
export interface KernelExecution { status: "ok" | "error" | "aborted"; }
