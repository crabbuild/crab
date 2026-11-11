import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

vi.mock("node:child_process", () => ({ spawn: vi.fn() }));
vi.mock("../electron/env/userShellEnv", () => ({ getExpandedEnv: vi.fn(async () => ({ PATH: "/usr/bin" })) }));
import { spawn } from "node:child_process";
import { getExpandedEnv } from "../electron/env/userShellEnv";
import { AgentProcessClosureError, runAgentCommand, spawnLocalAgent } from "../electron/agent-process";

describe.skipIf(process.platform === "win32")("local native process closure", () => {
  const missing = () => Object.assign(new Error("No such process"), { code: "ESRCH" });
  let child: EventEmitter & { stdin: PassThrough; stdout: PassThrough; stderr: PassThrough; pid: number; kill: ReturnType<typeof vi.fn> };
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), pid: 42, kill: vi.fn() });
    vi.mocked(spawn).mockReturnValue(child as unknown as ReturnType<typeof spawn>);
  });
  afterEach(() => { vi.restoreAllMocks(); vi.useRealTimers(); });

  it("retains ownership after stdio closes and never signals a released group", async () => {
    let present = true;
    const signal = vi.spyOn(process, "kill").mockImplementation(() => { if (!present) throw missing(); return true; });
    const owner = spawnLocalAgent("provider", [], { cwd: "/project" });
    await vi.waitFor(() => expect(spawn).toHaveBeenCalled());
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
    await vi.waitFor(() => expect(spawn).toHaveBeenCalled());
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
    await vi.waitFor(() => expect(spawn).toHaveBeenCalled());
    const closed = vi.fn(); owner.process.on("close", closed); owner.process.on("error", () => {});
    const failure = expect(owner.closed).rejects.toThrow("closure is unconfirmed");
    child.emit("exit", 0, null); child.emit("close", 0, null);
    await vi.advanceTimersByTimeAsync(10_000);
    await failure;
    expect(closed).not.toHaveBeenCalled();
  });

  it("acknowledges buffered initialization only after the native pipe accepts it", async () => {
    let prepared!: (env: Record<string, string>) => void;
    vi.mocked(getExpandedEnv).mockReturnValueOnce(new Promise(resolve => { prepared = resolve; }));
    let written!: (error?: Error | null) => void;
    vi.spyOn(child.stdin, "write").mockImplementation((_bytes, _encoding, done) => { written = done!; return false; });
    vi.spyOn(process, "kill").mockImplementation(() => { throw missing(); });
    const owner = spawnLocalAgent("provider", [], { cwd: "/project" });
    let acknowledged = false;
    owner.process.stdin!.write("initialization", () => { acknowledged = true; });
    expect(spawn).not.toHaveBeenCalled();
    prepared({ PATH: "/usr/bin" });
    await vi.waitFor(() => expect(written).toBeTypeOf("function"));
    expect(acknowledged).toBe(false);
    written();
    await vi.advanceTimersByTimeAsync(0);
    expect(acknowledged).toBe(true);
    child.emit("exit", 0, null); child.emit("close", 0, null);
    await vi.advanceTimersByTimeAsync(25);
    await owner.closed;
  });

  it("cancels pending discovery without spawning from its late reply", async () => {
    let finish!: (env: Record<string, string>) => void;
    vi.mocked(getExpandedEnv).mockReturnValueOnce(new Promise(resolve => { finish = resolve; }));
    const owner = spawnLocalAgent("provider", [], { cwd: "/project" });
    owner.process.kill("SIGTERM");
    await owner.closed;
    finish({ PATH: "/usr/bin" });
    await vi.advanceTimersByTimeAsync(0);
    expect(spawn).not.toHaveBeenCalled();
  });

});

describe("owned workspace command capture", () => {
  const fixture = () => {
    const child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
    let resolve!: () => void; let reject!: (reason: unknown) => void;
    const closed = new Promise<void>((done, fail) => { resolve = done; reject = fail; });
    const factory = vi.fn(() => ({ process: child, closed }));
    return { child, factory, reject, finish: () => { child.emit("close", 0); resolve(); } };
  };
  afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

  it("preserves split UTF-8 output through the physical closure receipt", async () => {
    const { child, factory, finish } = fixture();
    const captured = runAgentCommand(factory, "git", ["show", "HEAD"], { cwd: "/workspace" });
    const bytes = Buffer.from("雪🙂");
    child.stdout.write(bytes.subarray(0, 4)); child.stdout.write(bytes.subarray(4));
    finish();
    expect(await captured).toEqual({ stdout: "雪🙂" });
  });

  it.each(["timeout", "output"])("retains the writer after %s until closure is confirmed", async reason => {
    vi.useFakeTimers();
    const { child, factory, finish } = fixture();
    const captured = runAgentCommand(factory, "git", [], { cwd: "/workspace", timeoutMs: 30_000 });
    let settled = false; void captured.then(() => { settled = true; }, () => { settled = true; });
    if (reason === "timeout") await vi.advanceTimersByTimeAsync(30_000);
    else child.stderr.write(Buffer.alloc(1024 * 1024 + 1));
    expect({ settled, signals: child.kill.mock.calls }).toEqual({ settled: false, signals: [["SIGTERM"]] });
    await vi.advanceTimersByTimeAsync(5000);
    expect({ settled, signals: child.kill.mock.calls }).toEqual({ settled: false, signals: [["SIGTERM"], ["SIGKILL"]] });
    finish();
    await expect(captured).rejects.toThrow(reason === "timeout" ? "timed out" : "output limit");
  });

  it("distinguishes unknown physical closure from a failed completed command", async () => {
    const { factory, reject } = fixture();
    const captured = runAgentCommand(factory, "git", [], { cwd: "/workspace" });
    const failure = expect(captured).rejects.toBeInstanceOf(AgentProcessClosureError);
    reject(new Error("SSH owner retired"));
    await failure;
  });

  it("refuses an already cancelled command before opening a process", async () => {
    const { factory } = fixture();
    const signal = AbortSignal.abort(new Error("Preparation cancelled"));
    await expect(runAgentCommand(factory, "git", [], { cwd: "/workspace", signal })).rejects.toThrow("Preparation cancelled");
    expect(factory).not.toHaveBeenCalled();
  });

  it("awaits physical closure after cancellation", async () => {
    const { child, factory, finish } = fixture();
    const controller = new AbortController();
    const captured = runAgentCommand(factory, "git", [], { cwd: "/workspace", signal: controller.signal });
    let settled = false; void captured.then(() => { settled = true; }, () => { settled = true; });
    controller.abort(new Error("Preparation cancelled"));
    await Promise.resolve();
    expect({ settled, signals: child.kill.mock.calls }).toEqual({ settled: false, signals: [["SIGTERM"]] });
    finish(); await expect(captured).rejects.toThrow("Preparation cancelled");
  });
});
