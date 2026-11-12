import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
vi.mock("../electron/env/userShellEnv", () => ({ getExpandedEnv: vi.fn() }));
import { getExpandedEnv } from "../electron/env/userShellEnv";
import { agentRegistry } from "../electron/orchestrator-registry";
import { _setRegistry, detectAgents, getCachedAgents } from "../electron/orchestrator-adapters";
import { saveAgentConfig } from "../electron/orchestrator-settings";
import { registerTerminalAgentCommand } from "../electron/terminal-agent-command";

describe.skipIf(process.platform === "win32")("host CLI presets", () => {
  let root: string;
  let bin: string;
  let configured: string;
  let previousSettings: string | undefined;
  beforeEach(() => {
    root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-host-cli-"));
    bin = path.join(root, "bin");
    fs.mkdirSync(bin);
    configured = path.join(root, "configured cli");
    const script = '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "fixture 1.2.3"; else printf "%s\\n" "$@" > "$CLI_PROOF"; printf "%s" "$CLI_CONFIG" >> "$CLI_PROOF"; fi\n';
    fs.writeFileSync(path.join(bin, "fixture-cli"), script, { mode: 0o755 });
    fs.writeFileSync(configured, script, { mode: 0o755 });
    previousSettings = process.env.CRAB_DESKTOP_SETTINGS_PATH;
    process.env.CRAB_DESKTOP_SETTINGS_PATH = path.join(root, "settings.json");
    vi.mocked(getExpandedEnv).mockImplementation(async extra => ({ PATH: `${bin}:/usr/bin:/bin`, ...extra }));
    _setRegistry({
      createAdapter: () => { throw new Error("CLI fixture does not create adapters"); },
      listRegistered: () => [{ type: "fixture", name: "Fixture CLI", manifest: { binary: "fixture-cli" }, capabilities: {} }],
    });
  });
  afterEach(() => {
    _setRegistry(agentRegistry);
    if (previousSettings === undefined) delete process.env.CRAB_DESKTOP_SETTINGS_PATH;
    else process.env.CRAB_DESKTOP_SETTINGS_PATH = previousSettings;
    fs.rmSync(root, { recursive: true, force: true });
  });
  function handler(shell = "/bin/sh") {
    const handlers = new Map<string, (...args: unknown[]) => unknown>();
    registerTerminalAgentCommand({ handle: (name, listener) => { handlers.set(name, listener); } }, () => shell);
    return (command: string) => handlers.get("crab:term-agent-command")!(null, { agentType: "fixture", command }) as Promise<{ command: string; env?: Record<string, string> }>;
  }

  it("resolves installed binaries, explicit paths, and disabled settings without stale cache", async () => {
    expect((await detectAgents({ force: true }))[0]).toMatchObject({ available: true, path: path.join(bin, "fixture-cli") });
    saveAgentConfig("fixture", { binaryPath: configured });
    expect(getCachedAgents()).toBeNull();
    expect((await detectAgents())[0]).toMatchObject({ available: true, configured: true, path: configured });
    saveAgentConfig("fixture", { binaryPath: configured, enabled: false });
    expect((await detectAgents())[0]).toMatchObject({ available: false });
    expect((await detectAgents())[0].configured).toBeUndefined();
  });

  it("does not substitute an installed binary for a broken explicit override", async () => {
    saveAgentConfig("fixture", { binaryPath: path.join(root, "missing-cli") });
    expect((await detectAgents({ force: true }))[0]).toMatchObject({ available: false, configured: true, path: "" });
    await expect(handler()("fixture-cli")).rejects.toThrow("not executable");
  });

  it("keeps a late probe from replacing a newer configuration's catalog", async () => {
    let finish!: (env: Record<string, string>) => void;
    const env = { PATH: `${bin}:/usr/bin:/bin` };
    vi.mocked(getExpandedEnv).mockReturnValueOnce(new Promise(resolve => { finish = resolve; }));
    const previous = detectAgents({ force: true });
    try {
      saveAgentConfig("fixture", { binaryPath: configured });
      await detectAgents();
      finish(env);
      await previous;
      expect(getCachedAgents()?.[0].path).toBe(configured);
    } finally { finish(env); await previous; }
  });

  it("executes the configured path and arguments with its environment", async () => {
    const proof = path.join(root, "cli.txt");
    saveAgentConfig("fixture", { binaryPath: configured, extraArgs: ["saved argument"], env: { CLI_PROOF: proof, CLI_CONFIG: "configured-value" } });
    await detectAgents({ force: true });
    const prepared = await handler()("fixture-cli --resume");
    execFileSync("/bin/sh", ["-c", prepared.command], { env: { PATH: "/usr/bin:/bin", ...prepared.env } });
    expect(fs.readFileSync(proof, "utf8")).toBe("saved argument\n--resume\nconfigured-value");
  });

  it("accepts a manifest's executable path containing spaces", async () => {
    _setRegistry({ createAdapter: () => { throw new Error("Not used"); }, listRegistered: () => [
      { type: "fixture", name: "Fixture CLI", manifest: { binary: configured }, capabilities: {} },
    ] });
    await detectAgents({ force: true });
    expect(await handler()(`${configured} --resume`)).toMatchObject({ command: `'${configured}' --resume` });
  });

  it.each(["pwsh.exe", "powershell.exe", "cmd.exe"])("formats a configured path for %s", async shell => {
    saveAgentConfig("fixture", { binaryPath: configured });
    await detectAgents({ force: true });
    const prepared = await handler(shell)("fixture-cli --resume");
    expect(prepared.command).toBe(shell === "cmd.exe" ? `"${configured}" --resume` : `& '${configured}' --resume`);
  });
});
