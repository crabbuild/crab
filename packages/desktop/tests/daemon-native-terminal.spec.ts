import { it, expect, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { DaemonClient } from "../electron/daemon/daemon-client";
import { DaemonServer } from "../electron/daemon/daemon-server";
import { PROTOCOL_VERSION } from "../electron/daemon/protocol";
import { createNodePtySpawner, NodePtySubprocessHandle } from "../electron/daemon/subprocess-handle";
import { TerminalHost } from "../electron/daemon/terminal-host";
import type { IPty } from "node-pty";
import type { SessionInfo } from "../electron/daemon/types";
import { socketAddress } from "../electron/daemon/socket-address";

it.each(["win32", "darwin", "linux"])("terminates a %s PTY using its supported kill contract", platform => {
  const descriptor = Object.getOwnPropertyDescriptor(process, "platform")!;
  const kill = vi.fn((signal?: string) => {
    if (platform === "win32" && signal) throw new Error("Signals not supported on windows.");
  });
  const pty = { pid: 1, onData: vi.fn(), onExit: vi.fn(), kill } as unknown as IPty;
  try {
    Object.defineProperty(process, "platform", { value: platform });
    new NodePtySubprocessHandle(pty).kill("SIGTERM");
    expect(kill).toHaveBeenCalledWith(platform === "win32" ? undefined : "SIGTERM");
  } finally { Object.defineProperty(process, "platform", descriptor); }
});

it.each(["short", "long"])("executes and terminates a real shell through a %s control path", async length => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-pty-"));
  const directory = length === "long" ? path.join(root, "nested-home-".repeat(6)) : root;
  fs.mkdirSync(directory, { recursive: true });
  const base = path.join(directory, `daemon-v${PROTOCOL_VERSION}`);
  const server = new DaemonServer({
    socketPath: `${base}.sock`, tokenPath: `${base}.token`, pidPath: `${base}.pid`,
    spawnSubprocess: createNodePtySpawner(), idleTimeoutMs: 0,
  });
  let output = "";
  const client = new DaemonClient({
    socketDir: directory, daemonEntryPath: "",
    onSessionData: (_id, data) => { output += data; },
    onSessionExit: () => {}, onAgentStateChange: () => {},
  });
  const previousShell = process.env.SHELL;
  try {
    delete process.env.SHELL;
    await server.start();
    await client.ensureConnected();
    await client.request("createOrAttach", { sessionId: "native-test", cols: 80, rows: 24, cwd: root });
    // Split the marker so shell input echo cannot satisfy the output assertion.
    const command = process.platform === "win32"
      ? "echo crab-pty-%COMPUTERNAME%\r"
      : "printf 'crab-pty-%s\\n' ready\r";
    const expected = process.platform === "win32" ? `crab-pty-${process.env.COMPUTERNAME}` : "crab-pty-ready";
    await client.request("write", { sessionId: "native-test", data: command });
    await vi.waitFor(() => expect(output).toContain(expected), { timeout: 10_000 });
    const sessions = await client.request<SessionInfo[]>("listSessions");
    const session = sessions.find(session => session.sessionId === "native-test");
    if (!session?.pid) throw new Error("Native terminal has no process identity");
    const pid = session.pid;
    await client.request("kill", { sessionId: "native-test" });
    await vi.waitFor(() => {
      expect(() => process.kill(pid, 0)).toThrowError(expect.objectContaining({ code: "ESRCH" }));
    }, { timeout: 10_000 });
  } finally {
    if (previousShell === undefined) delete process.env.SHELL;
    else process.env.SHELL = previousShell;
    client.disconnect();
    await server.shutdown({ killSessions: true });
    const address = socketAddress(`${base}.sock`);
    if (process.platform !== "win32" && address !== `${base}.sock`) {
      fs.rmSync(path.dirname(address), { recursive: true, force: true });
    }
    fs.rmSync(root, { recursive: true, force: true });
  }
}, 20_000);

it.skipIf(process.platform === "win32").each(["public", "symlink"])("rejects a %s directory for a shortened socket", async kind => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-private-ipc-"));
  const base = path.join(root, "nested-home-".repeat(6), `daemon-v${PROTOCOL_VERSION}`);
  fs.mkdirSync(path.dirname(base), { recursive: true });
  const directory = path.dirname(socketAddress(`${base}.sock`));
  const server = new DaemonServer({
    socketPath: `${base}.sock`, tokenPath: `${base}.token`, pidPath: `${base}.pid`,
    spawnSubprocess: createNodePtySpawner(), idleTimeoutMs: 0,
  });
  try {
    if (kind === "public") { fs.mkdirSync(directory); fs.chmodSync(directory, 0o755); }
    else fs.symlinkSync(root, directory);
    await expect(server.start()).rejects.toThrow("Daemon socket directory is not private and owned");
  } finally {
    await server.shutdown({ killSessions: true });
    fs.rmSync(directory, { recursive: true, force: true });
    fs.rmSync(root, { recursive: true, force: true });
  }
});

it.skipIf(process.platform === "win32" || !fs.existsSync("/bin/bash"))("honors the selected shell and runs a command line after shell readiness", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-shell-profile-"));
  const host = new TerminalHost({ spawnSubprocess: createNodePtySpawner() });
  let output = "";
  try {
    host.createOrAttach({
      sessionId: "selected-shell", cwd: root, cols: 100, rows: 30,
      shell: "/bin/bash", shellReadySupported: true,
      startupCommand: "printf 'selected-%s:%s\\n' \"$BASH_VERSION\" \"$CRAB_PROFILE_PROOF\"",
      env: { ...process.env, HOME: root, CRAB_PROFILE_PROOF: "argument with spaces" } as Record<string, string>,
      streamClient: { id: "profile-client", sendData: (_id, data) => { output += data.toString("utf8"); }, sendExit: () => {}, sendStateChanged: () => {} },
    });
    await vi.waitFor(() => expect(output).toMatch(/selected-\d[^\r\n]*:argument with spaces/), { timeout: 5000 });
  } finally { host.dispose(); fs.rmSync(root, { recursive: true, force: true }); }
});
