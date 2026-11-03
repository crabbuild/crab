import * as fs from "node:fs";
import * as path from "node:path";

interface NativeLayout {
  packaged: boolean;
  appPath: string;
  resourcesPath: string;
  platform: NodeJS.Platform;
  arch: string;
}

export function nativeBinaries(layout: NativeLayout): { agent: string; crab: string } {
  const directory = layout.packaged
    ? path.join(layout.resourcesPath, "agent")
    : path.join(layout.appPath, "resources", "native", `${layout.platform === "win32" ? "win" : layout.platform === "darwin" ? "mac" : layout.platform}-${layout.arch}`);
  const extension = layout.platform === "win32" ? ".exe" : "";
  const agent = path.join(directory, `crab-desktop-agent${extension}`);
  const crab = path.join(directory, `crab${extension}`);
  for (const binary of [agent, crab]) {
    if (!fs.existsSync(binary)) {
      throw new Error(`Missing Crab v2 runtime: ${binary}. Run 'npm run build:agent'.`);
    }
  }
  return { agent, crab };
}
