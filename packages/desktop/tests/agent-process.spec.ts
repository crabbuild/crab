import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

vi.mock("node:child_process", () => ({ spawn: vi.fn() }));
vi.mock("../electron/orchestrator-adapters", () => ({ getExpandedEnv: () => ({ PATH: "/usr/bin" }) }));
import { spawn } from "node:child_process";
import { spawnLocalAgent } from "../electron/agent-process";

describe.skipIf(process.platform === "win32")("local native process closure", () => {
  const missing = () => Object.assign(new Error("No such process"), { code: "ESRCH" });
  let child: EventEmitter & { stdin: PassThrough; stdout: PassThrough; stderr: PassThrough; pid: number; kill: ReturnType<typeof vi.fn> };
  beforeEach(() => {
    vi.useFakeTimers();
    child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), pid: 42, kill: vi.fn() });
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
  });
  afterEach(() => { vi.restoreAllMocks(); vi.useRealTimers(); });

  it("retains ownership after stdio closes and never signals a released group", async () => {
    let present = true;
    const signal = vi.spyOn(process, "kill").mockImplementation(() => { if (!present) throw missing(); return true; });
    const owner = spawnLocalAgent("provider", [], { cwd: "/project" });
    const closed = vi.fn();
    owner.process.on("close", closed);
    child.emit("exit", 0, null); child.emit("close", 0, null);
    await vi.advanceTimersByTimeAsync(100);
    expect(closed).not.toHaveBeenCalled();
    present = false;
    await vi.advanceTimersByTimeAsync(25);
    await owner.closed;
    signal.mockClear();
    expect({ closed: closed.mock.calls, killed: owner.process.kill("SIGKILL"), signals: signal.mock.calls }).toEqual({ closed: [[0, null]], killed: false, signals: [] });
  });

  it("waits through denied zombie probes until absence is confirmed", async () => {
    let denied = true;
    vi.spyOn(process, "kill").mockImplementation(() => { throw denied ? Object.assign(new Error("Permission denied"), { code: "EPERM" }) : missing(); });
    const owner = spawnLocalAgent("provider", [], { cwd: "/project" });
    const closed = vi.fn(); owner.process.on("close", closed);
    child.emit("exit", 0, null); child.emit("close", 0, null);
    await vi.advanceTimersByTimeAsync(100);
    expect(closed).not.toHaveBeenCalled();
    denied = false;
    await vi.advanceTimersByTimeAsync(25);
    await owner.closed;
    expect(closed).toHaveBeenCalledOnce();
  });

  it("rejects unconfirmed closure without releasing the writer", async () => {
    vi.spyOn(process, "kill").mockImplementation(() => { throw Object.assign(new Error("Permission denied"), { code: "EPERM" }); });
    const owner = spawnLocalAgent("provider", [], { cwd: "/project" });
    const closed = vi.fn(); owner.process.on("close", closed); owner.process.on("error", () => {});
    const failure = expect(owner.closed).rejects.toThrow("closure is unconfirmed");
    child.emit("exit", 0, null); child.emit("close", 0, null);
    await vi.advanceTimersByTimeAsync(10_000);
    await failure;
    expect(closed).not.toHaveBeenCalled();
  });
});
