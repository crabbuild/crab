export interface RpcFailure {
  message: string;
  code?: number | string;
  data?: unknown;
}

export type RpcReply<T> = { ok: true; value: T } | { ok: false; error: RpcFailure };

// Electron invoke serializes thrown Errors as messages only. Carry the native
// classification as plain data across IPC and reconstruct Error in the renderer.
export async function rpcReply<T>(work: () => Promise<T>): Promise<RpcReply<T>> {
  try {
    return { ok: true, value: await work() };
  } catch (error) {
    const detail = error && typeof error === "object" ? error as Partial<RpcFailure> : null;
    const failure: RpcFailure = { message: typeof detail?.message === "string" ? detail.message : String(error) };
    if (typeof detail?.code === "number" || typeof detail?.code === "string") failure.code = detail.code;
    if (detail && "data" in detail) failure.data = detail.data;
    return { ok: false, error: failure };
  }
}
