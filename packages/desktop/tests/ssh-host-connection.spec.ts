import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { generateKeyPairSync } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { connect as connectSocket, type Socket } from "node:net";
import { createServer } from "node:http";
import { chromium, expect as browserExpect, type Browser } from "@playwright/test";
import { createServer as createViteServer } from "vite";
import type { IpcMainInvokeEvent } from "electron";
import { SshProfileManager } from "../electron/ssh-profiles";
import { registerSshIpcHandlers } from "../electron/ssh-ipc";
import { tmpdir } from "node:os";
import path from "node:path";
import { Server, utils, type Connection } from "ssh2";
import { KnownHostsManager, createHostKeyVerifier, hostKeyFingerprint, type HostKeyInspection } from "../electron/ssh-known-hosts";
import { SshSessionManager, type SshProfile } from "../electron/ssh-manager";

function identity() {
  const pem = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ type: "pkcs1", format: "pem" });
  const key = utils.parseKey(pem);
  if (key instanceof Error || Array.isArray(key)) throw new Error("Invalid owned fixture key");
  return { pem, key: key.getPublicSSH(), parsed: key };
}
const first = identity(); const second = identity();
const { ipcHandlers } = vi.hoisted(() => ({ ipcHandlers: new Map<string, (...args: unknown[]) => unknown>() }));
vi.mock("electron", () => ({ ipcMain: { handle: (name: string, handler: (...args: unknown[]) => unknown) => ipcHandlers.set(name, handler) }, BrowserWindow: { getAllWindows: () => [] }, powerMonitor: { on: () => {} } }));
function pending<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}

