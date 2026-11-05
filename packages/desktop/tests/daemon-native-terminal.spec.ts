import { it, expect, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { DaemonClient } from "../electron/daemon/daemon-client";
import { DaemonServer } from "../electron/daemon/daemon-server";
import { PROTOCOL_VERSION } from "../electron/daemon/protocol";
import { createNodePtySpawner } from "../electron/daemon/subprocess-handle";

it("executes a real shell command through authenticated daemon connections", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-pty-"));
  const base = path.join(root, `daemon-v${PROTOCOL_VERSION}`);
  const server = new DaemonServer({
    socketPath: `${base}.sock`, tokenPath: `${base}.token`, pidPath: `${base}.pid`,
    spawnSubprocess: createNodePtySpawner(), idleTimeoutMs: 0,
  });
  let output = "";
  const client = new DaemonClient({
    socketDir: root, daemonEntryPath: "",
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
  } finally {
    if (previousShell === undefined) delete process.env.SHELL;
    else process.env.SHELL = previousShell;
    client.disconnect();
    await server.shutdown({ killSessions: true });
    fs.rmSync(root, { recursive: true, force: true });
  }
}, 20_000);
