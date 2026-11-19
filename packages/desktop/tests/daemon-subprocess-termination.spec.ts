import { afterEach, expect, it, vi } from "vitest";
import type { IPty } from "node-pty";
import { NodePtySubprocessHandle } from "../electron/daemon/subprocess-handle";

const platform = process.platform;
afterEach(() => {
  Object.defineProperty(process, "platform", { value: platform });
  vi.restoreAllMocks();
});

it.each([false, true])("ConPTY owns forced termination after a prior request: %s", graceful => {
  Object.defineProperty(process, "platform", { value: "win32" });
  const kill = vi.fn();
  const processKill = vi.spyOn(process, "kill").mockReturnValue(true);
  const pty = { pid: 12345, onData: vi.fn(), onExit: vi.fn(), kill } as unknown as IPty;
  const handle = new NodePtySubprocessHandle(pty);
  if (graceful) handle.kill("SIGTERM");
  handle.forceKill();
  handle.dispose();
  expect({ conpty: kill.mock.calls, process: processKill.mock.calls }).toEqual({ conpty: [[]], process: [] });
});

it("POSIX force termination still precedes descriptor disposal", () => {
  Object.defineProperty(process, "platform", { value: "linux" });
  const calls: string[] = [];
  vi.spyOn(process, "kill").mockImplementation(() => { calls.push("SIGKILL"); return true; });
  const pty = { pid: 12345, onData: vi.fn(), onExit: vi.fn(), kill: () => calls.push("dispose") } as unknown as IPty;
  const handle = new NodePtySubprocessHandle(pty);
  handle.forceKill();
  handle.dispose();
  expect(calls).toEqual(["SIGKILL", "dispose"]);
});
