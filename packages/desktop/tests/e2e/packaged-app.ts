import type { Page } from "@playwright/test";
import { existsSync, readFileSync } from "node:fs";
import * as path from "node:path";
import { createRequire } from "node:module";
import { extractFile } from "@electron/asar";
import type { StorageConnections } from "../../electron/storage-connections";

export function packagedModule<T>(resources: string, name: string): T {
  const file = path.resolve(__dirname, `../../dist-electron/${name}.cjs`);
  if (!readFileSync(file).equals(extractFile(path.join(resources, "app.asar"), `dist-electron/${name}.cjs`))) throw new Error(`Packaged module differs from build output: ${name}`);
  return createRequire(__filename)(file) as T;
}

export function unboundStorage(resources: string, file: string, changed: (keys: string[], state: "changed" | "removed") => Promise<void>): StorageConnections {
  const { StorageConnections: Connections } = packagedModule<{ StorageConnections: typeof StorageConnections }>(resources, "storage-connections");
  // Staged transport fixtures have no saved credentials. Use the real vault's
  // locked-keyring contract; encrypted persistence has separate GUI proof.
  return new Connections(file, {
    isEncryptionAvailable: () => false, getSelectedStorageBackend: () => "basic_text",
    encryptString: () => { throw new Error("Fixture keyring is unavailable"); },
    decryptString: () => { throw new Error("Fixture keyring is unavailable"); },
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

export async function call(page: Page, method: string, params: unknown): Promise<Record<string, unknown>> {
  return page.evaluate(async ({ method, params }) => {
    const bridge = (window as unknown as { crab: { rpc: (method: string, params: unknown) => Promise<Record<string, unknown>> } }).crab;
    return bridge.rpc(method, params);
  }, { method, params });
}
