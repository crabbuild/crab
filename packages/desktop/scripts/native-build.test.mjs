import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import verifyNative from "./verify-native.cjs";
import configureUpdater from "./configure-updater.cjs";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

const require = createRequire(import.meta.url);
const { Platform } = require("app-builder-lib");

for (const [platform, os, arch, index] of [["darwin", "mac", "arm64", 3], ["linux", "linux", "x64", 1], ["win32", "win", "x64", 1]]) {
  test(`packaging validates the complete ${platform} runtime`, async t => {
    const root = await mkdtemp(path.join(tmpdir(), "crab-runtime-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const directory = path.join(root, "resources", "native", `${os}-${arch}`);
    await mkdir(directory, { recursive: true });
    const extension = platform === "win32" ? ".exe" : "";
    const hashes = {};
    for (const name of ["crab", "crab-desktop-agent", "crab-nfs-mount"]) {
      const bytes = Buffer.from(`${name} ${platform} ${arch}`);
      await writeFile(path.join(directory, `${name}${extension}`), bytes);
      hashes[`${name}${extension}`] = createHash("sha256").update(bytes).digest("hex");
    }
    await writeFile(path.join(directory, `git-remote-crab${extension}`), `crab ${platform} ${arch}`);
    await writeFile(path.join(directory, "runtime.json"), JSON.stringify({ version: "2.0.0", platform, arch, hashes }));
    const context = { arch: index, electronPlatformName: platform, packager: { projectDir: root, appInfo: { version: "2.0.0" } } };
    await verifyNative(context);
    await writeFile(path.join(directory, `crab${extension}`), "stale CLI");
    await assert.rejects(verifyNative(context), /checksum mismatch/);
  });
}

for (const platform of [Platform.MAC, Platform.WINDOWS, Platform.LINUX]) {
  for (const channel of ["latest", "alpha"]) {
    test(`${platform.name} directory package retains ${channel} update configuration`, async t => {
      const root = await mkdtemp(path.join(tmpdir(), "crab-update-config-"));
      t.after(() => rm(root, { recursive: true, force: true }));
      const config = parse(await readFile(new URL("../electron-builder.yml", import.meta.url), "utf8"));
      assert.equal(config.afterPack, "scripts/configure-updater.cjs");
      const publish = { ...config.publish, channel };
      const packager = {
        config: { ...config, publish }, platform, platformSpecificBuildOptions: {},
        info: { config }, appInfo: { updaterCacheDirName: "crab-desktop-updater" },
        expandMacro: value => value, getResourcesDir: () => root,
        isForceCodeSigningVerification: true,
        signingManager: { value: Promise.resolve({ computedPublisherName: { value: Promise.resolve(["Crab"]) } }) },
      };
      await configureUpdater({ arch: 1, appOutDir: root, packager });
      const actual = parse(await readFile(path.join(root, "app-update.yml"), "utf8"));
      assert.deepEqual(actual, {
        ...publish, updaterCacheDirName: "crab-desktop-updater",
        ...(platform === Platform.WINDOWS ? { publisherName: ["Crab"] } : {}),
      });
    });
  }
}

test("plain Node SSH sessions do not initialize Electron power APIs", () => {
  const manager = fileURLToPath(new URL("../dist-electron/ssh-manager.cjs", import.meta.url));
  const electron = require.resolve("electron");
  // Electron's Node entry can synchronously install its binary. Observe the
  // real compiled constructor without making this regression download it.
  execFileSync(process.execPath, ["-e", `
    const assert = require("node:assert/strict");
    let reads = 0;
    const exports = Object.defineProperty({}, "powerMonitor", { get() {
      reads += 1;
      throw new Error("Electron API unavailable in plain Node");
    } });
    require.cache[process.argv[2]] = { exports, loaded: true };
    const { SshSessionManager } = require(process.argv[1]);
    new SshSessionManager(async () => {});
    assert.equal(reads, 0);
  `, manager, electron], { env: { ...process.env, ELECTRON_RUN_AS_NODE: "1" } });
});
