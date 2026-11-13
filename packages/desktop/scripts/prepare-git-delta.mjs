import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { copyFile, mkdir, mkdtemp, open, readFile, realpath, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";

// Git's fixture builder itself allocates full delta bases. Keep preparation
// outside the measured reader process tree so its RSS cannot hide a regression.
const args = process.argv.slice(2), checkout = args[0] === "--checkout";
if (checkout) args.shift();
assert.equal(args.length, 1, "Usage: prepare-git-delta.mjs [--checkout] <manifest>");
const target = process.env.CARGO_TARGET_DIR;
assert.ok(target && path.isAbsolute(target), "Set an absolute per-checkout CARGO_TARGET_DIR");
const evidence = await mkdtemp(path.join(await realpath(target), "git-range-qualification-"));
const source = path.join(evidence, "source"), execute = promisify(execFile);
await mkdir(source);
await writeFile(path.join(evidence, "gitconfig"), "");
const env = { ...process.env, GIT_CONFIG_GLOBAL: path.join(evidence, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
for (const key of ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"]) delete env[key];
const git = async (args, input = "") => {
  const pending = execute("git", args, { cwd: source, env, maxBuffer: 1024 * 1024 });
  pending.child.stdin.end(input);
  return (await pending).stdout.trim();
};
await git(["init", "--template=", "--object-format=sha1", "--initial-branch=main"]);
const size = 512 * 1024 ** 2, head = Buffer.from(`${checkout ? "$Id$ " : ""}verified-delta-head\n`);
const fillByte = checkout ? 0x78 : 0;
const tails = [Buffer.from("verified-base-tail\n"), Buffer.from("verified-next-tail\n")], oids = [];
for (const [index, tail] of tails.entries()) {
  const file = path.join(source, `body-${index}.bin`), descriptor = await open(file, "wx");
  try {
    if (checkout) {
      const block = Buffer.alloc(1024 * 1024, fillByte);
      for (let offset = 0; offset < size; offset += block.length) {
        let written = 0;
        while (written < block.length) {
          const next = await descriptor.write(block, written, block.length - written, offset + written);
          assert.ok(next.bytesWritten > 0, "Dense fixture write must make progress");
          written += next.bytesWritten;
        }
      }
    } else await descriptor.truncate(size);
    assert.equal((await descriptor.write(head, 0, head.length, 0)).bytesWritten, head.length);
    assert.equal((await descriptor.write(tail, 0, tail.length, size - tail.length)).bytesWritten, tail.length);
  } finally { await descriptor.close(); }
  oids.push(await git(["-c", "core.bigFileThreshold=1g", "-c", "core.compression=1", "hash-object", "--no-filters", "-w", file]));
}
const packs = path.join(source, ".git/objects/pack");
const pack = await git(["-c", "core.bigFileThreshold=1g", "pack-objects", "--threads=1", "--window=10", "--window-memory=0", "--depth=50", "--compression=1", path.join(packs, "pack")], oids.join("\n") + "\n");
const verification = await git(["verify-pack", "-v", path.join(packs, `pack-${pack}.idx`)]);
const delta = verification.split("\n").map(line => line.split(/\s+/)).find(fields => oids.includes(fields[0]) && fields.length === 7);
assert.ok(delta, "Fixture must contain a real delta entry");
assert.equal(delta[1], "blob");
assert.ok(Number(delta[5]) > 0);
assert.equal(Number(await git(["cat-file", "-s", delta[0]])), size);
const file = path.join(source, "large.bin"), index = oids.indexOf(delta[0]);
await copyFile(path.join(source, `body-${index}.bin`), file);
await git(["update-index", "--add", "--cacheinfo", "100644", delta[0], "large.bin"]);
await git(["prune-packed"]);
for (const oid of oids) {
  await assert.rejects(stat(path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2))), error => error.code === "ENOENT");
}
const hash = createHash("sha256");
for await (const bytes of createReadStream(file)) hash.update(bytes);
const manifest = { schema: "crab.desktop.git-delta-fixture", evidenceDirectory: evidence, source, logicalBytes: size,
  oid: delta[0], base: delta[6], depth: Number(delta[5]), deltaProgramBytes: Number(delta[2]),
  head: head.toString("base64"), tail: tails[index].toString("base64"), fillByte, sha256: hash.digest("hex"),
  object: path.join(packs, `pack-${pack}.pack`), packBytes: (await stat(path.join(packs, `pack-${pack}.pack`))).size, verification };
if (checkout) {
  // Git's independent checkout oracle also buffers the full object. Its cost
  // belongs to preparation, outside the bounded desktop process measurement.
  const attributes = "large.bin ident text=auto eol=crlf\n";
  const originalIndex = await readFile(path.join(source, ".git/index"));
  await writeFile(path.join(source, ".gitattributes"), attributes, { flag: "wx" });
  const child = spawn("git", ["--no-replace-objects", "cat-file", "--filters", ":large.bin"],
    { cwd: source, env, stdio: ["ignore", "pipe", "pipe"] });
  let stderr = "", bytes = 0;
  child.stderr.on("data", chunk => { stderr = (stderr + chunk).slice(-65536); });
  const done = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => code === 0 ? resolve() : reject(new Error(`Checkout oracle exited ${code}/${signal}: ${stderr}`)));
  });
  void done.catch(() => {});
  const converted = createHash("sha256");
  for await (const chunk of child.stdout) { converted.update(chunk); bytes += chunk.length; }
  await done;
  assert.deepEqual(await readFile(path.join(source, ".git/index")), originalIndex);
  assert.ok(bytes > size, "Checkout oracle must expand ident and line endings");
  manifest.checkout = { attributes, bytes, sha256: converted.digest("hex"), oracle: await git(["--version"]) };
  assert.notEqual(manifest.checkout.sha256, manifest.sha256);
}
const destination = path.resolve(args[0]);
await mkdir(path.dirname(destination), { recursive: true });
await writeFile(destination, JSON.stringify(manifest, null, 2) + "\n", { flag: "wx" });
console.log(JSON.stringify(manifest));
