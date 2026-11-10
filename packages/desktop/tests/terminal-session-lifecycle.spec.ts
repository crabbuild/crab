import { expect, it, vi } from "vitest";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { DaemonClient } from "../electron/daemon/daemon-client";
import { DaemonServer } from "../electron/daemon/daemon-server";
import { createDaemonBridge } from "../electron/daemon/daemon-bridge";
import { createNodePtySpawner } from "../electron/daemon/subprocess-handle";
import { TermRouter, type TermSpawnResult } from "../electron/term-router";
import { CircuitBreaker } from "../electron/daemon/circuit-breaker";
import { DaemonSpawner } from "../electron/daemon/daemon-spawner";
import { detectAndHandlePathMismatch } from "../electron/daemon/path-mismatch-detection";

it("keeps an interactive foreground process when the app disconnects and reconnects", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "crab-terminal-lifecycle-"));
  const server = new DaemonServer({
    socketPath: path.join(root, "daemon-v2.sock"),
    tokenPath: path.join(root, "daemon-v2.token"),
    pidPath: path.join(root, "daemon-v2.pid"),
    spawnSubprocess: createNodePtySpawner(), idleTimeoutMs: 0,
  });
  const client = new DaemonClient({
    socketDir: root, daemonEntryPath: "",
    onSessionData: (id, data) => bridge.handleSessionData(id, data),
    onSessionExit: () => {}, onAgentStateChange: () => {},
  });
  let output = "";
  const bridge = createDaemonBridge({
    client, app: { on: () => {} }, getWorkspacePath: () => root,
    getMainWindow: () => ({ isDestroyed: () => false, webContents: {
      send: (_channel: string, event: { data: string }) => { output += event.data; },
    } }),
  });
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  const router = new TermRouter({
    ipcMain: { handle: (name, handler) => { handlers.set(name, handler); }, removeHandler: name => { handlers.delete(name); } },
    daemonBridge: { ...bridge, handleSpawn: async (payload, id) => ({ daemonSessionId: (await bridge.handleSpawn(payload, id)).id }) },
    inProcessBridge: { handleSpawn: vi.fn(), handleWrite: vi.fn(), handleResize: vi.fn(), handleKill: vi.fn(), dispose: vi.fn() },
    modeResolution: { current: () => ({ daemonReady: true }) }, circuitBreaker: new CircuitBreaker(),
    getMainWindow: () => null,
  });
  writeFileSync(path.join(root, "agent.cjs"), `
    console.log('agent-ready:' + process.pid);
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', line => console.log('agent-reply:' + process.pid + ':' + line.trim()));
  `);
  try {
    await server.start();
    await client.ensureConnected();
    router.register();
    const spawned = await handlers.get("crab:term-spawn")!(null, { cwd: root }) as TermSpawnResult;
    expect(spawned.resolvedSessionType).toBe("daemon");
    await bridge.handleWrite(spawned.id, `${JSON.stringify(process.execPath)} agent.cjs\r`);
    await vi.waitFor(() => expect(output).toMatch(/agent-ready:(\d+)/), { timeout: 5000 });
    const agentPid = Number(/agent-ready:(\d+)/.exec(output)![1]);
    await expect(client.request("prepare-upgrade", { successorPath: path.resolve(__dirname, "../dist-electron/daemon/daemon-main.cjs") }))
      .rejects.toThrow("deferred while terminals are running");
    const sessionId = bridge.getDaemonSessionIds()[0];
    bridge.dispose();
    client.disconnect();
    await client.ensureConnected();
    const restored = await client.request<{ isNew: boolean; snapshot: { snapshotAnsi: string } }>("createOrAttach", {
      sessionId, cols: 100, rows: 35, cwd: root,
    });
    expect(restored.isNew).toBe(false);
    expect(restored.snapshot.snapshotAnsi).toContain(`agent-ready:${agentPid}`);
    // Reconnect streaming without a renderer mapping; input must reach the same foreground process.
    let resumed = "";
    const reader = new DaemonClient({
      socketDir: root, daemonEntryPath: "", onSessionData: (_id, data) => { resumed += data; },
      onSessionExit: () => {}, onAgentStateChange: () => {},
    });
    try {
      await reader.ensureConnected();
      await reader.request("createOrAttach", { sessionId, cols: 100, rows: 35, cwd: root });
      await reader.request("write", { sessionId, data: "after-reopen\r" });
      await vi.waitFor(() => expect(resumed).toContain(`agent-reply:${agentPid}:after-reopen`), { timeout: 5000 });
    } finally { reader.disconnect(); }
  } finally {
    router.dispose(); bridge.dispose(); client.disconnect();
    await server.shutdown({ killSessions: true });
    rmSync(root, { recursive: true, force: true });
  }
}, 15_000);

