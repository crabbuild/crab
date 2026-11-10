import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { copyFile, lstat, mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-(alpha|beta|rc)\.(0|[1-9]\d*))?$/;
const targets = new Set(["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64"]);

export async function stageRemoteRuntime(directory, output) {
  const runtime = JSON.parse(await readFile(path.join(directory, "runtime.json"), "utf8"));
  if (!versionPattern.test(runtime.version) || !targets.has(`${runtime.platform}-${runtime.arch}`)) throw new Error("Unsupported remote runtime identity");
  const prefix = `Crab-${runtime.version}-runtime-${runtime.platform}-${runtime.arch}`;
  const hashes = {};
  for (const name of ["crab", "crab-desktop-agent"]) {
    const file = path.join(directory, name); const stat = await lstat(file);
    if (!stat.isFile() || stat.size === 0 || stat.size > 512 * 1024 * 1024) throw new Error(`Invalid remote runtime binary: ${name}`);
    const hash = createHash("sha256");
    for await (const chunk of createReadStream(file)) hash.update(chunk);
    hashes[name] = hash.digest("hex");
    if (hashes[name] !== runtime.hashes?.[name]) throw new Error(`Remote runtime checksum mismatch: ${name}`);
  }
  await mkdir(output, { recursive: true });
  for (const name of Object.keys(hashes)) await copyFile(path.join(directory, name), path.join(output, `${prefix}-${name}`));
  await writeFile(path.join(output, `${prefix}.json`), JSON.stringify({ version: runtime.version, platform: runtime.platform, arch: runtime.arch, hashes }, null, 2) + "\n");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [directory, output] = process.argv.slice(2);
  if (!directory || !output) throw new Error("Usage: node scripts/remote-runtime.mjs <native-runtime-dir> <output-dir>");
  await stageRemoteRuntime(directory, output);
}
