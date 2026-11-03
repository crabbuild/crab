import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import verifyNative from "./verify-native.cjs";

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
