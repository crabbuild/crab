import { expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { DaemonSessions, daemonDirectory, prepareDaemonHistory } from "../electron/daemon/daemon-sessions";
import { DaemonClient } from "../electron/daemon/daemon-client";
import { DaemonServer } from "../electron/daemon/daemon-server";
import { createNodePtySpawner } from "../electron/daemon/subprocess-handle";
import type { CreateOrAttachResult } from "../electron/daemon/protocol";

it.each(["root", "runtime"])("preserves a %s owner's real terminal and routes new sessions after its runtime disappears", async location => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-daemon-owners-"));
  const oldDirectory = location === "root" ? root : daemonDirectory(root, path.join(root, "old-app.cjs"));
  const currentDirectory = daemonDirectory(root, path.join(root, "new-app.cjs"));
  let runtimeExists = true;
  const spawn = createNodePtySpawner();
  function server(directory: string, previous: boolean) {
    fs.mkdirSync(directory, { recursive: true });
    return new DaemonServer({
      socketPath: path.join(directory, "daemon-v2.sock"), tokenPath: path.join(directory, "daemon-v2.token"),
      pidPath: path.join(directory, "daemon-v2.pid"), idleTimeoutMs: 0,
      spawnSubprocess: options => {
        if (previous && !runtimeExists) throw new Error("Runtime spawn helper was removed");
        return spawn(options);
      },
    });
  }
  const previous = server(oldDirectory, true);
  const current = server(currentDirectory, false);
  const oldClient = new DaemonClient({ socketDir: oldDirectory, daemonEntryPath: "", onSessionData() {}, onSessionExit() {}, onAgentStateChange() {} });
  const output = new Map<string, string>();
  const disconnected = vi.fn();
  const sessions = () => new DaemonSessions({
    socketDir: currentDirectory, controlRoot: root, daemonEntryPath: "", log() {},
    onSessionData: (id, data) => output.set(id, (output.get(id) ?? "") + data),
    onSessionExit() {}, onAgentStateChange() {}, onSessionsDisconnected: disconnected,
  });
  let client = sessions();
  const payload = { cwd: root, cols: 80, rows: 24, env: { HOME: root, ZDOTDIR: root } };
  try {
    await previous.start();
    const before = await oldClient.request<CreateOrAttachResult>("createOrAttach", { ...payload, sessionId: "old" });
    const ending = await oldClient.request<CreateOrAttachResult>("createOrAttach", { ...payload, sessionId: "ending" });
    runtimeExists = false;
    await expect(oldClient.request("createOrAttach", { ...payload, sessionId: "broken-spawn" })).rejects.toThrow("spawn helper was removed");
    await current.start();
    const attached = await client.request<CreateOrAttachResult>("createOrAttach", { ...payload, sessionId: "old" });
    expect(attached).toMatchObject({ pid: before.pid, isNew: false });
    const fresh = await client.request<CreateOrAttachResult>("createOrAttach", { ...payload, sessionId: "new" });
    expect(fresh.pid).not.toBe(before.pid);
    await client.request("createOrAttach", { ...payload, sessionId: "ending" });
    await client.request("write", { sessionId: "ending", data: "exit\r" });
    await vi.waitFor(() => expect(() => process.kill(ending.pid!, 0)).toThrowError(expect.objectContaining({ code: "ESRCH" })));
    expect(await client.request("createOrAttach", { ...payload, sessionId: "ending" })).toMatchObject({ isNew: true });
    expect(client.ownsCurrentSession("ending")).toBe(true);
    const command = process.platform === "win32" ? "echo owner-%COMPUTERNAME%\r" : "printf 'owner-%s\\n' ready\r";
    const expected = process.platform === "win32" ? `owner-${process.env.COMPUTERNAME}` : "owner-ready";
    for (const sessionId of ["old", "new"]) {
      await client.request("resize", { sessionId, cols: 96, rows: 32 });
      await client.request("write", { sessionId, data: command });
      await vi.waitFor(() => expect(output.get(sessionId)).toContain(expected), { timeout: 5000 });
    }
    expect(await client.request("getMetrics")).toMatchObject({ activeSessionCount: 3 });
    client.disconnect();
    client = sessions();
    expect(await client.request("createOrAttach", { ...payload, sessionId: "old" })).toMatchObject({ pid: before.pid, isNew: false });
    await previous.shutdown({ killSessions: true });
    await vi.waitFor(() => expect(disconnected).toHaveBeenCalledWith(["old"]));
    expect(await client.request("createOrAttach", { ...payload, sessionId: "new" })).toMatchObject({ pid: fresh.pid, isNew: false });
  } finally {
    oldClient.disconnect(); client.disconnect();
    await previous.shutdown({ killSessions: true });
    await current.shutdown({ killSessions: true });
    fs.rmSync(root, { recursive: true, force: true });
  }
}, 20_000);

it("claims only dead runtime history for cold restore across application replacement", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-daemon-history-"));
  const dead = daemonDirectory(root, "dead");
  const live = daemonDirectory(root, "live");
  const current = daemonDirectory(root, "current");
  const session = path.join("repository", "task@@terminal");
  for (const directory of [dead, live]) {
    fs.mkdirSync(path.join(directory, "history", session), { recursive: true });
    fs.writeFileSync(path.join(directory, "history", session, "meta.json"), "{}");
    fs.writeFileSync(path.join(directory, "history", session, "checkpoint.json"), directory);
  }
  fs.writeFileSync(path.join(live, "daemon-v2.pid"), JSON.stringify({ pid: process.pid, startedAtMs: Date.now(), entryPath: "live" }));
  try {
    const destination = prepareDaemonHistory(current)!;
    expect(fs.readFileSync(path.join(destination, session, "checkpoint.json"), "utf8")).toBe(dead);
    expect(fs.existsSync(path.join(live, "history", session, "checkpoint.json"))).toBe(true);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