describe("real SSH host verification", () => {
  let directory: string; let hosts: KnownHostsManager; let keyPath: string;
  const managers: SshSessionManager[] = []; const servers: Server[] = [];
  const connections = new Set<Connection>(); const sockets = new Set<Socket>();
  beforeEach(async () => {
    directory = await mkdtemp(path.join(tmpdir(), "crab-owned-ssh-"));
    hosts = new KnownHostsManager(path.join(directory, "pins.json"), path.join(directory, "user"), path.join(directory, "global"));
    keyPath = path.join(directory, "identity.pem"); await writeFile(keyPath, first.pem, { mode: 0o600 });
  });
  afterEach(async () => {
    for (const manager of managers.splice(0)) for (const connection of manager.getAllConnections()) await manager.disconnect(connection.profile.id);
    for (const socket of sockets) socket.destroy(); sockets.clear();
    for (const connection of connections) connection.end(); connections.clear();
    await Promise.all(servers.splice(0).map(server => new Promise<void>(resolve => server.close(() => resolve()))));
    ipcHandlers.clear();
    await rm(directory, { recursive: true, force: true });
  });
  async function server(key = first.pem, forwarding = false, commandMode: "ok" | "hang" | "fail" = "ok") {
    let authentications = 0; let commands = 0; let closed = 0;
    const instance = new Server({ hostKeys: [key] }, connection => {
      connections.add(connection); connection.on("error", () => {}); connection.once("close", () => { connections.delete(connection); closed += 1; });
      connection.on("authentication", context => {
        authentications += 1;
        if (context.method === "none" && forwarding) context.accept();
        else if (context.method === "publickey" && context.key.data.equals(first.key) && (!context.signature || (context.blob && first.parsed.verify(context.blob, context.signature, context.hashAlgo)))) context.accept();
        else context.reject();
      });
      connection.on("ready", () => {
        connection.on("session", accept => accept().on("exec", (acceptCommand, reject, info) => {
          if (info.command !== "echo ok") { reject(); return; }
          commands += 1; const channel = acceptCommand();
          if (commandMode === "hang") return;
          channel.write("ok\n"); channel.exit(commandMode === "fail" ? 1 : 0); channel.end();
        }));
        if (forwarding) connection.on("tcpip", (accept, reject, info) => {
          // This owned server forwards only to owned loopback fixtures.
          if (info.destIP !== "127.0.0.1") { reject(); return; }
          const socket = connectSocket({ host: info.destIP, port: info.destPort }); sockets.add(socket);
          socket.once("close", () => sockets.delete(socket)); socket.once("error", () => reject());
          socket.once("connect", () => { const channel = accept(); channel.on("error", () => socket.destroy()); channel.once("close", () => socket.destroy()); socket.pipe(channel).pipe(socket); });
        });
      });
    });
    servers.push(instance);
    await new Promise<void>((resolve, reject) => { instance.once("error", reject); instance.listen(0, "127.0.0.1", resolve); });
    const address = instance.address();
    if (!address || typeof address === "string") throw new Error("Owned server did not bind");
    return { port: address.port, authentications: () => authentications, commands: () => commands, closed: () => closed, commandMode: (mode: "ok" | "hang" | "fail") => { commandMode = mode; } };
  }
  function profile(port: number, jumpHost?: string): SshProfile {
    return { id: "fixture", label: "Owned fixture", host: "127.0.0.1", port, username: "fixture", authMethod: { type: "key", keyPath }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: false, autoDeployAgent: false, tags: [], portForwards: [], createdAt: Date.now(), jumpHost };
  }
  function manager(confirm: (request: HostKeyInspection, signal: AbortSignal) => Promise<boolean>) {
    const owner = new SshSessionManager(createHostKeyVerifier(hosts, confirm)); managers.push(owner); return owner;
  }

  it("blocks authentication until approval is saved, then reuses it for Test Connection", async () => {
    const remote = await server(); const requested = pending<HostKeyInspection>(); const answer = pending<boolean>();
    const owner = manager(async request => { requested.resolve(request); return answer.promise; });
    const connecting = owner.connect(profile(remote.port));
    const request = await requested.promise;
    expect({ status: request.status, port: request.port, authentications: remote.authentications(), pins: hosts.list().length }).toEqual({ status: "unknown", port: remote.port, authentications: 0, pins: 0 });
    answer.resolve(true); await connecting;
    const cached = manager(async () => { throw new Error("Trusted fixture must not prompt"); });
    expect({ status: owner.getStatus("fixture"), trust: hosts.verify("127.0.0.1", remote.port, first.key).status, tested: await cached.testConnection(profile(remote.port)) }).toEqual({ status: "connected", trust: "trusted", tested: { ok: true } });
  });
  it("rejects a first-use key before any authentication", async () => {
    const remote = await server(); const owner = manager(async () => false);
    await expect(owner.connect(profile(remote.port))).rejects.toMatchObject({ code: "SSH_HOST_KEY_REJECTED" });
    expect({ authentications: remote.authentications(), status: owner.getStatus("fixture"), pins: hosts.list().length }).toEqual({ authentications: 0, status: "error", pins: 0 });
  });
  it("presents a changed key with its previous fingerprint", async () => {
    const remote = await server(second.pem); hosts.accept("127.0.0.1", remote.port, first.key); let request: HostKeyInspection | undefined;
    const owner = manager(async presented => { request = presented; return false; });
    await expect(owner.connect(profile(remote.port))).rejects.toMatchObject({ code: "SSH_HOST_KEY_REJECTED" });
    expect({ status: request?.status, previous: request?.previousFingerprints, authentications: remote.authentications() }).toEqual({ status: "changed", previous: [hostKeyFingerprint(first.key)], authentications: 0 });
  });
  it("refuses a revoked key without presenting an approval prompt", async () => {
    const remote = await server(); let prompts = 0;
    await writeFile(path.join(directory, "global"), `@revoked [127.0.0.1]:${remote.port} ssh-rsa ${first.key.toString("base64")}\n`);
    const owner = manager(async () => { prompts += 1; return true; });
    expect({ result: await owner.testConnection(profile(remote.port)), prompts, authentications: remote.authentications() }).toEqual({ result: { ok: false, error: "The SSH host key is revoked and cannot be trusted" }, prompts: 0, authentications: 0 });
  });
  it("verifies each jump hop and the final tunneled endpoint", async () => {
    const jump1 = await server(first.pem, true); const jump2 = await server(second.pem, true); const remote = await server(); const approved: number[] = [];
    const owner = manager(async request => { approved.push(request.port); return true; });
    const result = await owner.testConnection(profile(remote.port, `127.0.0.1:${jump1.port},127.0.0.1:${jump2.port}`));
    expect({ result, approved, pins: hosts.list().map(pin => pin.port) }).toEqual({ result: { ok: true }, approved: [jump1.port, jump2.port, remote.port], pins: [jump1.port, jump2.port, remote.port] });
  });
  it("fails a remote command with a nonzero status even when it prints ok", async () => {
    const remote = await server(first.pem, false, "fail"); const owner = manager(async () => true);
    expect(await owner.testConnection(profile(remote.port))).toEqual({ ok: false, error: "Remote connection test exited with status 1" });
  });

  it.skipIf(!process.env.CRAB_AGENT_LIVE_ROOT)("shows command timeout, closes SSH, and recovers on Retry in the real form", async () => {
    const remote = await server(first.pem, false, "hang"); const owner = manager(async () => true);
    registerSshIpcHandlers(new SshProfileManager(path.join(directory, "profiles.json")), owner, hosts);
    const handler = ipcHandlers.get("ssh:test-connection");
    if (!handler) throw new Error("Connection test IPC not registered");
    const operations = new Set<Promise<void>>();
    const bridge = createServer((request, response) => {
      const operation = (async () => {
        response.setHeader("Access-Control-Allow-Origin", "*"); response.setHeader("Access-Control-Allow-Headers", "Content-Type");
        if (request.method === "OPTIONS") { response.end(); return; }
        if (request.url === "/profile") { response.end(JSON.stringify(profile(remote.port))); return; }
        const chunks: Buffer[] = []; for await (const chunk of request) chunks.push(Buffer.from(chunk));
        const result = await handler({} as IpcMainInvokeEvent, JSON.parse(Buffer.concat(chunks).toString()));
        response.end(JSON.stringify(result));
      })().catch((reason: unknown) => { response.end(JSON.stringify({ ok: false, error: String(reason) })); });
      operations.add(operation); void operation.finally(() => operations.delete(operation));
    });
    const vite = await createViteServer({ cacheDir: `${directory}-vite-cache`, server: { host: "127.0.0.1", port: 0, strictPort: false }, clearScreen: false });
    let browser: Browser | undefined;
    try {
      browser = await chromium.launch({ channel: "chrome" });
      await new Promise<void>(resolve => bridge.listen(0, "127.0.0.1", resolve)); await vite.listen();
      const api = bridge.address(); const ui = vite.httpServer?.address();
      if (!api || typeof api === "string" || !ui || typeof ui === "string") throw new Error("Fixture bridge unavailable");
      const page = await browser.newPage();
      page.on("pageerror", error => { console.error("Connection fixture renderer:", error.message); });
      await page.goto(`http://127.0.0.1:${ui.port}/tests/fixtures/ssh-connection-form.html?api=${encodeURIComponent(`http://127.0.0.1:${api.port}`)}`);
      const test = page.getByRole("button", { name: "Test SSH connection", exact: true });
      await test.click({ timeout: 10_000 }); await browserExpect(page.getByRole("alert")).toHaveText("Connection timed out", { timeout: 10_000 });
      await vi.waitFor(() => expect(remote.closed()).toBe(1));
      await page.screenshot({ path: path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "ssh-test-command-timeout.png") });
      await browserExpect(test).toBeEnabled(); remote.commandMode("ok");
      await test.click(); await browserExpect(page.getByRole("alert")).toContainText("Connection successful");
      await vi.waitFor(() => expect(remote.closed()).toBe(2)); expect(remote.commands()).toBe(2);
      await page.screenshot({ path: path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "ssh-test-command-recovery.png") });
    } finally {
      await browser?.close(); await vite.close(); bridge.closeAllConnections();
      await new Promise<void>(resolve => bridge.close(() => resolve())); await Promise.allSettled([...operations]);
      await rm(`${directory}-vite-cache`, { recursive: true, force: true });
    }
  }, 30_000);
});
