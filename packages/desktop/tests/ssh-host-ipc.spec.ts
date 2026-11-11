import { afterEach, describe, expect, it, vi } from "vitest";
import { generateKeyPairSync } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { utils } from "ssh2";
import { KnownHostsManager } from "../electron/ssh-known-hosts";
import { SshProfileManager } from "../electron/ssh-profiles";
import { SshSessionManager } from "../electron/ssh-manager";
import { registerSshIpcHandlers } from "../electron/ssh-ipc";
const { handlers } = vi.hoisted(() => ({ handlers: new Map<string, (...args: unknown[]) => unknown>() }));
vi.mock("electron", () => ({
  ipcMain: { handle: (channel: string, handler: (...args: unknown[]) => unknown) => handlers.set(channel, handler) },
  BrowserWindow: { getAllWindows: () => [] }, powerMonitor: { on: () => {} },
}));
afterEach(() => { vi.clearAllTimers(); vi.useRealTimers(); handlers.clear(); });
describe("SSH host settings IPC", () => {
  it("lists persisted approvals and removes only the requested key", async () => {
    vi.useFakeTimers(); const directory = mkdtempSync(path.join(tmpdir(), "crab-host-ipc-"));
    try {
      const key = utils.parseKey(generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ type: "pkcs1", format: "pem" }));
      if (key instanceof Error || Array.isArray(key)) throw new Error("Invalid owned key");
      const hosts = new KnownHostsManager(path.join(directory, "pins.json"), path.join(directory, "user"), path.join(directory, "global"));
      hosts.accept("fixture.invalid", 22, key.getPublicSSH()); hosts.accept("fixture.invalid", 2222, key.getPublicSSH());
      registerSshIpcHandlers(new SshProfileManager(path.join(directory, "profiles.json")), new SshSessionManager(async () => {}), hosts);
      await handlers.get("ssh:remove-known-host")!(undefined, "fixture.invalid", 22, "ssh-rsa");
      expect(await handlers.get("ssh:list-known-hosts")!(undefined)).toEqual([expect.objectContaining({ hostname: "fixture.invalid", port: 2222, keyType: "ssh-rsa" })]);
    } finally { rmSync(directory, { recursive: true, force: true }); }
  });
});
