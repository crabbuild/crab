import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { stageRemoteRuntime } from "./remote-runtime.mjs";

for (const [platform, arch] of [["linux", "x64"], ["linux", "arm64"], ["darwin", "x64"], ["darwin", "arm64"]]) {
  test(`remote release contains the exact ${platform}/${arch} CLI and sidecar`, async t => {
    const root = await mkdtemp(path.join(tmpdir(), "crab-runtime-release-")); t.after(() => rm(root, { recursive: true, force: true }));
    const source = path.join(root, "source"); const output = path.join(root, "release"); await mkdir(source);
    const hashes = {};
    for (const name of ["crab", "crab-desktop-agent"]) {
      const data = Buffer.from(`${platform}/${arch}/${name}`); await writeFile(path.join(source, name), data); hashes[name] = createHash("sha256").update(data).digest("hex");
    }
    const runtime = { version: "2.0.0-alpha.1", platform, arch, hashes };
    await writeFile(path.join(source, "runtime.json"), JSON.stringify(runtime)); await stageRemoteRuntime(source, output);
    const prefix = `Crab-${runtime.version}-runtime-${platform}-${arch}`;
    const manifest = JSON.parse(await readFile(path.join(output, `${prefix}.json`), "utf8"));
    for (const name of ["crab", "crab-desktop-agent"]) assert.equal(createHash("sha256").update(await readFile(path.join(output, `${prefix}-${name}`))).digest("hex"), manifest.hashes[name]);
    await writeFile(path.join(source, "crab"), "corrupted CLI"); await assert.rejects(stageRemoteRuntime(source, output), /checksum mismatch/);
  });
}
