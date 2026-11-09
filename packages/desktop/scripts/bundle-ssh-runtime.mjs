import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { link, lstat, mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";

async function digest(file) {
  const stat = await lstat(file);
  if (!stat.isFile() || stat.size === 0) throw new Error(`Expected a nonempty runtime file: ${file}`);
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return { sha256: hash.digest("hex"), bytes: stat.size };
}

export async function bundleSshRuntime(directory, output) {
  const runtime = JSON.parse(await readFile(path.join(directory, "runtime.json"), "utf8"));
  if (!["darwin", "linux"].includes(runtime.platform) || !["arm64", "x64"].includes(runtime.arch) ||
      typeof runtime.version !== "string" || !runtime.version) {
    throw new Error("SSH runtime requires a versioned native Linux or macOS build");
  }
  const hashes = {};
  for (const name of ["crab", "crab-desktop-agent"]) {
    const actual = await digest(path.join(directory, name));
    if (actual.sha256 !== runtime.hashes?.[name]) throw new Error(`SSH runtime checksum mismatch: ${name}`);
    hashes[name] = actual.sha256;
  }
  await mkdir(output, { recursive: true });
  const temporary = await mkdtemp(path.join(output, ".staging-"));
  const target = `${runtime.platform}-${runtime.arch}`;
  const archive = path.join(temporary, `${target}.tar.gz`);
  try {
    // Hard links avoid copying the large native pair into another staging tree.
    // Only these files enter the remote runtime; the desktop NFS helper stays local.
    for (const name of Object.keys(hashes)) await link(path.join(directory, name), path.join(temporary, name));
    const payload = { version: runtime.version, platform: runtime.platform, arch: runtime.arch, hashes };
    await writeFile(path.join(temporary, "runtime.json"), JSON.stringify(payload) + "\n");
    await new Promise((resolve, reject) => {
      // GNU tar treats a colon in an archive name as a remote host separator.
      // A relative name keeps Windows drive letters out of that parser.
      const child = spawn("tar", ["--format=ustar", "-czf", path.basename(archive), "crab", "crab-desktop-agent", "runtime.json"], {
        cwd: temporary, stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, COPYFILE_DISABLE: "1" },
      });
      let stderr = "";
      child.stderr.on("data", data => { stderr = (stderr + data.toString()).slice(-8192); });
      child.once("error", reject);
      child.once("close", code => code === 0 ? resolve() : reject(new Error(`SSH runtime archive failed (${code}): ${stderr}`)));
    });
    const descriptor = { ...payload, archive: await digest(archive) };
    const metadata = path.join(temporary, `${target}.json`);
    await writeFile(metadata, JSON.stringify(descriptor, null, 2) + "\n");
    await rename(archive, path.join(output, `${target}.tar.gz`));
    await rename(metadata, path.join(output, `${target}.json`));
    return descriptor;
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
