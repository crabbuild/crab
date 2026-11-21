import { afterEach, expect, it, vi } from "vitest";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { IPty } from "node-pty";
import { NodePtySubprocessHandle } from "../electron/daemon/subprocess-handle";
import { TerminalHost } from "../electron/daemon/terminal-host";
import { DaemonServer } from "../electron/daemon/daemon-server";
import { DaemonClient } from "../electron/daemon/daemon-client";

vi.mock("node:child_process", async original => ({
  ...await original<typeof import("node:child_process")>(),
  fork: vi.fn(() => { throw new Error("Replacement attempted before terminal cleanup finished"); }),
}));

const platform = process.platform;
afterEach(() => {
  Object.defineProperty(process, "platform", { value: platform });
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function consoleProcess() {
  let exit: (event: { exitCode: number }) => void = () => {};
  const kill = vi.fn();
  const terminal = {
    pid: 12345, kill, onData() {}, onExit(callback: typeof exit) { exit = callback; },
  } as unknown as IPty;
  return { handle: new NodePtySubprocessHandle(terminal), kill, exit: () => exit({ exitCode: 0 }) };
}

const shutdownCases = ["host", "daemon"].flatMap(owner =>
  ["running", "retired", "timed-out"].map(state => ({ owner, state })),
);
shutdownCases.push({ owner: "idle daemon", state: "timed-out" });
it.each(shutdownCases)(
  "joins ConPTY closure for a $state session before $owner shutdown completes", async ({ owner, state }) => {
    Object.defineProperty(process, "platform", { value: "win32" });
    const previous = consoleProcess();
    const current = consoleProcess();
    const spawn = vi.fn().mockReturnValueOnce(previous.handle).mockReturnValueOnce(current.handle);
    const server = owner !== "host"
      ? new DaemonServer({ socketPath: "", tokenPath: "", pidPath: "", spawnSubprocess: spawn, idleTimeoutMs: 0 })
      : null;
    const host = server?.terminalHost ?? new TerminalHost({ spawnSubprocess: spawn });
    const options = {
      sessionId: "owned", cols: 80, rows: 24, shell: "cmd.exe",
      streamClient: { id: "client", sendData() {}, sendExit() {}, sendStateChanged() {} },
    };
    host.createOrAttach(options);
    if (state !== "running") {
      vi.useFakeTimers();
      host.kill("owned");
      vi.advanceTimersByTime(5000);
      // A removed session can still own its directory while another shell runs.
      if (state === "retired") host.createOrAttach({ ...options, sessionId: "current" });
      vi.useRealTimers();
    }
    let closed = false;
    const shutdownOwner = () => server ? server.shutdown({ killSessions: owner !== "idle daemon" }) : host.dispose();
    const shutdown = Promise.all([shutdownOwner(), shutdownOwner()]).then(() => { closed = true; });
    current.exit();
    await Promise.resolve();
    await Promise.resolve();
    const pendingBeforeExit = !closed;
    previous.exit();
    await shutdown;
    expect({ pendingBeforeExit, closed }).toEqual({ pendingBeforeExit: true, closed: true });
});

it("releases an exited ConPTY without enumerating its closed console again", () => {
  Object.defineProperty(process, "platform", { value: "win32" });
  const console = consoleProcess();
  console.exit();
  console.handle.dispose();
  expect(console.kill).not.toHaveBeenCalled();
});

it("defers daemon replacement until retired terminal resources close", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "crab-upgrade-close-"));
  const terminal = consoleProcess();
  let close = () => {};
  const disposal = new Promise<void>(resolve => { close = resolve; });
  vi.spyOn(terminal.handle, "dispose").mockReturnValue(disposal);
  const server = new DaemonServer({
    socketPath: path.join(root, "daemon-v2.sock"), tokenPath: path.join(root, "daemon-v2.token"),
    pidPath: path.join(root, "daemon-v2.pid"), idleTimeoutMs: 0, spawnSubprocess: () => terminal.handle,
  });
  const client = new DaemonClient({ socketDir: root, daemonEntryPath: "", onSessionData() {}, onSessionExit() {}, onAgentStateChange() {} });
  try {
    await server.start();
    await client.request("createOrAttach", { sessionId: "retiring", cols: 80, rows: 24 });
    terminal.exit();
    // Starting the successor before cleanup finishes can exhaust its bounded
    // socket-bind retries while the predecessor is still waiting to shut down.
    await expect(client.request("prepare-upgrade", { successorPath: path.join(root, "successor.cjs") }))
      .rejects.toThrow("deferred while terminals");
  } finally {
    close();
    client.disconnect();
    await server.shutdown({ killSessions: true });
    rmSync(root, { recursive: true, force: true });
  }
});
