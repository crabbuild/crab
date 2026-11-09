const { readFileSync, readdirSync } = require("node:fs");
const { createHash } = require("node:crypto");
const path = require("node:path");
const { Arch } = require("builder-util");

module.exports = async context => {
  const arch = Arch[context.arch];
  const platform = context.electronPlatformName;
  const os = { darwin: "mac", win32: "win", linux: "linux" }[platform];
  const directory = path.join(context.packager.projectDir, "resources", "native", `${os}-${arch}`);
  const runtime = JSON.parse(readFileSync(path.join(directory, "runtime.json"), "utf8"));
  const { version } = context.packager.appInfo;
  if (runtime.version !== version || runtime.platform !== platform || runtime.arch !== arch) {
    throw new Error("Desktop runtime does not match the package version, OS and architecture. Run build:agent on the target runner.");
  }
  const { readSshRuntime, SSH_RUNTIME_TARGETS } = require("../dist-electron/ssh-deployer.cjs");
  const remote = path.join(context.packager.projectDir, "resources", "native", "ssh-runtimes");
  const names = readdirSync(remote).filter(name => name.endsWith(".json") || name.endsWith(".tar.gz"));
  const targets = names.filter(name => name.endsWith(".json")).map(name => name.slice(0, -5));
  if (!targets.length || names.length !== targets.length * 2) throw new Error("Incomplete SSH runtime archive catalog");
  if (process.env.SIGN_RELEASE === "true" && SSH_RUNTIME_TARGETS.some(target => !targets.includes(target))) {
    throw new Error("Release packages require all Linux/macOS SSH runtime architectures");
  }
  await Promise.all(targets.map(target => readSshRuntime(remote, target, version)));
  const extension = platform === "win32" ? ".exe" : "";
  for (const name of ["crab", "crab-nfs-mount", "crab-desktop-agent", "git-remote-crab"]) {
    const file = `${name}${extension}`;
    const expected = runtime.hashes[name === "git-remote-crab" ? `crab${extension}` : file];
    const actual = createHash("sha256").update(readFileSync(path.join(directory, file))).digest("hex");
    if (!expected || actual !== expected) throw new Error(`Desktop runtime checksum mismatch: ${file}`);
  }
};
