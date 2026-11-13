import { expect, type ElectronApplication, type Page } from "@playwright/test";
import { createReadStream, existsSync, readFileSync, realpathSync, rmSync } from "node:fs";
import * as path from "node:path";
import { createRequire } from "node:module";
import { Readable } from "node:stream";
import { extractFile } from "@electron/asar";
import { DaemonClient } from "../../electron/daemon/daemon-client";
import { PROTOCOL_VERSION } from "../../electron/daemon/protocol";
import { socketAddress } from "../../electron/daemon/socket-address";
import type { StorageConnections } from "../../electron/storage-connections";
import type { RemoteRuntimeStore } from "../../electron/ssh-runtime";

export function packagedModule<T>(resources: string, name: string): T {
  const file = path.resolve(__dirname, `../../dist-electron/${name}.cjs`);
  if (!readFileSync(file).equals(extractFile(path.join(resources, "app.asar"), `dist-electron/${name}.cjs`))) throw new Error(`Packaged module differs from build output: ${name}`);
  return createRequire(__filename)(file) as T;
}

export async function prepareFixtureSsh(application: ElectronApplication, fixture: { port: number; publicHostKey: Buffer }): Promise<void> {
  const options = await application.evaluate(({ app }) => ({
    packaged: true, appPath: app.getAppPath(), version: app.getVersion(), cacheDirectory: `${app.getPath("userData")}/remote-runtimes`,
  }));
  const resources = path.dirname(options.appPath);
  const { RemoteRuntimeStore: RuntimeStore } = packagedModule<{ RemoteRuntimeStore: typeof RemoteRuntimeStore }>(resources, "ssh-runtime");
  // Unreleased builds have no public release assets yet. Populate the isolated
  // cache through the actual resolver and this build's hashed producer outputs.
  const runtimes = new RuntimeStore(options, async input => {
    const name = path.basename(new URL(String(input)).pathname);
    return new Response(Readable.toWeb(createReadStream(path.resolve(__dirname, "../../release/remote-runtime", name))) as unknown as ReadableStream<Uint8Array>);
  });
  if (process.platform !== "darwin" && process.platform !== "linux") throw new Error("SSH fixture requires a POSIX runtime");
  if (process.arch !== "arm64" && process.arch !== "x64") throw new Error("Unsupported SSH fixture architecture");
  await runtimes.resolve({ platform: process.platform, arch: process.arch }, new AbortController().signal);
  // Pin only the owned loopback server in this isolated profile. Native prompt
  // decisions and rotation are exercised separately by ssh-host-key.spec.ts.
  await application.evaluate(({ app }, { port, key }) => {
    const modules = process.getBuiltinModule("node:module") as typeof import("node:module");
    const require = modules.createRequire(`${app.getAppPath()}/package.json`);
    const { KnownHostsManager } = require(`${app.getAppPath()}/dist-electron/ssh-known-hosts.cjs`) as typeof import("../../electron/ssh-known-hosts");
    new KnownHostsManager(`${app.getPath("userData")}/ssh-known-hosts.json`).accept("127.0.0.1", port, Buffer.from(key, "base64"));
  }, { port: fixture.port, key: fixture.publicHostKey.toString("base64") });
}

export function unboundStorage(resources: string, file: string, changed: (keys: string[], state: "changed" | "removed") => Promise<void>): StorageConnections {
  const { StorageConnections: Connections } = packagedModule<{ StorageConnections: typeof StorageConnections }>(resources, "storage-connections");
  // Staged transport fixtures have no saved credentials. Use the real vault's
  // locked-keyring contract; encrypted persistence has separate GUI proof.
  return new Connections(file, {
    isAsyncEncryptionAvailable: async () => false, getSelectedStorageBackend: () => "basic_text",
    encryptStringAsync: async () => { throw new Error("Fixture keyring is unavailable"); },
    decryptStringAsync: async () => { throw new Error("Fixture keyring is unavailable"); },
  }, process.platform, changed);
}

export function packagedExecutable(): string {
  const release = path.resolve(__dirname, "../../release");
  const candidates = process.platform === "darwin"
    ? ["mac-arm64/Crab.app/Contents/MacOS/Crab", "mac/Crab.app/Contents/MacOS/Crab"]
    : process.platform === "win32" ? ["win-unpacked/Crab.exe"] : ["linux-unpacked/crab"];
  const executablePath = process.env.CRAB_PACKAGED_APP ?? candidates.map(file => path.join(release, file)).find(existsSync);
  if (!executablePath) throw new Error("Build the unpacked application with npm run package:dir first");
  return executablePath;
}

export function isNativeAgentCommand(command: string): boolean {
  const directory = path.dirname(packagedExecutable());
  const resources = process.platform === "darwin" ? path.resolve(directory, "../Resources") : path.resolve(directory, "resources");
  const executable = realpathSync(path.join(resources, "agent", process.platform === "win32" ? "crab-desktop-agent.exe" : "crab-desktop-agent"));
  // Electron resolves bundle symlinks and launches this actor without arguments.
  // Canonicalize before matching its whole executable; callers also bind its PID.
  return command === executable || command.startsWith(`${executable} `);
}

export async function call(page: Page, method: string, params: unknown): Promise<Record<string, unknown>> {
  return page.evaluate(async ({ method, params }) => {
    const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<Record<string, unknown>> } }).crab;
    try { return await bridge.rpc(method, params); }
    catch (error) {
      const detail = error as { code?: unknown; message?: unknown } | null;
      throw new Error(`${method} failed (${String(detail?.code ?? "unknown")}): ${String(detail?.message ?? error)}`);
    }
  }, { method, params });
}

export async function answerConfirmation(page: Page, message: RegExp, accept = true): Promise<void> {
  await page.getByRole("alertdialog", { name: message }).getByRole("button", { name: accept ? "Confirm" : "Cancel", exact: true }).click();
}

export async function closeFixtureDaemon(home: string): Promise<void> {
  // Only callers that created this HOME own its daemon. Tests inheriting the
  // developer's HOME must leave that durable process running.
  const socketDir = path.join(home, ".crab", "desktop", "daemon");
  const base = path.join(socketDir, `daemon-v${PROTOCOL_VERSION}`);
  const { pid } = JSON.parse(readFileSync(`${base}.pid`, "utf8")) as { pid: number };
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error("Fixture daemon has no process identity");
  const client = new DaemonClient({
    socketDir, daemonEntryPath: "",
    onSessionData: () => {}, onSessionExit: () => {}, onAgentStateChange: () => {},
  });
  try {
    expect(await client.request("shutdown", { killSessions: true })).toEqual({ acknowledged: true });
  } finally { client.disconnect(); }
  await expect.poll(() => {
    try { process.kill(pid, 0); return false; }
    catch (error: unknown) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
      return true;
    }
  }, { timeout: 10_000 }).toBe(true);
  const address = socketAddress(`${base}.sock`);
  if (process.platform !== "win32" && address !== `${base}.sock`) {
    rmSync(path.dirname(address), { recursive: true });
  }
}
