import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentEvent, HydrationStatusResult } from "../src/ipc";
import { hydrationStatusStore as store } from "../src/lib/hydration-status-store";

const native = vi.hoisted(() => ({ read: vi.fn(), listener: undefined as ((event: AgentEvent) => void) | undefined, off: vi.fn() }));
vi.mock("../src/ipc", () => ({
  rpc: { hydrationStatus: native.read },
  onAgentEvent: (listener: (event: AgentEvent) => void) => {
    native.listener = listener;
    return () => { native.listener = undefined; native.off(); };
  },
}));

const cleanup: (() => void)[] = [];
beforeEach(() => { native.read.mockReset(); native.off.mockClear(); });
afterEach(() => { for (const off of cleanup.splice(0)) off(); });

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function snapshot(hydrated: number): HydrationStatusResult {
  return { items: [{ path: "model.bin", kind: "crab", state: hydrated ? "hydrated" : "pointer", logical: 4096, on_disk: hydrated ? 4096 : 128 }],
    summary: { total_tracked: 1, hydrated, pointer: 1 - hydrated, partial: 0, missing: 0, logical_bytes: "4096", on_disk_bytes: hydrated ? "4096" : "128" } };
}

function subscribe(repoId = "repo") {
  const off = store.subscribe(repoId, vi.fn());
  cleanup.push(off);
  return off;
}

describe("shared hydration status", () => {
  it("shares one inventory read for repeated notifications of the same admitted native owner", async () => {
    native.read.mockResolvedValue(snapshot(1));
    subscribe();
    await vi.waitFor(() => expect(native.read).toHaveBeenCalledTimes(1));
    const event = { method: "repo:capabilities-changed", params: { repo_id: "repo", native_session_id: "admitted" } };
    native.listener!(event);
    await vi.waitFor(() => expect(native.read).toHaveBeenCalledTimes(2));
    native.listener!(event);
    await Promise.resolve();
    expect(native.read).toHaveBeenCalledTimes(2);
  });

  it("clears an unavailable native owner's inventory and refreshes only after repository repair", async () => {
    native.read.mockResolvedValueOnce(snapshot(1)).mockResolvedValueOnce(snapshot(0));
    subscribe();
    await vi.waitFor(() => expect(store.snapshot("repo").summary?.hydrated).toBe(1));
    native.listener!({ method: "repo:capabilities-changed", params: { repo_id: "repo", native_session_id: null } });
    expect(store.snapshot("repo").summary).toBeNull();
    native.listener!({ method: "progress/complete", params: { op: "hydrate", job_id: "old-job", status: 1 } });
    expect(native.read).toHaveBeenCalledTimes(1);
    native.listener!({ method: "repo:capabilities-changed", params: { repo_id: "repo", native_session_id: "repaired" } });
    await vi.waitFor(() => expect(store.snapshot("repo").summary?.pointer).toBe(1));
  });

  it("discards a replaced native owner's late inventory before reading the repaired repository", async () => {
    const earlier = deferred<HydrationStatusResult>();
    native.read.mockReturnValueOnce(earlier.promise).mockResolvedValueOnce(snapshot(0));
    subscribe();
    native.listener!({ method: "repo:capabilities-changed", params: { repo_id: "repo", native_session_id: null } });
    native.listener!({ method: "repo:capabilities-changed", params: { repo_id: "repo", native_session_id: "repaired" } });
    expect(native.read).toHaveBeenCalledTimes(1);
    earlier.resolve(snapshot(1));
    await vi.waitFor(() => expect(native.read).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(store.snapshot("repo").summary?.pointer).toBe(1));
  });

  it("does not clear a healthy sibling's hydration inventory when another repository loses its owner", async () => {
    native.read.mockResolvedValue(snapshot(1));
    subscribe("healthy");
    await vi.waitFor(() => expect(store.snapshot("healthy").summary?.hydrated).toBe(1));
    native.listener!({ method: "repo:capabilities-changed", params: { repo_id: "other", native_session_id: null } });
    expect(store.snapshot("healthy").summary?.hydrated).toBe(1);
    expect(native.read).toHaveBeenCalledTimes(1);
  });

  it("shares one native read and refreshes every consumer after partial mutation completion", async () => {
    native.read.mockResolvedValueOnce(snapshot(0)).mockResolvedValueOnce(snapshot(1));
    subscribe(); subscribe();
    await vi.waitFor(() => expect(store.snapshot("repo").summary?.pointer).toBe(1));
    expect(native.read).toHaveBeenCalledTimes(1);
    native.listener!({ method: "progress/complete", params: { op: "hydrate", job_id: "job", status: 1 } });
    await vi.waitFor(() => expect(store.snapshot("repo").byPath.get("model.bin")?.state).toBe("hydrated"));
    expect(native.read).toHaveBeenCalledTimes(2);
  });

  it("drains and rejects an earlier view before fetching a coalesced post-mutation snapshot", async () => {
    const earlier = deferred<HydrationStatusResult>();
    const fresh = deferred<HydrationStatusResult>();
    native.read.mockReturnValueOnce(earlier.promise).mockReturnValueOnce(fresh.promise);
    subscribe();
    for (let index = 0; index < 3; index++) native.listener!({ method: "progress/complete", params: { op: "dehydrate", job_id: "job", status: 0 } });
    earlier.resolve(snapshot(1));
    await vi.waitFor(() => expect(native.read).toHaveBeenCalledTimes(2));
    expect(store.snapshot("repo").summary).toBeNull();
    fresh.resolve(snapshot(0));
    await vi.waitFor(() => expect(store.snapshot("repo").summary?.pointer).toBe(1));
  });

  it("does not let a released owner's late result replace a newly opened repository snapshot", async () => {
    const earlier = deferred<HydrationStatusResult>();
    native.read.mockReturnValueOnce(earlier.promise).mockResolvedValueOnce(snapshot(0));
    const off = subscribe();
    off();
    subscribe();
    await vi.waitFor(() => expect(store.snapshot("repo").summary?.pointer).toBe(1));
    earlier.resolve(snapshot(1));
    await earlier.promise;
    await Promise.resolve();
    off();
    expect(store.snapshot("repo").summary?.pointer).toBe(1);
  });

  it("keeps failures unknown and permits a fresh read through the same route", async () => {
    native.read.mockRejectedValueOnce(new Error("Connection unavailable")).mockResolvedValueOnce(snapshot(1));
    subscribe("ssh://profile/repo");
    await vi.waitFor(() => expect(store.snapshot("ssh://profile/repo").error).toBe("Connection unavailable"));
    expect(store.snapshot("ssh://profile/repo").summary).toBeNull();
    store.refresh("ssh://profile/repo");
    await vi.waitFor(() => expect(store.snapshot("ssh://profile/repo").summary?.hydrated).toBe(1));
    expect(native.read.mock.calls.every(([repoId]) => repoId === "ssh://profile/repo")).toBe(true);
  });

  it("releases event and snapshot ownership when the last consumer leaves", async () => {
    native.read.mockResolvedValue(snapshot(0));
    const first = subscribe();
    const second = subscribe();
    first();
    expect(native.off).not.toHaveBeenCalled();
    second();
    expect(native.off).toHaveBeenCalledTimes(1);
    expect(store.snapshot("repo").summary).toBeNull();
    await Promise.resolve();
    expect(store.snapshot("repo").summary).toBeNull();
  });
});
