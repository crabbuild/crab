import { spawn, execFileSync } from "node:child_process";
import { access, copyFile, mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { createHash } from "node:crypto";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { stageRemoteRuntime } from "./remote-runtime.mjs";

const desktop = fileURLToPath(new URL("../", import.meta.url));
const root = path.resolve(desktop, "../..");
const platform = { darwin: "mac", win32: "win", linux: "linux" }[process.platform];
if (!platform) throw new Error(`Unsupported desktop platform: ${process.platform}`);

const configured = process.env.CARGO_TARGET_DIR;
if (!configured || !path.isAbsolute(configured)) {
  throw new Error("Set CARGO_TARGET_DIR to an absolute, per-checkout directory on the Workspace build volume.");
}
if (process.env.CARGO_BUILD_TARGET) {
  throw new Error("Build desktop artifacts on a native runner; unset CARGO_BUILD_TARGET.");
}
if (process.platform === "darwin" && process.env.GITHUB_ACTIONS !== "true") {
  const base = await realpath(path.join(homedir(), "Workspace", "crabbuild-target"));
  const parent = await realpath(path.dirname(configured));
  if (parent !== base) throw new Error("CARGO_TARGET_DIR must be directly beneath Workspace/crabbuild-target.");
}
await mkdir(configured, { recursive: true });
await access(configured, constants.W_OK);
const target = await realpath(configured);
if (target === root || target.startsWith(`${root}${path.sep}`)) {
  throw new Error("Cargo build artifacts must be outside the checkout.");
}

// Match the release CLI feature set without requiring a FUSE driver at app startup.
const features = ["simd-accel", "tier", "replication-s3-control-plane", "replication-gcs-control-plane", "replication-azure-control-plane", "coordinator-dynamodb", "coordinator-spanner", "coordinator-cosmosdb", "watch", "nfs", "gix-pathmatch"];
const args = ["build", "--release", "--locked", "--no-default-features", "-p", "crab", "--bin", "crab", "--bin", "crab-nfs-mount", "-p", "crab-desktop-agent", "--bin", "crab-desktop-agent", "--features", features.map(feature => `crab/${feature}`).join(",")];
await new Promise((resolve, reject) => {
  const child = spawn("cargo", args, { cwd: root, stdio: "inherit" });
  child.once("error", reject);
  child.once("exit", code => code === 0 ? resolve() : reject(new Error(`Cargo exited with status ${code}`)));
});

const destination = path.join(desktop, "resources", "native", `${platform}-${process.arch}`);
await rm(destination, { recursive: true, force: true });
await mkdir(destination, { recursive: true });
const extension = process.platform === "win32" ? ".exe" : "";
const binaries = ["crab", "crab-nfs-mount", "crab-desktop-agent"];
const hashes = {};
for (const name of binaries) {
  const filename = `${name}${extension}`;
  const source = path.join(target, "release", filename);
  await copyFile(source, path.join(destination, filename));
  hashes[filename] = createHash("sha256").update(await readFile(source)).digest("hex");
}
// Git discovers its remote helper by executable name, including on Windows.
await copyFile(path.join(destination, `crab${extension}`), path.join(destination, `git-remote-crab${extension}`));
const { version } = JSON.parse(await readFile(path.join(desktop, "package.json"), "utf8"));
for (const [name, prefix] of [["crab", "crab"], ["crab-desktop-agent", "crab-desktop-agent"]]) {
  const actual = execFileSync(path.join(destination, name + extension), ["--version"], { encoding: "utf8" }).trim();
  if (actual !== `${prefix} ${version}`) throw new Error(`Desktop ${version} cannot bundle ${actual}`);
}
await writeFile(path.join(destination, "runtime.json"), JSON.stringify({ version, platform: process.platform, arch: process.arch, hashes }, null, 2) + "\n");
if (process.platform !== "win32") await stageRemoteRuntime(destination, path.join(desktop, "release", "remote-runtime"));
console.log(`Crab ${version} desktop runtime staged in ${destination}`);
