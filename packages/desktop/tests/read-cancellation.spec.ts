import { afterEach, describe, expect, it, vi } from "vitest";
import { callCancellable, rpc } from "../src/ipc";

afterEach(() => vi.unstubAllGlobals());

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

describe("read cancellation", () => {
  it("awaits the native terminal reply and preserves its failure after a cancel request", async () => {
    const work = deferred<unknown>();
    const bridge = vi.fn((method: string, _params: Record<string, unknown>) => method === "rpc/cancel" ? Promise.resolve({ ok: true }) : work.promise);
    vi.stubGlobal("window", { crab: { rpc: bridge } });
    const operation = callCancellable("preview_safetensors", { repo_id: "repo", path: "weights" });
    let settled = false;
    const terminal = operation.promise.then(() => { settled = true; }, () => { settled = true; });
    operation.cancel();
    await Promise.resolve();
    await Promise.resolve();
    expect(settled).toBe(false);
    const failure = { code: 1012, message: "Content hash differs", data: { kind: "content_read" } };
    work.reject(failure);
    await expect(operation.promise).rejects.toMatchObject(failure);
    await terminal;
  });

  it("routes preview aborts to the same native operation without serializing the signal", async () => {
    const work = deferred<unknown>();
    const bridge = vi.fn((method: string, _params: Record<string, unknown>) => method === "rpc/cancel" ? Promise.resolve({ ok: true }) : work.promise);
    vi.stubGlobal("window", { crab: { rpc: bridge } });
    const controller = new AbortController();
    const result = rpc.previewNpy("ssh://server/repository", "array.npy", { signal: controller.signal });
    controller.abort();
    const request = bridge.mock.calls[0];
    expect(request[0]).toBe("preview_npy");
    const params = request[1];
    expect(params.signal).toBeUndefined();
    expect(bridge).toHaveBeenLastCalledWith("rpc/cancel", { op_id: params.op_id, repo_id: "ssh://server/repository" });
    work.reject({ code: 1010, message: "Operation cancelled" });
    await expect(result).rejects.toMatchObject({ code: 1010 });
  });

  it("does not start an already aborted preview", async () => {
    const bridge = vi.fn();
    vi.stubGlobal("window", { crab: { rpc: bridge } });
    const controller = new AbortController();
    controller.abort();
    await expect(rpc.previewZip("repo", "archive.zip", { signal: controller.signal })).rejects.toMatchObject({ name: "AbortError" });
    expect(bridge).not.toHaveBeenCalled();
  });

  it("does not cancel an operation that has already completed", async () => {
    const bridge = vi.fn().mockResolvedValue({ size: 12 });
    vi.stubGlobal("window", { crab: { rpc: bridge } });
    const operation = callCancellable("read_range", { repo_id: "repo", path: "file" });
    await operation.promise;
    operation.cancel();
    expect(bridge).toHaveBeenCalledTimes(1);
  });

  it("preserves completion when a cancel request loses the race", async () => {
    const work = deferred<unknown>();
    const bridge = vi.fn((method: string) => method === "rpc/cancel" ? Promise.resolve({ ok: false }) : work.promise);
    vi.stubGlobal("window", { crab: { rpc: bridge } });
    const operation = callCancellable("read_range", { repo_id: "repo", path: "file" });
    operation.cancel();
    work.resolve({ bytes_b64: "Ynl0ZXM=" });
    await expect(operation.promise).resolves.toEqual({ bytes_b64: "Ynl0ZXM=" });
  });
});
