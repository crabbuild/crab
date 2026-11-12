import { spawn, execFileSync } from "node:child_process";
import { access, copyFile, mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { createHash } from "node:crypto";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { stageRemoteRuntime, verifyLinuxAbi } from "./remote-runtime.mjs";

const desktop = fileURLToPath(new URL("../", import.meta.url));
const root = path.resolve(desktop, "../..");
const platform = { darwin: "mac", win32: "win", linux: "linux" }[process.platform];
if (!platform) throw new Error(`Unsupported desktop platform: ${process.platform}`);
const testing = process.argv[2] === "--test";
if (process.argv.length > (testing ? 3 : 2)) throw new Error("Usage: node scripts/build-native.mjs [--test]");

async function run(binary, args, env = process.env) {
  await new Promise((resolve, reject) => {
    const child = spawn(binary, args, { cwd: root, env, stdio: "inherit" });
    child.once("error", reject);
    child.once("exit", code => code === 0 ? resolve() : reject(new Error(`${binary} exited with status ${code}`)));
  });
}

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

if (process.platform === "linux") {
  const glibc = process.report.getReport().header.glibcVersionRuntime?.split(".").map(Number);
  const baselineLibc = glibc?.[0] === 2 && glibc[1] === 28;
  let compilerMajor = 0;
  if (baselineLibc) {
    try { compilerMajor = Number(execFileSync("c++", ["-dumpversion"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim().split(".")[0]); } catch {}
  }
  if (!baselineLibc || !(compilerMajor >= 14)) {
    // Build and test every Linux native binary against the oldest supported
    // GNU ABI. Keep Cargo caches/artifacts in this checkout's external target.
    const dockerfile = path.join(desktop, "scripts", "linux-native.Dockerfile");
    const digest = createHash("sha256").update(await readFile(dockerfile)).digest("hex").slice(0, 20);
    const image = `crab-linux-native:${digest}`;
    const architecture = `linux/${process.arch === "arm64" ? "arm64" : "amd64"}`;
    const cache = path.join(target, "cargo-home");
    await mkdir(cache, { recursive: true });
    await run("docker", ["build", "--pull", "--platform", architecture, "--file", dockerfile, "--tag", image, path.dirname(dockerfile)]);
    const command = ["run", "--rm", "--init", "--platform", architecture, "--user", `${process.getuid()}:${process.getgid()}`,
      "--volume", `${root}:${root}`, "--volume", `${target}:${target}`, "--volume", `${cache}:/cargo-home`,
      "--env", `CARGO_TARGET_DIR=${target}`, "--env", "CARGO_HOME=/cargo-home",
      "--env", "CARGO_INCREMENTAL", "--env", "CARGO_BUILD_JOBS", "--env", "GIT_SHA",
      image, "node", fileURLToPath(import.meta.url), ...(testing ? ["--test"] : [])];
    await run("docker", command);
    process.exit(0);
  }
}

const compiler = process.platform === "linux" && process.arch === "arm64"
  ? { CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER: "cc", CC_aarch64_unknown_linux_gnu: "cc" } : {};
const environment = { ...process.env, ...compiler };
if (testing) {
  const extension = process.platform === "win32" ? ".exe" : "";
  const env = { ...environment, CRAB_BIN: path.join(desktop, "resources", "native", `${platform}-${process.arch}`, `crab${extension}`) };
  // Linux target/linker configuration belongs to this producer environment;
  // host Cargo commands can neither reuse its toolchain nor qualify its output.
  for (const filter of ["namespace_", "catalog::", "private_fs::platform::tests"]) {
    await run("cargo", ["test", "--release", "--locked", "-p", "crab-cache", "--lib", "--features", "local-cache,xet-chunk-cache", filter], env);
  }
  await run("cargo", ["test", "--release", "-p", "crab-workflow", "--locked", "--lib"], env);
  await run("cargo", ["test", "--release", "-p", "crab", "--locked", "--no-default-features", "--lib", "cmd::run::tests::sole_workflow_file_keeps_discovery_target_identity"], env);
  await run("cargo", ["test", "--release", "-p", "crab", "--locked", "--no-default-features", "--lib", "cmd::run::tests::local_execution_does_not_resolve_the_repository_remote"], env);
  await run("cargo", ["test", "--release", "-p", "crab-desktop-agent", "--locked"], env);
  for (const filter of ["native_crab_and_lfs_pointer_previews_use_staging_without_hydrating", "native_crab_comparison_", "materialization_", "restoration_", "native_cleanup_reports_usage_and_preserves_busy_and_retained_state"]) {
    await run("cargo", ["test", "--release", "-p", "crab-desktop-agent", "--locked", filter, "--", "--ignored", "--test-threads=1"], env);
  }
  await run("cargo", ["test", "--release", "-p", "crab-git", "--locked", "--features", "process", "--lib", "process::tests"], env);
  process.exit(0);
}

// Match the release CLI feature set without requiring a FUSE driver at app startup.
const features = ["simd-accel", "tier", "replication-s3-control-plane", "replication-gcs-control-plane", "replication-azure-control-plane", "coordinator-dynamodb", "coordinator-spanner", "coordinator-cosmosdb", "watch", "nfs", "gix-pathmatch"];
const args = ["build", "--release", "--locked", "--no-default-features", "-p", "crab", "--bin", "crab", "--bin", "crab-nfs-mount", "-p", "crab-desktop-agent", "--bin", "crab-desktop-agent", "--features", features.map(feature => `crab/${feature}`).join(",")];
await run("cargo", args, environment);

const destination = path.join(desktop, "resources", "native", `${platform}-${process.arch}`);
await rm(destination, { recursive: true, force: true });
await mkdir(destination, { recursive: true });
const extension = process.platform === "win32" ? ".exe" : "";
const binaries = ["crab", "crab-nfs-mount", "crab-desktop-agent"];
const hashes = {};
for (const name of binaries) {
  const filename = `${name}${extension}`;
  const source = path.join(target, "release", filename);
  if (process.platform === "linux") verifyLinuxAbi(source);
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
