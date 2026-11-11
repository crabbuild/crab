import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { createReadStream } from "node:fs";
import { copyFile, lstat, mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-(alpha|beta|rc)\.(0|[1-9]\d*))?$/;
const targets = new Set(["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64"]);

/** Checks the linked GNU runtime contract before publishing Linux binaries. */
export function verifyLinuxAbi(file) {
  const symbols = execFileSync("readelf", ["--version-info", file], { encoding: "utf8", env: { ...process.env, LC_ALL: "C" } });
  const limits = { GLIBC: [2, 28], GLIBCXX: [3, 4, 25], CXXABI: [1, 3, 11] };
  const required = [...symbols.matchAll(/Name: (GLIBC|GLIBCXX|CXXABI)_([\d.]+)/g)];
  if (!required.some(([, name]) => name === "GLIBC")) throw new Error(`Linux binary has no GNU libc version contract: ${file}`);
  const unsupported = required.filter(([, name, value]) => {
    const version = value.split(".").map(Number); const limit = limits[name];
    const different = version.findIndex((part, index) => part !== (limit[index] ?? 0));
    return different >= 0 && version[different] > (limit[different] ?? 0);
  }).map(([, name, value]) => `${name}_${value}`);
  if (unsupported.length) throw new Error(`${file} exceeds the Linux glibc 2.28/GCC 8 ABI: ${[...new Set(unsupported)].join(", ")}`);
  const libraries = execFileSync("readelf", ["--dynamic", file], { encoding: "utf8", env: { ...process.env, LC_ALL: "C" } });
  if (/NEEDED.*lib(?:ssl|crypto)\.so/.test(libraries)) throw new Error(`Linux runtime requires external OpenSSL: ${file}`);
}

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
