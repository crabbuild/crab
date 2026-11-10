import { it, expect, vi } from "vitest";
import { EventEmitter } from "node:events";
import { mkdtempSync, mkdirSync, readFileSync, realpathSync, existsSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import * as path from "node:path";
import type { Client } from "ssh2";
import type { SshSessionManager } from "../electron/ssh-manager";
import { RemoteTerminalManager } from "../electron/ssh-terminal";
import { terminalSshServer } from "./fixtures/ssh-terminal-server";

function manager(client: Client) {
  const connections = Object.assign(new EventEmitter(), { getConnection: () => ({ status: "connected", client }) });
  return new RemoteTerminalManager(connections as unknown as SshSessionManager);
}

it.skipIf(process.platform === "win32")("resumes the same foreground SSH process after transport and application replacement", async () => {
  const root = mkdtempSync("/tmp/crab-ssh-live-");
  const cwd = path.join(root, "project's work");
  mkdirSync(cwd);
  const proof = path.join(root, "agent.jsonl");
  const server = await terminalSshServer(root);
  let first: Client | undefined;
  let second: Client | undefined;
  let restored: RemoteTerminalManager | undefined;
  let terminalId: string | undefined;
  try {
    first = await server.connect();
    const original = manager(first);
    let initialOutput = "";
    original.on("ssh:terminal-data", ({ data }) => { initialOutput += data; });
    terminalId = await original.openTerminal("remote", 90, 28, cwd);
    const executable = process.env.CRAB_TEST_NODE ?? "node";
    const agent = path.resolve(__dirname, "fixtures/terminal-recovery/agent.cjs");
    original.writeToTerminal(terminalId, `${JSON.stringify(executable)} ${JSON.stringify(agent)} ${JSON.stringify(proof)}\r`);
    await vi.waitFor(() => expect(existsSync(proof), initialOutput).toBe(true), { timeout: 10_000 });
    const before = JSON.parse(readFileSync(proof, "utf8").split("\n")[0]);
    original.detachTerminal(terminalId);
    first.end();
    second = await server.connect();
    restored = manager(second);
    let output = "";
    restored.on("ssh:terminal-data", ({ data }) => { output += data; });
    expect(await restored.openTerminal("remote", 90, 28, cwd, terminalId)).toBe(terminalId);
    await vi.waitFor(() => expect(output).toContain(String(before.pid)), { timeout: 5000 });
    restored.resizeTerminal(terminalId, 110, 35);
    await vi.waitFor(() => {
      const records = readFileSync(proof, "utf8").trim().split("\n").map(line => JSON.parse(line));
      expect(records.some(value => value.input === "resize" && value.cols === 110 && value.rows === 35)).toBe(true);
    }, { timeout: 5000 });
    restored.writeToTerminal(terminalId, "after-ssh-upgrade\r");
    await vi.waitFor(() => {
      const records = readFileSync(proof, "utf8").trim().split("\n").map(line => JSON.parse(line));
      expect(records.find(value => value.input === "after-ssh-upgrade")).toMatchObject({ pid: before.pid, cols: 110, rows: 35, cwd: realpathSync(cwd) });
    }, { timeout: 5000 });
    await restored.closeTerminal(terminalId);
    await expect(restored.openTerminal("remote", 80, 24, cwd, terminalId)).rejects.toThrow();
  } finally {
    if (terminalId) await restored?.closeTerminal(terminalId).catch(() => {});
    first?.end(); second?.end();
    await server.close();
    try { execFileSync("tmux", ["-L", "crab-desktop", "kill-server"], { env: server.env, stdio: "ignore" }); } catch {}
    rmSync(root, { recursive: true, force: true });
  }
}, 30_000);

it.skipIf(process.platform === "win32")("refuses a non-durable SSH shell when tmux is missing", async () => {
  const root = mkdtempSync("/tmp/crab-ssh-missing-");
  const server = await terminalSshServer(root, "/does-not-exist");
  const client = await server.connect();
  try { await expect(manager(client).openTerminal("remote", 80, 24)).rejects.toThrow("require tmux"); }
  finally { client.end(); await server.close(); rmSync(root, { recursive: true, force: true }); }
});
