import { afterEach, expect, it, vi } from "vitest";
import type { IPty } from "node-pty";
import { NodePtySubprocessHandle } from "../electron/daemon/subprocess-handle";
import { TerminalHost } from "../electron/daemon/terminal-host";
import { DaemonServer } from "../electron/daemon/daemon-server";

const platform = process.platform;
afterEach(() => {
  Object.defineProperty(process, "platform", { value: platform });
  vi.useRealTimers();
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
it.each(shutdownCases)(
  "joins ConPTY closure for a $state session before $owner shutdown completes", async ({ owner, state }) => {
    Object.defineProperty(process, "platform", { value: "win32" });
    const previous = consoleProcess();
    const current = consoleProcess();
    const spawn = vi.fn().mockReturnValueOnce(previous.handle).mockReturnValueOnce(current.handle);
    const server = owner === "daemon"
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
    const shutdownOwner = () => server ? server.shutdown({ killSessions: true }) : host.dispose();
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
