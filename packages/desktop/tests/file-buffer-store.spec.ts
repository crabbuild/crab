import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FileBufferStore } from "../src/lib/file-buffer-store";

vi.mock("../src/ipc", () => ({ mainProcess: {} }));
const tick = () => new Promise<void>(resolve => setImmediate(resolve));
function deferred() {
  let resolve!: () => void, reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const target = { repoId: "ssh://profile/repo", fullPath: "/repo/file.txt", available: true, nativeSessionId: "native:1" };
function opened(write = vi.fn().mockResolvedValue(undefined)) {
  const store = new FileBufferStore(write);
  const unsubscribe = store.subscribe("file", () => {});
  store.bind("file", target);
  store.finishRead("file", store.beginRead("file")!, "disk");
  return { store, write, unsubscribe };
}
beforeEach(() => { vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] }); });
afterEach(() => { vi.useRealTimers(); });

describe("repository file buffers", () => {
  it("retains failed edits and errors after all views close, then acknowledges retry", async () => {
    const { store, write, unsubscribe } = opened(vi.fn().mockRejectedValueOnce(new Error("denied")).mockResolvedValue(undefined));
    store.edit("file", "pending"); await store.save("file"); unsubscribe();
    const close = store.subscribe("file", () => {});
    expect(store.snapshot("file")).toMatchObject({ content: "pending", dirty: true, error: "denied" });
    store.bind("file", target); await tick();
    expect(write).toHaveBeenCalledTimes(1);
    await store.save("file"); close();
    expect(store.pendingCount()).toBe(0);
    expect(store.snapshot("file").content).toBeNull();
  });

  it("shares one writer across views and drains a newer edit after an older acknowledgement", async () => {
    const first = deferred(), next = deferred();
    const { store, write, unsubscribe } = opened(vi.fn().mockReturnValueOnce(first.promise).mockReturnValueOnce(next.promise));
    const closeSecond = store.subscribe("file", () => {});
    store.edit("file", "first"); const saved = store.save("file");
    expect(store.save("file")).toBe(saved);
    await tick(); unsubscribe(); closeSecond();
    store.subscribe("file", () => {}); store.bind("file", target);
    expect(store.save("file")).toBe(saved);
    store.edit("file", "latest"); first.resolve(); await tick();
    expect(store.snapshot("file")).toMatchObject({ content: "latest", dirty: true, saving: true });
    expect(write.mock.calls.map(call => call[1])).toEqual(["first", "latest"]);
    next.resolve(); await saved;
    expect(store.snapshot("file")).toMatchObject({ content: "latest", dirty: false, saving: false });
  });

  it("a clean save or blur never writes cached content over external changes", async () => {
    const { store, write } = opened();
    await store.save("file");
    expect(write).not.toHaveBeenCalled();
  });

  it("keeps an unmounted remote edit and retries once when its native owner returns", async () => {
    const { store, write, unsubscribe } = opened();
    store.runtime(target.repoId, false, null); store.edit("file", "offline"); unsubscribe();
    await vi.advanceTimersByTimeAsync(1000);
    expect(write).not.toHaveBeenCalled();
    store.runtime(target.repoId, true, "native:2"); store.runtime(target.repoId, true, "native:2"); await tick();
    expect(write.mock.calls).toEqual([[{ repoId: "ssh://profile/repo", fullPath: "/repo/file.txt" }, "offline", "native:2"]]);
    expect(store.pendingCount()).toBe(0);
  });

  it("a replacement native owner joins the old writer and retries its late failure", async () => {
    const first = deferred();
    const { store, write } = opened(vi.fn().mockReturnValueOnce(first.promise).mockResolvedValue(undefined));
    store.edit("file", "pending"); const saved = store.save("file"); await tick();
    store.runtime(target.repoId, false, null); store.runtime(target.repoId, true, "native:2");
    first.reject(new Error("old session lost")); await saved;
    expect(write.mock.calls.map(call => call[2])).toEqual(["native:1", "native:2"]);
    expect(store.snapshot("file")).toMatchObject({ content: "pending", dirty: false, error: null });
  });

  it("rebinding the same physical buffer retries under its new logical repository", async () => {
    const first = deferred();
    const { store, write } = opened(vi.fn().mockReturnValueOnce(first.promise).mockResolvedValue(undefined));
    store.edit("file", "pending"); const saved = store.save("file"); await tick();
    store.bind("file", { ...target, repoId: "replacement", nativeSessionId: "native:2" });
    first.reject(new Error("repository closed")); await saved;
    expect(write.mock.calls.at(-1)).toEqual([{ repoId: "replacement", fullPath: "/repo/file.txt" }, "pending", "native:2"]);
    expect(store.snapshot("file").dirty).toBe(false);
  });

  it("local native repair cannot hide a denied in-flight save or drain a newer edit", async () => {
    const first = deferred();
    const { store, write } = opened(vi.fn().mockReturnValueOnce(first.promise).mockResolvedValue(undefined));
    store.bind("file", { ...target, repoId: "local" });
    store.edit("file", "first"); const saved = store.save("file"); await tick();
    store.edit("file", "latest"); store.runtime("local", true, "native:2");
    first.reject(new Error("denied")); await saved; await vi.advanceTimersByTimeAsync(1000);
    store.runtime("local", true, "native:3"); await tick();
    expect(write).toHaveBeenCalledTimes(1);
    expect(store.snapshot("file")).toMatchObject({ content: "latest", dirty: true, error: "denied" });
    await store.save("file");
    expect(store.snapshot("file")).toMatchObject({ content: "latest", dirty: false, error: null });
  });

  it("runtime loss is authoritative over a stale view binding", async () => {
    const { store, write } = opened();
    store.runtime(target.repoId, false, null); store.bind("file", target); store.edit("file", "offline");
    await store.save("file");
    expect(write).not.toHaveBeenCalled();
    expect(store.snapshot("file").available).toBe(false);
  });

  it("a late read cannot replace pending content or a newer read", () => {
    const { store } = opened();
    const old = store.beginRead("file")!, latest = store.beginRead("file")!;
    expect(store.finishRead("file", old, "obsolete")).toBe(false);
    store.edit("file", "pending");
    expect(store.finishRead("file", latest, "obsolete")).toBe(false);
    expect(store.beginRead("file")).toBeNull();
    expect(store.snapshot("file").content).toBe("pending");
  });

  it("a read from an earlier closed buffer cannot update its reopened lifetime", () => {
    const { store, unsubscribe } = opened();
    const old = store.beginRead("file")!; unsubscribe();
    store.subscribe("file", () => {}); store.bind("file", target);
    const current = store.beginRead("file")!;
    expect(store.finishRead("file", old, "obsolete")).toBe(false);
    expect(store.finishRead("file", current, "current")).toBe(true);
  });

  it("explicit Reload drains after an admitted write so the old save cannot win", async () => {
    const first = deferred();
    const { store, write } = opened(vi.fn().mockReturnValueOnce(first.promise).mockResolvedValue(undefined));
    store.edit("file", "old edit"); const saved = store.save("file"); await tick();
    store.reload("file", "selected disk version"); first.resolve(); await saved;
    expect(write.mock.calls.map(call => call[1])).toEqual(["old edit", "selected disk version"]);
    expect(store.snapshot("file")).toMatchObject({ content: "selected disk version", dirty: false });
  });

  it("empty and multibyte buffers remain valid edits", async () => {
    const { store, write } = opened();
    store.edit("file", ""); await store.save("file");
    store.edit("file", "🦀 café"); await store.save("file");
    expect(write.mock.calls.map(call => call[1])).toEqual(["", "🦀 café"]);
    expect(store.pendingCount()).toBe(0);
  });
});