it("reuses the live daemon after the application entry path is replaced by an upgrade", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "crab-terminal-upgrade-"));
  const socketDir = path.join(root, "daemon");
  mkdirSync(socketDir);
  const entry = path.resolve(__dirname, "../dist-electron/daemon/daemon-main.cjs");
  const oldEntry = path.join(root, "old-app.cjs");
  const newEntry = path.join(root, "new-app.cjs");
  const wrapper = `require(${JSON.stringify(entry)});`;
  writeFileSync(oldEntry, wrapper);
  writeFileSync(newEntry, wrapper);
  let output = "";
  const client = new DaemonClient({
    socketDir, daemonEntryPath: oldEntry, onSessionData: (_id, data) => { output += data; },
    onSessionExit: () => {}, onAgentStateChange: () => {},
  });
  const upgraded = new DaemonClient({
    socketDir, daemonEntryPath: newEntry, onSessionData: (_id, data) => { output += data; },
    onSessionExit: () => {}, onAgentStateChange: () => {},
  });
  const spawner = new DaemonSpawner({ socketDir, entryPath: oldEntry });
  let daemonPid: number | undefined;
  try {
    await spawner.ensureRunning();
    daemonPid = JSON.parse(readFileSync(spawner.pidPath, "utf8")).pid;
    await client.ensureConnected();
    const before = await client.request<{ pid: number }>("createOrAttach", { sessionId: "upgrade-test", cwd: root, cols: 80, rows: 24 });
    client.disconnect();
    rmSync(oldEntry);
    const mismatch = detectAndHandlePathMismatch({ pidPath: spawner.pidPath, socketPath: spawner.socketPath, tokenPath: spawner.tokenPath, currentEntryPath: newEntry });
    expect(mismatch).toMatchObject({ mismatch: true, killed: false, cleanedUp: false });
    await new DaemonSpawner({ socketDir, entryPath: newEntry }).ensureRunning();
    await upgraded.ensureConnected();
    const after = await upgraded.request<{ pid: number; isNew: boolean }>("createOrAttach", { sessionId: "upgrade-test", cwd: root, cols: 100, rows: 35 });
    expect(after).toMatchObject({ pid: before.pid, isNew: false });
    expect(JSON.parse(readFileSync(spawner.pidPath, "utf8")).pid).toBe(daemonPid);
    const command = process.platform === "win32" ? "echo upgraded-%COMPUTERNAME%\r" : "printf 'upgraded-%s\\n' live\r";
    await upgraded.request("write", { sessionId: "upgrade-test", data: command });
    await vi.waitFor(() => expect(output).toContain(process.platform === "win32" ? `upgraded-${process.env.COMPUTERNAME}` : "upgraded-live"), { timeout: 5000 });
  } finally {
    client.disconnect();
    if (upgraded.isConnected) await upgraded.request("shutdown", { killSessions: true }).catch(() => {});
    upgraded.disconnect();
    if (daemonPid) {
      await vi.waitFor(() => {
        try { process.kill(daemonPid!, 0); throw new Error("verification daemon is still exiting"); }
        catch (error) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; }
      }, { timeout: 5000 }).catch(() => { try { process.kill(daemonPid!, "SIGTERM"); } catch {} });
    }
    rmSync(root, { recursive: true, force: true });
  }
}, 20_000);
