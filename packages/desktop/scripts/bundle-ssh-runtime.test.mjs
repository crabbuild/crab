import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { bundleSshRuntime } from "./bundle-ssh-runtime.mjs";

test("SSH archives carry the verified native pair and their own metadata", async t => {
  const root = await mkdtemp(path.join(tmpdir(), "crab-ssh-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const input = path.join(root, "native"), output = path.join(root, "archives"), extracted = path.join(root, "extracted");
  await mkdir(input); await mkdir(extracted);
  const files = { crab: Buffer.from("matching CLI"), "crab-desktop-agent": Buffer.from("matching sidecar") };
  const hashes = {};
  for (const [name, bytes] of Object.entries(files)) {
    await writeFile(path.join(input, name), bytes, { mode: 0o755 });
    hashes[name] = createHash("sha256").update(bytes).digest("hex");
  }
  const runtime = { version: "2.0.0-alpha.1", platform: "linux", arch: "arm64", hashes };
  await writeFile(path.join(input, "runtime.json"), JSON.stringify(runtime));
  await writeFile(path.join(input, "crab-nfs-mount"), "local helper");
  const descriptor = await bundleSshRuntime(input, output);
  const archive = path.join(output, "linux-arm64.tar.gz");
  const archived = await readFile(archive);
  assert.deepEqual(descriptor.archive, { sha256: createHash("sha256").update(archived).digest("hex"), bytes: archived.length });
  execFileSync("tar", ["-xzf", path.relative(extracted, archive).split(path.sep).join("/")], { cwd: extracted });
  assert.deepEqual((await readdir(extracted)).sort(), ["crab", "crab-desktop-agent", "runtime.json"]);
  assert.deepEqual(JSON.parse(await readFile(path.join(extracted, "runtime.json"), "utf8")), runtime);
  for (const [name, bytes] of Object.entries(files)) assert.deepEqual(await readFile(path.join(extracted, name)), bytes);
  await writeFile(path.join(input, "crab"), "tampered CLI");
  await assert.rejects(bundleSshRuntime(input, output), /SSH runtime checksum mismatch: crab/);
  assert.deepEqual((await readdir(output)).sort(), ["linux-arm64.json", "linux-arm64.tar.gz"]);
  assert.deepEqual(await readFile(archive), archived);
});
