import { afterEach, expect, it } from "vitest";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { nativeBinaries } from "../electron/native-binaries";

let directory: string | undefined;
afterEach(() => { if (directory) rmSync(directory, { recursive: true, force: true }); });

it.each(["darwin", "linux", "win32"] as const)("resolves the bundled %s CLI and agent together", platform => {
  directory = mkdtempSync(path.join(tmpdir(), "crab-native-"));
  const agentDirectory = path.join(directory, "agent");
  mkdirSync(agentDirectory);
  const extension = platform === "win32" ? ".exe" : "";
  for (const name of ["crab", "crab-desktop-agent"]) writeFileSync(path.join(agentDirectory, name + extension), "");
  const layout = { packaged: true, appPath: directory, resourcesPath: directory, platform, arch: "x64" };
  expect(nativeBinaries(layout).crab).toBe(path.join(agentDirectory, "crab" + extension));
  rmSync(path.join(agentDirectory, "crab" + extension));
  expect(() => nativeBinaries(layout)).toThrow("Missing Crab v2 runtime");
});
