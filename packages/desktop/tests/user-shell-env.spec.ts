import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import type { ChildProcess } from "node:child_process";
import * as path from "node:path";

const fixture = vi.hoisted(() => ({ read: vi.fn(), spawn: vi.fn() }));
vi.mock("shell-env", () => ({ shellEnv: fixture.read }));
vi.mock("node:child_process", async original => ({ ...await original<typeof import("node:child_process")>(), spawn: fixture.spawn }));
import { getExpandedEnv, getUserShellBaseEnv, invalidateUserShellEnvCache } from "../electron/env/userShellEnv";
import { GenericAdapter } from "../electron/orchestrator-adapters";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}
beforeEach(() => {
  invalidateUserShellEnvCache(); fixture.read.mockReset(); fixture.spawn.mockReset();
});
afterEach(() => { invalidateUserShellEnvCache(); });

describe("shared user environment admission", () => {
  it("coalesces concurrent login environment requests", async () => {
    const waiting = deferred<NodeJS.ProcessEnv>(); fixture.read.mockReturnValue(waiting.promise);
    const first = getUserShellBaseEnv(), second = getUserShellBaseEnv();
    await vi.waitFor(() => expect(fixture.read.mock.calls.length).toBe(1));
    expect(first).toBe(second);
    waiting.resolve({ PATH: "/fixture/login", CRAB_SHELL_FIXTURE: "shared" });
    expect((await second).CRAB_SHELL_FIXTURE).toBe("shared");
  });

  it("does not let an invalidated pending lookup replace the next cache owner", async () => {
    const old = deferred<NodeJS.ProcessEnv>(), next = deferred<NodeJS.ProcessEnv>();
    fixture.read.mockReturnValueOnce(old.promise).mockReturnValueOnce(next.promise);
    const previous = getUserShellBaseEnv();
    await vi.waitFor(() => expect(fixture.read.mock.calls.length).toBe(1));
    invalidateUserShellEnvCache(); const current = getUserShellBaseEnv();
    await vi.waitFor(() => expect(fixture.read.mock.calls.length).toBe(2));
    old.resolve({ PATH: "/fixture/old" }); await previous;
    expect(getUserShellBaseEnv()).toBe(current);
    next.resolve({ PATH: "/fixture/new" });
    expect((await current).PATH?.split(path.delimiter)).toContain("/fixture/new");
  });

  it("preserves explicit agent variables and both process and login paths", async () => {
    fixture.read.mockResolvedValue({ PATH: "/fixture/login", CRAB_SHELL_FIXTURE: "shared" });
    const env = await getExpandedEnv({ PATH: "/fixture/explicit", CRAB_SHELL_FIXTURE: "explicit" });
    expect(env.CRAB_SHELL_FIXTURE).toBe("explicit");
    expect(env.PATH.split(path.delimiter).slice(0, 2)).toEqual(["/fixture/explicit", "/fixture/login"]);
  });

  it("settles a cancelled launch while shared shell discovery is still blocked", async () => {
    const waiting = deferred<NodeJS.ProcessEnv>(); fixture.read.mockReturnValue(waiting.promise);
    const adapter = new GenericAdapter("fixture-agent");
    const pending = adapter.spawn("fixture", "/fixture");
    await vi.waitFor(() => expect(fixture.read.mock.calls.length).toBe(1));
    adapter.cancel(); await pending;
    waiting.resolve({ PATH: "/fixture/login" });
    await new Promise<void>(resolve => setImmediate(resolve));
    expect(fixture.spawn.mock.calls.length).toBe(0);
  });

  it("starts an admitted process only after its environment arrives", async () => {
    const waiting = deferred<NodeJS.ProcessEnv>(); fixture.read.mockReturnValue(waiting.promise);
    const child = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn() });
    fixture.spawn.mockReturnValue(child as unknown as ChildProcess);
    const adapter = new GenericAdapter("fixture-agent");
    const pending = adapter.spawn("fixture", "/fixture");
    await vi.waitFor(() => expect(fixture.read.mock.calls.length).toBe(1));
    expect(fixture.spawn.mock.calls.length).toBe(0);
    waiting.resolve({ CRAB_SHELL_FIXTURE: "shared" }); await pending;
    expect(fixture.spawn.mock.calls.length).toBe(1);
    const options = fixture.spawn.mock.calls[0][2] as { env: Record<string, string> };
    expect(options.env.CRAB_SHELL_FIXTURE).toBe("shared");
    adapter.cancel();
  });
});
