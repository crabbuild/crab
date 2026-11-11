import { describe, expect, it, vi } from "vitest";
import { FileTransferOwner } from "../electron/file-transfer-owner";

function pending() {
  let resolve!: (value: number) => void;
  let reject!: (reason: Error) => void;
  return { promise: new Promise<number>((yes, no) => { resolve = yes; reject = no; }), resolve: (value: number) => resolve(value), reject: (error: Error) => reject(error) };
}

describe("repository transfer lifetime", () => {
  it("drains all repository owners before shutdown and refuses later admission", async () => {
    const owner = new FileTransferOwner(vi.fn()); const first = pending(), second = pending();
    const a = owner.run("one", () => first.promise), b = owner.run("two", () => second.promise);
    let stopped = false;
    const stop = owner.stop().then(() => { stopped = true; });
    await expect(owner.run("three", vi.fn())).rejects.toThrow("services are closing");
    first.resolve(1); await a; expect(stopped).toBe(false);
    second.resolve(2); await b; await stop; expect(stopped).toBe(true);
  });
  it("keeps all admitted publishers alive after their view closes and rejects later intent", async () => {
    const release = vi.fn().mockResolvedValue({ closed: "worktree" });
    const owner = new FileTransferOwner(release);
    const first = pending(), second = pending();
    const a = owner.run("worktree", () => first.promise);
    const b = owner.run("worktree", () => second.promise);
    const closed = owner.close("worktree");
    const late = vi.fn();
    await expect(owner.run("worktree", late)).rejects.toThrow("services are closing");
    expect(late).not.toHaveBeenCalled();
    first.resolve(1);
    expect(await a).toBe(1);
    expect(release).not.toHaveBeenCalled();
    second.resolve(2);
    expect(await b).toBe(2);
    await closed;
    expect(release).toHaveBeenCalledExactlyOnceWith("worktree");
  });

  it("preserves publication failure while still draining and releasing its repository", async () => {
    const release = vi.fn().mockResolvedValue({});
    const owner = new FileTransferOwner(release);
    const operation = pending();
    const work = owner.run("worktree", () => operation.promise);
    const outcome = expect(work).rejects.toThrow("Integrity failure");
    const closed = owner.close("worktree");
    operation.reject(new Error("Integrity failure"));
    await outcome;
    await closed;
    expect(release).toHaveBeenCalledExactlyOnceWith("worktree");
  });

  it("does not hold sibling repository closure behind another publisher", async () => {
    const release = vi.fn().mockResolvedValue({});
    const owner = new FileTransferOwner(release);
    const operation = pending();
    const work = owner.run("busy", () => operation.promise);
    await owner.close("idle");
    expect(release).toHaveBeenCalledExactlyOnceWith("idle");
    operation.resolve(0);
    await work;
  });

  it("reports native release failure", async () => {
    const owner = new FileTransferOwner(async () => { throw new Error("Close failed"); });
    await expect(owner.close("worktree")).rejects.toThrow("Close failed");
  });
});
