/**
 * Wiring tests for CodexAppServerAdapter spawn arguments.
 *
 * Verifies that:
 *   1. The adapter spawns `codex app-server --listen stdio://` with the
 *      correct args, cwd, stdio, and expanded env.
 *   2. Extra args from per-agent config are appended after the base args.
 *   3. The env includes PATH expansion via getExpandedEnv and custom vars.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock child_process before any module reads it so every spawn call is
// captured and no real codex process is forked during the test.
const spawnCalls: Array<{ binary: string; args: string[]; options: unknown }> = [];

vi.mock("node:child_process", async () => {
  const actual = await vi.importActual<typeof import("node:child_process")>("node:child_process");
  return {
    ...actual,
    spawn: vi.fn((binary: string, args: string[], options: unknown) => {
      spawnCalls.push({ binary, args, options });
      return makeFakeProc();
    }),
    execFile: vi.fn((...args: unknown[]) => {
      const cb = args[args.length - 1] as (err: Error | null, stdout: string, stderr: string) => void;
      if (typeof cb === "function") {
        const binary = args[0] as string;
        if (binary === "which" || binary === "where") {
          const target = (args[1] as string[])?.[0] ?? "";
          cb(null, `/usr/local/bin/${target}\n`, "");
        } else {
          cb(null, "1.0.0\n", "");
        }
        return;
      }
      return (actual.execFile as Function)(...args);
    }),
    execFileSync: vi.fn((_shell: string, _args: string[], _opts: unknown) => {
      return "/usr/local/bin:/usr/bin:/bin";
    }),
  };
});

function makeFakeProc() {
  const { Readable, Writable } = require("node:stream");
  const stdout = new Readable({ read() { this.push(null); } });
  const stderr = new Readable({ read() { this.push(null); } });
  const stdin: any = new Writable({ write(_c: any, _e: any, cb: any) { cb(); } });
  stdin.endCalled = false;
  const origEnd = stdin.end.bind(stdin);
  stdin.end = (...args: any[]) => { stdin.endCalled = true; return origEnd(...args); };
  const fake: any = {
    stdin,
    stdout,
    stderr,
    on: vi.fn(),
    kill: vi.fn(),
  };
  return fake;
}

import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";
import { getExpandedEnv } from "../electron/orchestrator-adapters";

beforeEach(() => {
  spawnCalls.length = 0;
});

describe("codex app-server wiring", () => {
  it("spawns `codex app-server --listen stdio://` with correct cwd and stdio", () => {
    const adapter = new CodexAppServerAdapter();
    adapter.spawn("test prompt", "/tmp/workdir", { CUSTOM_VAR: "value" });

    expect(spawnCalls).toHaveLength(1);
    const call = spawnCalls[0];
    expect(call.binary).toBe("codex");
    expect(call.args).toEqual(["app-server", "--listen", "stdio://"]);

    const opts = call.options as { cwd: string; stdio: unknown; env: Record<string, string> };
    expect(opts.cwd).toBe("/tmp/workdir");
    expect(opts.stdio).toEqual(["pipe", "pipe", "pipe"]);
  });

  it("env includes custom vars and PATH expansion via getExpandedEnv", () => {
    const adapter = new CodexAppServerAdapter();
    adapter.spawn("test prompt", "/tmp/workdir", { CUSTOM_VAR: "value" });

    const call = spawnCalls[0];
    const opts = call.options as { env: Record<string, string> };

    // Custom env var is forwarded.
    expect(opts.env.CUSTOM_VAR).toBe("value");

    // PATH is expanded (contains extra paths from getExpandedEnv).
    expect(opts.env.PATH).toBeDefined();
    expect(opts.env.PATH.length).toBeGreaterThan(0);

    // Verify the env matches what getExpandedEnv produces.
    const expected = getExpandedEnv({ CUSTOM_VAR: "value" });
    expect(opts.env).toEqual(expected);
  });

  it("extraArgs are appended after the base args", () => {
    const adapter = new CodexAppServerAdapter(["--model", "o3"]);
    adapter.spawn("test prompt", "/tmp/workdir");

    expect(spawnCalls).toHaveLength(1);
    const call = spawnCalls[0];
    expect(call.binary).toBe("codex");
    expect(call.args).toEqual(["app-server", "--listen", "stdio://", "--model", "o3"]);
  });
});