describe("restoration buffer reservation", () => {
  it("refuses pending and admitted saves before reserving any selected buffer", async () => {
    const first = deferred();
    const { store } = opened(vi.fn().mockReturnValue(first.promise));
    store.edit("file", "pending");
    expect(() => store.protect(["clean", "file"])).toThrow("pending editor");
    expect(store.snapshot("clean").restoring).toBe(false);
    const saving = store.save("file");
    expect(() => store.protect(["file"])).toThrow("pending editor");
    first.resolve(); await saving;
  });
  it("revokes stale reads, survives unmount and reloads clean content after release", () => {
    const { store, unsubscribe } = opened();
    const read = store.beginRead("file")!;
    const release = store.protect(["file", "file"]);
    unsubscribe();
    const close = store.subscribe("file", () => {});
    expect(store.snapshot("file").restoring).toBe(true);
    expect(store.finishRead("file", read, "old")).toBe(false);
    expect(store.beginRead("file")).toBeNull();
    release(); release();
    expect(store.snapshot("file")).toMatchObject({ restoring: false, content: null, dirty: false });
    store.finishRead("file", store.beginRead("file")!, "restored");
    expect(store.snapshot("file").content).toBe("restored");
    close();
  });
  it("retains a late draft without autosaving over restoration, including recovery", async () => {
    const { store, write } = opened();
    const release = store.protect(["file"]);
    store.edit("file", "late draft");
    await store.save("file");
    store.reload("file", "stale reload");
    release();
    store.runtime(target.repoId, false, null); store.runtime(target.repoId, true, "native:2");
    store.bind("file", { ...target, repoId: "new-alias", nativeSessionId: "native:2" });
    await vi.advanceTimersByTimeAsync(1000);
    expect(write).not.toHaveBeenCalled();
    expect(store.snapshot("file")).toMatchObject({ content: "late draft", dirty: true, restoring: false });
    await store.save("file");
    expect(write.mock.calls.map(call => call[1])).toEqual(["late draft"]);
  });
  it("reserves unopened files before synchronous view callbacks can start writers", async () => {
    const { store, write } = opened();
    const close = store.subscribe("file", () => { if (store.snapshot("file").restoring) store.edit("second", "late"); });
    const release = store.protect(["file", "second"]);
    store.bind("second", target);
    await vi.advanceTimersByTimeAsync(1000);
    expect(store.snapshot("second")).toMatchObject({ restoring: true, dirty: true, content: "late" });
    expect(write).not.toHaveBeenCalled();
    release(); close();
  });
});
