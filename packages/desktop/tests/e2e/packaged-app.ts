import type { Page } from "@playwright/test";
import { existsSync } from "node:fs";
import * as path from "node:path";

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
